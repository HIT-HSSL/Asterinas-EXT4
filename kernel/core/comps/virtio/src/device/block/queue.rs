// SPDX-License-Identifier: MPL-2.0

use alloc::{sync::Arc, vec::Vec};

use aster_block::{
    bio::{BioStatus, BioType},
    request_queue::BioRequest,
};
use aster_util::mem_obj_slice::Slice;
use ostd::{
    mm::{PAGE_SIZE, VmIo, dma::DmaStream},
    sync::{LocalIrqDisabled, SpinLock, WaitQueue},
};

use super::{BlockFeatures, ReqType, RespStatus, flush::FlushGate};
use crate::{
    device::VirtioDeviceError, id_alloc::SyncIdAlloc, queue::VirtQueue, transport::VirtioTransport,
};

#[derive(Debug)]
struct SubmittedRequest {
    id: u16,
    bio_request: BioRequest,
}

#[derive(Debug)]
pub(super) struct CompletedRequest {
    pub(super) bio_request: BioRequest,
    pub(super) status: BioStatus,
}

impl SubmittedRequest {
    fn new(id: u16, bio_request: BioRequest) -> Self {
        Self { id, bio_request }
    }
}

#[derive(Debug)]
struct SubmittedTokenTable<T> {
    slots: Vec<Option<T>>,
}

impl<T> SubmittedTokenTable<T> {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            slots: core::iter::repeat_with(|| None).take(capacity).collect(),
        }
    }

    fn insert(&mut self, token: u16, value: T) -> Result<(), T> {
        let Some(slot) = self.slots.get_mut(usize::from(token)) else {
            return Err(value);
        };
        if slot.is_some() {
            return Err(value);
        }
        *slot = Some(value);
        Ok(())
    }

    fn take(&mut self, token: u16) -> Option<T> {
        self.slots.get_mut(usize::from(token))?.take()
    }
}

pub(super) fn validated_queue_size(
    advertised: u16,
    driver_max: u16,
) -> Result<u16, VirtioDeviceError> {
    let queue_size = advertised.min(driver_max);
    if queue_size < 2 || !queue_size.is_power_of_two() {
        return Err(VirtioDeviceError::InvalidQueueArgs);
    }
    Ok(queue_size)
}

pub(super) struct BlockQueue {
    index: u16,
    queue_size: u16,
    inner: SpinLock<BlockQueueInner, LocalIrqDisabled>,
    block_requests: Arc<DmaStream>,
    block_responses: Arc<DmaStream>,
    id_allocator: SyncIdAlloc,
    descriptor_wait_queue: WaitQueue,
    completion_wait_queue: WaitQueue,
    flush_gate: Arc<FlushGate>,
}

impl core::fmt::Debug for BlockQueue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BlockQueue")
            .field("index", &self.index)
            .field("queue_size", &self.queue_size)
            .field("inner", &*self.inner.lock())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct BlockQueueInner {
    virtqueue: VirtQueue,
    submitted_requests: SubmittedTokenTable<SubmittedRequest>,
}

impl BlockQueue {
    pub(super) fn new(
        index: u16,
        queue_size: u16,
        transport: &mut dyn VirtioTransport,
        flush_gate: Arc<FlushGate>,
    ) -> Result<Arc<Self>, VirtioDeviceError> {
        let virtqueue = VirtQueue::new(index, queue_size, transport)?;
        let request_frames = (usize::from(queue_size) * REQ_SIZE).div_ceil(PAGE_SIZE);
        let response_frames = (usize::from(queue_size) * RESP_SIZE).div_ceil(PAGE_SIZE);
        let block_requests = Arc::new(
            DmaStream::alloc(request_frames, false).map_err(VirtioDeviceError::ResourceAlloc)?,
        );
        let block_responses = Arc::new(
            DmaStream::alloc(response_frames, false).map_err(VirtioDeviceError::ResourceAlloc)?,
        );

        Ok(Arc::new(Self {
            index,
            queue_size,
            inner: SpinLock::new(BlockQueueInner {
                virtqueue,
                submitted_requests: SubmittedTokenTable::with_capacity(usize::from(queue_size)),
            }),
            block_requests,
            block_responses,
            id_allocator: SyncIdAlloc::with_capacity(usize::from(queue_size)),
            descriptor_wait_queue: WaitQueue::new(),
            completion_wait_queue: WaitQueue::new(),
            flush_gate,
        }))
    }

    pub(super) fn index(&self) -> u16 {
        self.index
    }

    pub(super) fn queue_size(&self) -> u16 {
        self.queue_size
    }

    pub(super) fn notify_completion_irq(&self) {
        self.completion_wait_queue.wake_one();
    }

    pub(super) fn drain_completions(&self, mut complete: impl FnMut(CompletedRequest)) {
        let first_request = self.completion_wait_queue.wait_until(|| self.pop_completion());
        complete(self.finish_completion(first_request));

        while let Some(request) = self.pop_completion() {
            complete(self.finish_completion(request));
        }
    }

    fn pop_completion(&self) -> Option<SubmittedRequest> {
        let request = {
            let mut inner = self.inner.lock();
            if !inner.virtqueue.can_pop() {
                return None;
            }

            let (token, _) = inner.virtqueue.pop_used_with_min_bytes(RESP_SIZE).ok()?;
            inner
                .submitted_requests
                .take(token)
                .expect("used virtio-blk token has no submitted request")
        };

        self.descriptor_wait_queue.wake_all();
        Some(request)
    }

    fn finish_completion(&self, complete_request: SubmittedRequest) -> CompletedRequest {
        let id = usize::from(complete_request.id);
        let resp_slice =
            Slice::new(&self.block_responses, id * RESP_SIZE..(id + 1) * RESP_SIZE);
        resp_slice.sync_from_device().unwrap();
        let resp: BlockResp = resp_slice.read_val(0).unwrap();
        self.id_allocator.dealloc(id);
        let status = if matches!(RespStatus::try_from(resp.status), Ok(RespStatus::Ok)) {
            BioStatus::Complete
        } else {
            BioStatus::IoError
        };

        if status == BioStatus::Complete
            && let BioType::Read = complete_request.bio_request.type_()
        {
            complete_request
                .bio_request
                .bios()
                .flat_map(|bio| {
                    bio.segments()
                        .iter()
                        .map(|segment| segment.inner_dma_slice())
                })
                .for_each(|dma_slice| dma_slice.sync_from_device().unwrap());
        }

        CompletedRequest {
            bio_request: complete_request.bio_request,
            status,
        }
    }

    pub(super) fn submit(&self, bio_request: BioRequest, features: BlockFeatures) {
        match bio_request.type_() {
            BioType::Read => self.read(bio_request),
            BioType::Write => self.write(bio_request),
            BioType::Flush => self.flush(bio_request, features.contains(BlockFeatures::FLUSH)),
        }
    }

    fn read(&self, bio_request: BioRequest) {
        let id = self.id_allocator.alloc();
        let req_slice = self.request_slice(
            id,
            BlockReq {
                type_: ReqType::In as _,
                reserved: 0,
                sector: bio_request.sid_range().start.to_raw(),
            },
        );
        let resp_slice = self.response_slice(id);
        let num_used_descs = bio_request.num_segments() + 2;

        self.add_request(id, bio_request, num_used_descs, move |queue, request| {
            let mut outputs: Vec<&Slice<_>> = Vec::with_capacity(request.num_segments() + 1);
            outputs.extend(request.bios().flat_map(|bio| {
                bio.segments()
                    .iter()
                    .map(|segment| segment.inner_dma_slice())
            }));
            outputs.push(&resp_slice);
            queue
                .add_dma_bufs(&[&req_slice], outputs.as_slice())
                .expect("add queue failed")
        });
    }

    fn write(&self, bio_request: BioRequest) {
        let id = self.id_allocator.alloc();
        let req_slice = self.request_slice(
            id,
            BlockReq {
                type_: ReqType::Out as _,
                reserved: 0,
                sector: bio_request.sid_range().start.to_raw(),
            },
        );
        let resp_slice = self.response_slice(id);
        let num_used_descs = bio_request.num_segments() + 2;

        self.add_request(id, bio_request, num_used_descs, move |queue, request| {
            let mut inputs: Vec<&Slice<_>> = Vec::with_capacity(request.num_segments() + 1);
            inputs.push(&req_slice);
            for dma_slice in request.bios().flat_map(|bio| {
                bio.segments()
                    .iter()
                    .map(|segment| segment.inner_dma_slice())
            }) {
                dma_slice.sync_to_device().unwrap();
                inputs.push(dma_slice);
            }
            queue
                .add_dma_bufs(inputs.as_slice(), &[&resp_slice])
                .expect("add queue failed")
        });
    }

    fn flush(&self, bio_request: BioRequest, flush_supported: bool) {
        if !flush_supported {
            self.flush_gate.complete_flush();
            bio_request.into_bios().for_each(|bio| {
                bio.complete(BioStatus::Complete);
            });
            return;
        }

        let id = self.id_allocator.alloc();
        let req_slice = self.request_slice(
            id,
            BlockReq {
                type_: ReqType::Flush as _,
                reserved: 0,
                sector: bio_request.sid_range().start.to_raw(),
            },
        );
        let resp_slice = self.response_slice(id);
        self.add_request(id, bio_request, 2, move |queue, _| {
            queue
                .add_dma_bufs(&[&req_slice], &[&resp_slice])
                .expect("add queue failed")
        });
    }

    fn request_slice(&self, id: usize, request: BlockReq) -> Slice<Arc<DmaStream>> {
        let slice = Slice::new(
            self.block_requests.clone(),
            id * REQ_SIZE..(id + 1) * REQ_SIZE,
        );
        slice.write_val(0, &request).unwrap();
        slice.sync_to_device().unwrap();
        slice
    }

    fn response_slice(&self, id: usize) -> Slice<Arc<DmaStream>> {
        let slice = Slice::new(
            self.block_responses.clone(),
            id * RESP_SIZE..(id + 1) * RESP_SIZE,
        );
        slice.write_val(0, &BlockResp::default()).unwrap();
        slice.sync_to_device().unwrap();
        slice
    }

    fn add_request<F>(
        &self,
        id: usize,
        bio_request: BioRequest,
        num_used_descs: usize,
        add_dma_bufs: F,
    ) where
        F: FnOnce(&mut VirtQueue, &BioRequest) -> u16,
    {
        if num_used_descs > usize::from(self.queue_size) {
            panic!("The request size surpasses the queue size");
        }

        let mut add_dma_bufs = Some(add_dma_bufs);
        let mut bio_request = Some(bio_request);
        self.descriptor_wait_queue.wait_until(|| {
            let mut inner = self.inner.lock();
            if num_used_descs > inner.virtqueue.available_desc() {
                return None;
            }
            let token = add_dma_bufs.take().unwrap()(
                &mut inner.virtqueue,
                bio_request.as_ref().unwrap(),
            );
            inner
                .submitted_requests
                .insert(
                    token,
                    SubmittedRequest::new(id as u16, bio_request.take().unwrap()),
                )
                .unwrap_or_else(|_| panic!("duplicate or invalid virtio-blk token {token}"));
            if inner.virtqueue.should_notify() {
                inner.virtqueue.notify();
            }
            Some(())
        });
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
struct BlockReq {
    type_: u32,
    reserved: u32,
    sector: u64,
}

const REQ_SIZE: usize = size_of::<BlockReq>();

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
struct BlockResp {
    status: u8,
}

const RESP_SIZE: usize = size_of::<BlockResp>();

impl Default for BlockResp {
    fn default() -> Self {
        Self {
            status: RespStatus::_NotReady as _,
        }
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::{SubmittedTokenTable, validated_queue_size};

    #[ktest]
    fn queue_size_is_capped_and_validated() {
        assert_eq!(validated_queue_size(1024, 1024).unwrap(), 1024);
        assert_eq!(validated_queue_size(256, 1024).unwrap(), 256);
        assert!(validated_queue_size(0, 1024).is_err());
        assert!(validated_queue_size(768, 1024).is_err());
    }

    #[ktest]
    fn token_table_tracks_each_queue_slot_once() {
        let mut table = SubmittedTokenTable::with_capacity(2);

        assert!(table.insert(0, 10).is_ok());
        assert!(table.insert(0, 11).is_err());
        assert_eq!(table.take(0), Some(10));
        assert_eq!(table.take(0), None);
        assert!(table.insert(2, 12).is_err());
    }
}
