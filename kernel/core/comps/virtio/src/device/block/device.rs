// SPDX-License-Identifier: MPL-2.0

use alloc::{
    boxed::Box,
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    fmt::Debug,
    sync::atomic::{AtomicU32, Ordering},
};

use aster_block::{
    BlockDeviceMeta, EXTENDED_DEVICE_ID_ALLOCATOR, PartitionInfo, PartitionNode,
    bio::{BioEnqueueError, BioType, SubmittedBio, bio_segment_pool_init},
    request_queue::{BioRequest, BioRequestSingleQueue},
};
use device_id::{DeviceId, MinorId};
use ostd::{arch::trap::TrapFrame, debug, info, sync::SpinLock};

use super::{
    BlockFeatures, VirtioBlockConfig,
    flush::FlushGate,
    mq::QueueRouter,
    queue::{BlockQueue, validated_queue_size},
};
use crate::{
    VIRTIO_BLOCK_MAJOR_ID,
    device::VirtioDeviceError,
    transport::{ConfigManager, DeviceTransport},
};

/// The number of minor device numbers allocated for each virtio disk,
/// including the whole disk and its partitions. If a disk has more than
/// 16 partitions, then allocate a device ID via `EXTENDED_DEVICE_ID_ALLOCATOR`.
const VIRTIO_DEVICE_MINORS: u32 = 16;

/// The number of virtio block devices, used to assign minor device numbers.
static NR_BLOCK_DEVICE: AtomicU32 = AtomicU32::new(0);

#[derive(Debug)]
pub struct BlockDevice {
    device: Arc<DeviceInner>,
    request_queues: Box<[BioRequestSingleQueue]>,
    queue_router: QueueRouter,
    id: DeviceId,
    name: String,
    partitions: SpinLock<Option<Vec<Arc<PartitionNode>>>>,
    weak_self: Weak<Self>,
}

impl BlockDevice {
    /// Returns the formatted device name.
    ///
    /// The device name starts at "vda". The 26th device is "vdz" and the 27th is "vdaa".
    /// The last one for two lettered suffix is "vdzz" which is followed by "vdaaa".
    fn formatted_device_name(mut index: u32) -> String {
        const VIRTIO_DISK_PREFIX: &str = "vd";

        let mut suffix = Vec::new();
        loop {
            suffix.push((b'a' + (index % 26) as u8) as char);
            index /= 26;
            if index == 0 {
                break;
            }
            index -= 1;
        }
        suffix.reverse();
        let mut name = String::from(VIRTIO_DISK_PREFIX);
        name.extend(suffix);
        name
    }

    /// Creates a new VirtIO-Block driver and registers it.
    pub(crate) fn init(device_transport: DeviceTransport) -> Result<(), VirtioDeviceError> {
        let device = DeviceInner::init(device_transport)?;

        let index = NR_BLOCK_DEVICE.fetch_add(1, Ordering::Relaxed);
        let id = DeviceId::new(
            VIRTIO_BLOCK_MAJOR_ID.get().unwrap().get(),
            MinorId::new(index * VIRTIO_DEVICE_MINORS),
        );
        let name = Self::formatted_device_name(index);
        let max_nr_segments_per_bio = usize::from(
            device
                .queues
                .iter()
                .map(|queue| queue.queue_size())
                .min()
                .unwrap()
                - 2,
        );

        let queue_count = device.queue_count();
        let request_queues = (0..queue_count)
            .map(|_| BioRequestSingleQueue::with_max_nr_segments_per_bio(max_nr_segments_per_bio))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let block_device = Arc::new_cyclic(|weak_self| BlockDevice {
            device,
            request_queues,
            queue_router: QueueRouter::new(queue_count),
            id,
            name,
            partitions: SpinLock::new(None),
            weak_self: weak_self.clone(),
        });

        aster_block::register(block_device).unwrap();

        bio_segment_pool_init();
        Ok(())
    }

    /// Dequeues a `BioRequest` from the software staging queue and
    /// processes the request.
    pub fn handle_requests(&self, queue_index: usize) {
        let request = self.request_queues[queue_index].dequeue();
        self.device.submit(queue_index, request)
    }

    /// Completes requests from one hardware virtqueue in task context.
    pub fn handle_completions(&self, queue_index: usize) {
        self.device
            .queue(queue_index)
            .drain_completions(|completion| {
                let request_type = completion.bio_request.type_();
                let completed_bios = completion.bio_request.bios().count();
                match request_type {
                    BioType::Write => self.device.flush_gate.complete_writes(completed_bios),
                    BioType::Flush => self.device.flush_gate.complete_flush(),
                    BioType::Read => {}
                }
                completion
                    .bio_request
                    .into_bios()
                    .for_each(|bio| bio.complete(completion.status));
            });
    }

    /// Negotiate features for the device specified bits 0~23
    pub fn queue_count(&self) -> usize {
        self.device.queue_count()
    }

    pub(crate) fn negotiate_features(device_features: u64) -> u64 {
        BlockFeatures::negotiated_with_device(device_features).bits()
    }
}

impl aster_block::BlockDevice for BlockDevice {
    fn enqueue(&self, bio: SubmittedBio) -> Result<(), BioEnqueueError> {
        let bio_type = bio.type_();
        let queue_index = match bio_type {
            BioType::Flush => {
                self.device.flush_gate.begin_flush();
                0
            }
            BioType::Write => {
                self.device.flush_gate.admit_write();
                self.queue_router.select()
            }
            BioType::Read => self.queue_router.select(),
        };
        let result = self.request_queues[queue_index].enqueue(bio);
        if result.is_err() {
            match bio_type {
                BioType::Write => self.device.flush_gate.complete_writes(1),
                BioType::Flush => self.device.flush_gate.complete_flush(),
                BioType::Read => {}
            }
        }
        result
    }

    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            max_nr_segments_per_bio: self.request_queues[0].max_nr_segments_per_bio(),
            nr_sectors: self.device.config_manager.capacity_sectors(),
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn id(&self) -> DeviceId {
        self.id
    }

    fn set_partitions(&self, infos: Vec<Option<PartitionInfo>>) {
        let mut partitions = self.partitions.lock();
        if let Some(old_partitions) = partitions.take() {
            for partition in old_partitions {
                let _ = aster_block::unregister(partition.id());
            }
        }

        let mut new_partitions = Vec::new();
        for (index, info_opt) in infos.iter().enumerate() {
            let Some(info) = info_opt else {
                continue;
            };

            let index = index as u32 + 1;
            let id = if index < VIRTIO_DEVICE_MINORS {
                DeviceId::new(self.id.major(), MinorId::new(self.id.minor().get() + index))
            } else {
                EXTENDED_DEVICE_ID_ALLOCATOR.get().unwrap().allocate()
            };
            let name = format!("{}{}", self.name(), index);
            let device = self.weak_self.upgrade().unwrap();

            let partition = Arc::new(PartitionNode::new(id, name, device, *info));
            new_partitions.push(partition);
        }

        for partition in new_partitions.iter() {
            let _ = aster_block::register(partition.clone());
        }

        *partitions = Some(new_partitions);
    }

    fn partitions(&self) -> Option<Vec<Arc<dyn aster_block::BlockDevice>>> {
        let partitions = self.partitions.lock();
        let devices = partitions
            .as_ref()?
            .iter()
            .map(|p| p.clone() as Arc<dyn aster_block::BlockDevice>)
            .collect();
        Some(devices)
    }
}

#[derive(Debug)]
struct DeviceInner {
    config_manager: ConfigManager<VirtioBlockConfig>,
    features: BlockFeatures,
    queues: Box<[Arc<BlockQueue>]>,
    transport: SpinLock<DeviceTransport>,
    flush_gate: Arc<FlushGate>,
}

impl DeviceInner {
    const MAX_QUEUE_SIZE: u16 = 1024;

    /// Creates and inits the device.
    fn init(mut device_transport: DeviceTransport) -> Result<Arc<Self>, VirtioDeviceError> {
        let config_manager = VirtioBlockConfig::new_manager(device_transport.as_ref());

        let config = config_manager.read_config();
        debug!("virio_blk_config = {:?}", config);

        let device_features = device_transport.read_device_features();
        let mq_offered =
            BlockFeatures::from_bits_truncate(device_features).contains(BlockFeatures::MQ);
        let features = BlockFeatures::negotiated_with_device(device_features);
        let mq_negotiated = features.contains(BlockFeatures::MQ);

        let block_size = if features.contains(BlockFeatures::BLK_SIZE) {
            config_manager.block_size()
        } else {
            // Fall back to standard sector size SECTOR_SIZE (512 bytes)
            // Reference:
            // <https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html#x1-2790004>
            VirtioBlockConfig::sector_size()
        };
        if block_size != VirtioBlockConfig::sector_size() {
            ostd::error!("block size {} is not supported yet", block_size);
            return Err(VirtioDeviceError::UnsupportedConfig);
        }

        let configured_queue_count = if mq_negotiated {
            config_manager.num_queues()
        } else {
            1
        };
        let transport_queue_count = device_transport.num_queues();
        let cpu_count = ostd::cpu::num_cpus();
        let effective_queue_count = super::mq::effective_queue_count(
            mq_negotiated,
            configured_queue_count,
            transport_queue_count,
            cpu_count,
        )?;
        info!(
            "virtio-blk queues: mq_offered={}, mq_negotiated={}, configured={}, transport={}, cpus={}, effective={}",
            mq_offered,
            mq_negotiated,
            configured_queue_count,
            transport_queue_count,
            cpu_count,
            effective_queue_count
        );
        let flush_gate = Arc::new(FlushGate::new());
        let mut queues = Vec::with_capacity(usize::from(effective_queue_count));
        for index in 0..effective_queue_count {
            let advertised = device_transport.max_queue_size(index)?;
            let queue_size = validated_queue_size(advertised, Self::MAX_QUEUE_SIZE)?;
            queues.push(BlockQueue::new(
                index,
                queue_size,
                device_transport.as_mut(),
                flush_gate.clone(),
            )?);
        }
        let queue_sizes: Vec<u16> = queues.iter().map(|queue| queue.queue_size()).collect();
        info!("virtio-blk initialized queue sizes: {:?}", queue_sizes);

        let device = Arc::new(Self {
            config_manager,
            features,
            queues: queues.into_boxed_slice(),
            transport: SpinLock::new(device_transport),
            flush_gate,
        });

        let cloned_device = device.clone();
        let handle_config_change = move |_: &TrapFrame| {
            cloned_device.handle_config_change();
        };

        {
            let mut transport = device.transport.lock();
            transport.register_cfg_callback(Box::new(handle_config_change))?;
            for queue in device.queues.iter() {
                let irq_queue = queue.clone();
                let handle_irq = move |_: &TrapFrame| {
                    irq_queue.notify_completion_irq();
                };
                transport.register_queue_callback(queue.index(), Box::new(handle_irq), true)?;
            }
            transport.finish_init();
        }

        Ok(device)
    }

    fn handle_config_change(&self) {
        info!("block device config space change");
    }

    fn queue_count(&self) -> usize {
        self.queues.len()
    }

    fn queue(&self, index: usize) -> &Arc<BlockQueue> {
        &self.queues[index]
    }

    fn submit(&self, queue_index: usize, bio_request: BioRequest) {
        self.queue(queue_index).submit(bio_request, self.features);
    }
}
