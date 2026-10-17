// SPDX-License-Identifier: MPL-2.0

//! NVPool-backed access primitives for ext4 metadata home blocks.

use ostd::mm::io::util::HasVmReaderWriter;

use super::prelude::*;
use crate::fs::nv_pool::{
    capacity::PoolClass,
    data_page::DataPageState,
    frame::{MetaKey, NvPool, PinnedFrame},
    id::NvPageId,
};

pub(super) mod persistent {
    pub trait Sealed {}
}

pub(super) trait Persistent: Pod + persistent::Sealed {}

impl<T: Pod + persistent::Sealed> Persistent for T {}

/// A pinned metadata frame whose bytes remain stable for the lifetime of the
/// handle.
pub(super) struct MetaFrameRef(PinnedFrame);

impl MetaFrameRef {
    pub(super) fn read(&self, output: &mut [u8]) -> Result<()> {
        self.0.read(output)
    }
}

/// Per-mount bridge between ext4 home blocks and the global NVPool namespace.
pub(super) struct Ext4NvState {
    pool: Arc<NvPool>,
    block_device: Arc<dyn BlockDevice>,
    block_size: usize,
    namespace: [u8; 16],
    operations: Mutex<()>,
    pinned_homes: Mutex<BTreeSet<Ext4Bid>>,
    recovery_data_candidates: Mutex<BTreeSet<NvPageId>>,
}

impl Ext4NvState {
    pub(super) fn new(
        pool: Arc<NvPool>,
        block_device: Arc<dyn BlockDevice>,
        block_size: usize,
        namespace: [u8; 16],
    ) -> Result<Self> {
        if block_size != PAGE_SIZE {
            return_errno_with_message!(
                Errno::EINVAL,
                "NV metadata frames require page-sized ext4 blocks"
            );
        }
        let recovery_data_candidates = pool
            .committed_data_pages(namespace)
            .into_iter()
            .map(|(_, page)| page)
            .collect();
        Ok(Self {
            pool,
            block_device,
            block_size,
            namespace,
            operations: Mutex::new(()),
            pinned_homes: Mutex::new(BTreeSet::new()),
            recovery_data_candidates: Mutex::new(recovery_data_candidates),
        })
    }

    pub(super) fn read_meta_block(&self, home: Ext4Bid) -> Result<MetaFrameRef> {
        let _operation = self.operations.lock();
        let key = self.key(home);
        let id = if let Some(id) = self.pool.resident_meta_frame(key) {
            id
        } else {
            let content = self.read_ssd(home)?;
            self.commit_new(home, &content)?
        };
        Ok(MetaFrameRef(self.pool.pin_frame(id)?))
    }

    pub(super) fn update_meta_block(
        &self,
        home: Ext4Bid,
        update: impl FnOnce(&mut [u8]) -> Result<()>,
    ) -> Result<()> {
        let _operation = self.operations.lock();
        let mut content = if let Some(id) = self.pool.resident_meta_frame(self.key(home)) {
            let pin = self.pool.pin_frame(id)?;
            let mut content = vec![0; self.block_size];
            pin.read(&mut content)?;
            content
        } else {
            self.read_ssd(home)?
        };
        update(&mut content)?;
        self.commit_new(home, &content).map(|_| ())
    }

    pub(super) fn read_authoritative_block(
        &self,
        home: Ext4Bid,
        ssd_allocated: bool,
    ) -> Result<[u8; BLOCK_SIZE]> {
        let _operation = self.operations.lock();
        let mut content = [0; BLOCK_SIZE];
        if let Some(id) = self.pool.resident_meta_frame(self.key(home)) {
            self.pool.pin_frame(id)?.read(&mut content)?;
            return Ok(content);
        }
        if !ssd_allocated {
            return_errno_with_message!(Errno::ENOENT, "metadata object is not allocated");
        }
        let ssd = self.read_ssd(home)?;
        content.copy_from_slice(&ssd);
        Ok(content)
    }

    pub(super) fn read_meta_at<T: Persistent>(&self, absolute_offset: usize) -> Result<T> {
        let (home, slot) = self.location(absolute_offset, size_of::<T>())?;
        let frame = self.read_meta_block(home)?;
        let mut content = vec![0; self.block_size];
        frame.read(&mut content)?;
        Ok(T::from_bytes(&content[slot..slot + size_of::<T>()]))
    }

    pub(super) fn update_meta_at<T: Persistent>(
        &self,
        absolute_offset: usize,
        update: impl FnOnce(&mut T) -> Result<()>,
    ) -> Result<()> {
        let (home, slot) = self.location(absolute_offset, size_of::<T>())?;
        self.update_meta_block(home, |content| {
            let mut value = T::from_bytes(&content[slot..slot + size_of::<T>()]);
            update(&mut value)?;
            content[slot..slot + size_of::<T>()].copy_from_slice(value.as_bytes());
            Ok(())
        })
    }

    #[cfg_attr(
        not(ktest),
        expect(dead_code, reason = "used by later NV writeback task")
    )]
    pub(super) fn evict_meta_block(&self, home: Ext4Bid) -> Result<()> {
        if self.pinned_homes.lock().contains(&home) {
            return_errno_with_message!(Errno::EBUSY, "metadata root is policy-pinned");
        }
        let _operation = self.operations.lock();
        self.evict_locked(home)
    }

    pub(super) fn preflight_pinned_roots(
        &self,
        nr_groups: usize,
        descriptor_size: usize,
    ) -> Result<()> {
        let descriptor_bytes = nr_groups
            .checked_mul(descriptor_size)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let gdt_blocks = descriptor_bytes.div_ceil(self.block_size);
        let required = 1usize
            .checked_add(gdt_blocks)
            .and_then(|count| count.checked_add(nr_groups.checked_mul(2)?))
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        if required > self.pool.stats().total_pages {
            return_errno_with_message!(Errno::ENOSPC, "NVPool cannot hold pinned ext4 roots");
        }
        Ok(())
    }

    pub(super) fn pin_metadata_roots(
        &self,
        homes: impl IntoIterator<Item = Ext4Bid>,
    ) -> Result<()> {
        let homes: BTreeSet<_> = homes.into_iter().collect();
        if homes.len() > self.pool.stats().total_pages {
            return_errno_with_message!(Errno::ENOSPC, "NVPool cannot hold pinned ext4 roots");
        }
        for &home in &homes {
            let _ = self.read_meta_block(home)?;
        }
        self.pinned_homes.lock().extend(homes);
        Ok(())
    }

    pub(super) fn commit_transient_block(
        &self,
        home: Ext4Bid,
        content: &[u8; BLOCK_SIZE],
    ) -> Result<()> {
        let _operation = self.operations.lock();
        self.commit_new(home, content).map(|_| ())
    }

    pub(super) fn clear_transient_block(&self, home: Ext4Bid) -> Result<()> {
        let _operation = self.operations.lock();
        let Some(id) = self.pool.resident_meta_frame(self.key(home)) else {
            return Ok(());
        };
        self.pool.clear_frame(id, None)?;
        self.pool.free_frame(id)
    }

    pub(super) fn pending_extent_mutation_inodes(&self) -> Result<Vec<Ext4Ino>> {
        self.pool
            .resident_metadata(self.namespace)
            .into_iter()
            .filter_map(|(home, _)| {
                super::extent_mutation_anchor::is_extent_mutation_home(home).then_some(home)
            })
            .map(super::extent_mutation_anchor::ino_from_extent_mutation_home)
            .collect()
    }

    /// Writes every committed metadata home to SSD and clears its NV frame.
    ///
    /// This is an explicit clean-unmount operation. It is intentionally not
    /// part of ordinary `sync(2)`, which must retain NV-authoritative metadata.
    pub(super) fn drain_all_metadata(&self) -> Result<()> {
        let _operation = self.operations.lock();
        let homes = self.pool.resident_metadata(self.namespace);
        if homes.iter().any(|(home, _)| {
            *home == super::rename_anchor::RENAME_ANCHOR_HOME
                || super::extent_mutation_anchor::is_extent_mutation_home(*home)
        }) {
            return_errno_with_message!(Errno::EBUSY, "recovery anchor is still active");
        }
        for (home, _) in homes {
            self.evict_locked(home)?;
        }
        Ok(())
    }

    pub(super) fn reclaim_unallocated_residents(
        &self,
        is_allocated: impl Fn(Ext4Bid) -> bool,
    ) -> Result<usize> {
        let pinned = self.pinned_homes.lock().clone();
        let _operation = self.operations.lock();
        let homes = self.pool.resident_metadata(self.namespace);
        let mut reclaimed = 0;
        for (home, id) in homes {
            if home == super::rename_anchor::RENAME_ANCHOR_HOME
                || super::extent_mutation_anchor::is_extent_mutation_home(home)
                || pinned.contains(&home)
                || is_allocated(home)
            {
                continue;
            }
            self.pool.clear_frame(id, None)?;
            self.pool.free_frame(id)?;
            reclaimed += 1;
        }
        Ok(reclaimed)
    }

    pub(super) fn stage_data_segment(
        &self,
        target_ssd: Ext4Bid,
        segment: &BioSegment,
    ) -> Result<NvPageId> {
        if segment.nbytes() != BLOCK_SIZE || segment.offset_within_first_block() != 0 {
            return_errno_with_message!(Errno::EINVAL, "NV data write must cover one full page");
        }
        let mut content = [0; BLOCK_SIZE];
        segment
            .inner_dma_slice()
            .reader()
            .unwrap()
            .read(&mut VmWriter::from(&mut content[..]));
        self.stage_data_page(target_ssd, &content)
    }

    pub(super) fn stage_data_page(
        &self,
        target_ssd: Ext4Bid,
        content: &[u8; BLOCK_SIZE],
    ) -> Result<NvPageId> {
        let page = self.pool.stage_data_page(content)?;
        let result = (|| {
            self.pool
                .transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)?;
            self.pool
                .commit_namespaced_data_page(page, self.key(target_ssd), None)?;
            Ok(page)
        })();
        match result {
            Ok(page) => Ok(page),
            Err(error) => {
                let _ = self.pool.abort_data_page(page);
                Err(error)
            }
        }
    }

    pub(super) fn read_data_segment(
        &self,
        page: NvPageId,
        segment: BioSegment,
        complete_fn: BioCompleteFn,
    ) -> Result<()> {
        if segment.nbytes() != BLOCK_SIZE || segment.offset_within_first_block() != 0 {
            return_errno_with_message!(Errno::EINVAL, "NV data read must cover one full page");
        }
        self.touch_data_page(page);
        let pin = self.pool.pin_namespaced_data_page(page, self.namespace)?;
        let mut content = [0; BLOCK_SIZE];
        pin.read(&mut content)?;
        segment
            .inner_dma_slice()
            .writer()
            .unwrap()
            .write(&mut VmReader::from(&content[..]));
        complete_fn(BioStatus::Complete);
        Ok(())
    }

    pub(super) fn data_page_target(&self, page: NvPageId) -> Result<Ext4Bid> {
        self.touch_data_page(page);
        let key = self.pool.data_page_key(page)?;
        if key.namespace() != self.namespace {
            return_errno_with_message!(Errno::EINVAL, "NV data page belongs to another filesystem");
        }
        Ok(key.home_block())
    }

    pub(super) fn write_data_page_to_ssd(&self, page: NvPageId, target: Ext4Bid) -> Result<()> {
        let pin = self.pool.pin_namespaced_data_page(page, self.namespace)?;
        let mut content = [0; BLOCK_SIZE];
        pin.read(&mut content)?;
        self.block_device
            .write_bytes(self.offset(target)?, &content)
            .map_err(|_| Error::with_message(Errno::EIO, "failed to drain NV data page"))?;
        if self.block_device.sync()? != BioStatus::Complete {
            return_errno_with_message!(Errno::EIO, "failed to persist drained NV data page");
        }
        Ok(())
    }

    pub(super) fn release_data_page(&self, page: NvPageId) -> Result<()> {
        self.touch_data_page(page);
        self.pool.clear_data_page(page, None)?;
        self.pool.free_data_page(page)
    }

    pub(super) fn reclaim_orphan_data_pages(
        &self,
        referenced: &BTreeSet<NvPageId>,
    ) -> Result<usize> {
        let _operation = self.operations.lock();
        let mut candidates = self.recovery_data_candidates.lock();
        let mut reclaimed = 0;
        for page in core::mem::take(&mut *candidates) {
            if referenced.contains(&page) {
                continue;
            }
            self.pool.clear_data_page(page, None)?;
            self.pool.free_data_page(page)?;
            reclaimed += 1;
        }
        Ok(reclaimed)
    }

    fn touch_data_page(&self, page: NvPageId) {
        self.recovery_data_candidates.lock().remove(&page);
    }

    pub(super) fn resident_frame(
        &self,
        home: Ext4Bid,
    ) -> Option<crate::fs::nv_pool::id::NvFrameId> {
        self.pool.resident_meta_frame(self.key(home))
    }

    fn read_ssd(&self, home: Ext4Bid) -> Result<Vec<u8>> {
        let mut content = vec![0; self.block_size];
        self.block_device
            .read_bytes(self.offset(home)?, &mut content)
            .map_err(|_| Error::with_message(Errno::EIO, "failed to read metadata home block"))?;
        Ok(content)
    }

    fn commit_new(
        &self,
        home: Ext4Bid,
        content: &[u8],
    ) -> Result<crate::fs::nv_pool::id::NvFrameId> {
        let id = self.pool.alloc_frame(PoolClass::Metadata)?;
        let result = (|| {
            self.pool.pin_frame(id)?.write(content)?;
            let old = self
                .pool
                .replace_metadata_frame(id, self.key(home), u64::MAX, None)?;
            if let Some(old) = old {
                if self.pool.clear_frame(old, None).is_ok() {
                    let _ = self.pool.free_frame(old);
                }
            }
            Ok(id)
        })();
        if result.is_err() {
            let _ = self.pool.free_frame(id);
        }
        result
    }

    fn evict_locked(&self, home: Ext4Bid) -> Result<()> {
        let Some(id) = self.pool.resident_meta_frame(self.key(home)) else {
            return Ok(());
        };
        let eviction = self.pool.begin_frame_eviction(id)?;
        let mut content = vec![0; self.block_size];
        eviction.read(&mut content)?;
        self.block_device
            .write_bytes(self.offset(home)?, &content)
            .map_err(|_| Error::with_message(Errno::EIO, "failed to write metadata home block"))?;
        if self.block_device.sync().map_err(Error::from)? != BioStatus::Complete {
            return_errno_with_message!(Errno::EIO, "failed to flush metadata home block");
        }
        eviction.finish(None)?;
        self.pool.free_frame(id)
    }

    fn offset(&self, home: Ext4Bid) -> Result<usize> {
        home.checked_mul(self.block_size as Ext4Bid)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or_else(|| Error::with_message(Errno::EOVERFLOW, "metadata block offset overflow"))
    }

    fn key(&self, home: Ext4Bid) -> MetaKey {
        MetaKey::new(self.namespace, home)
    }

    fn location(&self, absolute_offset: usize, len: usize) -> Result<(Ext4Bid, usize)> {
        let home = Ext4Bid::try_from(absolute_offset / self.block_size)
            .map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let slot = absolute_offset % self.block_size;
        if slot
            .checked_add(len)
            .is_none_or(|end| end > self.block_size)
        {
            return_errno_with_message!(Errno::EINVAL, "metadata value crosses its home block");
        }
        Ok((home, slot))
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;
    use crate::fs::{
        fs_impls::ext4::test_utils::{Ext4FixtureBuilder, Ext4MemoryDisk},
        nv_pool::{
            config::{NvPoolConfig, NvSimMode},
            dram::DramNvBackend,
        },
        vfs::file_system::FileSystem,
    };

    fn pool() -> Arc<NvPool> {
        pool_with_pages(8)
    }

    fn pool_with_pages(pages: usize) -> Arc<NvPool> {
        let config = NvPoolConfig::from_bytes(
            Some(pages * PAGE_SIZE),
            Some(pages * PAGE_SIZE),
            Some(0),
            Some(NvSimMode::Fast),
        )
        .unwrap()
        .unwrap();
        let backend = Arc::new(DramNvBackend::allocate(config).unwrap());
        Arc::new(NvPool::format(backend, config, None).unwrap())
    }

    fn state(disk: Arc<Ext4MemoryDisk>) -> Ext4NvState {
        Ext4NvState::new(pool(), disk as Arc<dyn BlockDevice>, BLOCK_SIZE, [0; 16]).unwrap()
    }

    #[ktest]
    fn metadata_update_is_visible_from_nv_before_ssd_eviction() {
        let disk = Arc::new(Ext4MemoryDisk::new(16));
        let home = 2;
        let mut initial = [0x31; BLOCK_SIZE];
        initial[19] = 0x44;
        disk.segment()
            .write_bytes(Bid::new(home).to_offset(), &initial)
            .unwrap();
        let state = state(disk.clone());

        state
            .update_meta_block(home, |bytes| {
                bytes[19] = 0xa7;
                Ok(())
            })
            .unwrap();

        let mut ssd = [0; BLOCK_SIZE];
        disk.segment()
            .read_bytes(Bid::new(home).to_offset(), &mut ssd)
            .unwrap();
        assert_eq!(ssd[19], 0x44);
        let frame = state.read_meta_block(home).unwrap();
        let mut current = [0; BLOCK_SIZE];
        frame.read(&mut current).unwrap();
        assert_eq!(current[19], 0xa7);
    }

    #[ktest]
    fn normal_drain_writes_all_metadata_homes_and_clears_residency() {
        let disk = Arc::new(Ext4MemoryDisk::new(16));
        let state = state(disk.clone());
        for (home, value) in [(2, 0x52), (3, 0x73)] {
            state
                .update_meta_block(home, |content| {
                    content.fill(value);
                    Ok(())
                })
                .unwrap();
        }

        state.drain_all_metadata().unwrap();

        for (home, value) in [(2, 0x52), (3, 0x73)] {
            let mut ssd = [0; BLOCK_SIZE];
            disk.segment()
                .read_bytes(Bid::new(home).to_offset(), &mut ssd)
                .unwrap();
            assert_eq!(ssd, [value; BLOCK_SIZE]);
            assert!(state.resident_frame(home).is_none());
        }
    }

    #[ktest]
    fn filesystem_drain_converts_nv_metadata_to_ssd_state() {
        let fixture = Ext4FixtureBuilder::new(128, 128, 128)
            .with_nv_pool(pool_with_pages(16))
            .build()
            .unwrap();
        let home = 10;
        fixture
            .ext4
            .nv_state()
            .unwrap()
            .update_meta_block(home, |content| {
                content.fill(0x6d);
                Ok(())
            })
            .unwrap();

        FileSystem::drain(fixture.ext4.as_ref()).unwrap();

        let mut ssd = [0; BLOCK_SIZE];
        fixture
            .disk
            .segment()
            .read_bytes(Bid::new(home).to_offset(), &mut ssd)
            .unwrap();
        assert_eq!(ssd, [0x6d; BLOCK_SIZE]);
        assert!(
            fixture
                .ext4
                .nv_state()
                .unwrap()
                .resident_frame(home)
                .is_none()
        );
    }

    #[ktest]
    fn resident_of_unallocated_block_is_reclaimed_idempotently() {
        let disk = Arc::new(Ext4MemoryDisk::new(16));
        let state = state(disk);
        state
            .update_meta_block(7, |content| {
                content.fill(0x77);
                Ok(())
            })
            .unwrap();

        assert_eq!(state.reclaim_unallocated_residents(|_| false).unwrap(), 1);
        assert!(state.resident_frame(7).is_none());
        assert_eq!(state.reclaim_unallocated_residents(|_| false).unwrap(), 0);
    }

    #[ktest]
    fn metadata_eviction_requires_no_reader_and_reloads_from_ssd() {
        let disk = Arc::new(Ext4MemoryDisk::new(16));
        let home = 2;
        let state = state(disk.clone());
        state
            .update_meta_block(home, |bytes| {
                bytes[7] = 0x9b;
                Ok(())
            })
            .unwrap();

        let reader = state.read_meta_block(home).unwrap();
        assert_eq!(
            state.evict_meta_block(home).unwrap_err().error(),
            Errno::EBUSY
        );
        drop(reader);
        state.evict_meta_block(home).unwrap();

        let resident_before = state.resident_frame(home);
        assert!(resident_before.is_none());
        let reloaded = state.read_meta_block(home).unwrap();
        let mut content = [0; BLOCK_SIZE];
        reloaded.read(&mut content).unwrap();
        assert_eq!(content[7], 0x9b);
        assert!(state.resident_frame(home).is_some());
    }

    #[ktest]
    fn failed_ssd_eviction_preserves_nv_authority_for_retry() {
        let disk = Arc::new(Ext4MemoryDisk::new(16));
        let home = 2;
        let state = state(disk.clone());
        state
            .update_meta_block(home, |bytes| {
                bytes[3] = 0xe1;
                Ok(())
            })
            .unwrap();

        disk.set_fail_writes(true);
        assert_eq!(
            state.evict_meta_block(home).unwrap_err().error(),
            Errno::EIO
        );
        assert!(state.resident_frame(home).is_some());
        let frame = state.read_meta_block(home).unwrap();
        let mut current = [0; BLOCK_SIZE];
        frame.read(&mut current).unwrap();
        assert_eq!(current[3], 0xe1);
        drop(frame);

        disk.set_fail_writes(false);
        state.evict_meta_block(home).unwrap();
        assert!(state.resident_frame(home).is_none());
    }

    #[ktest]
    fn group_zero_allocation_sync_changes_nv_bitmap_not_ssd_bitmap() {
        let fixture = Ext4FixtureBuilder::new(128, 128, 128)
            .with_block_bitmap_metadata_marked()
            .with_nv_pool(pool())
            .build()
            .unwrap();
        let bitmap_home = 2;
        let mut before = [0; BLOCK_SIZE];
        fixture
            .disk
            .segment()
            .read_bytes(Bid::new(bitmap_home).to_offset(), &mut before)
            .unwrap();

        let allocated = fixture.ext4.alloc_blocks(1, 0).unwrap();
        assert_eq!(allocated.end, allocated.start + 1);
        fixture.ext4.sync_metadata().unwrap();

        let mut ssd = [0; BLOCK_SIZE];
        fixture
            .disk
            .segment()
            .read_bytes(Bid::new(bitmap_home).to_offset(), &mut ssd)
            .unwrap();
        assert_eq!(ssd, before);
        let state = fixture.ext4.nv_state().unwrap();
        let frame = state.read_meta_block(bitmap_home).unwrap();
        let mut nv = [0; BLOCK_SIZE];
        frame.read(&mut nv).unwrap();
        let bit = usize::try_from(allocated.start).unwrap();
        assert_ne!(nv[bit / 8] & (1 << (bit % 8)), 0);
    }

    #[ktest]
    fn inode_allocation_sync_changes_nv_bitmap_not_ssd_bitmap() {
        use crate::fs::file::InodeType;

        let fixture = Ext4FixtureBuilder::new(128, 128, 128)
            .with_inode_bitmap_metadata_marked()
            .with_nv_pool(pool())
            .build()
            .unwrap();
        let bitmap_home = 3;
        let mut before = [0; BLOCK_SIZE];
        fixture
            .disk
            .segment()
            .read_bytes(Bid::new(bitmap_home).to_offset(), &mut before)
            .unwrap();

        let inode = fixture
            .ext4
            .create_inode(
                super::super::fs::ROOT_INO,
                InodeType::File,
                super::super::inode::FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        fixture.ext4.sync_metadata().unwrap();

        let mut ssd = [0; BLOCK_SIZE];
        fixture
            .disk
            .segment()
            .read_bytes(Bid::new(bitmap_home).to_offset(), &mut ssd)
            .unwrap();
        assert_eq!(ssd, before);
        let frame = fixture
            .ext4
            .nv_state()
            .unwrap()
            .read_meta_block(bitmap_home)
            .unwrap();
        let mut nv = [0; BLOCK_SIZE];
        frame.read(&mut nv).unwrap();
        let bit = (inode.ino() - 1) as usize;
        assert_ne!(nv[bit / 8] & (1 << (bit % 8)), 0);
    }

    #[ktest]
    fn inode_table_update_is_committed_to_nv_before_ssd() {
        use crate::fs::file::InodeType;

        let fixture = Ext4FixtureBuilder::new(128, 128, 128)
            .with_inode_bitmap_metadata_marked()
            .with_nv_pool(pool())
            .build()
            .unwrap();
        let table_home = 4;
        let mut before = [0; BLOCK_SIZE];
        fixture
            .disk
            .segment()
            .read_bytes(Bid::new(table_home).to_offset(), &mut before)
            .unwrap();

        let inode = fixture
            .ext4
            .create_inode(
                super::super::fs::ROOT_INO,
                InodeType::File,
                super::super::inode::FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        assert_eq!(inode.ino(), 11);

        let mut ssd = [0; BLOCK_SIZE];
        fixture
            .disk
            .segment()
            .read_bytes(Bid::new(table_home).to_offset(), &mut ssd)
            .unwrap();
        assert_eq!(ssd, before);
        let frame = fixture
            .ext4
            .nv_state()
            .unwrap()
            .read_meta_block(table_home)
            .unwrap();
        let mut nv = [0; BLOCK_SIZE];
        frame.read(&mut nv).unwrap();
        let inode_slot = 10 * 256;
        assert_ne!(&nv[inode_slot..inode_slot + 2], &[0, 0]);
    }

    #[ktest]
    fn allocation_commits_group_descriptor_and_superblock_slots_to_nv() {
        let fixture = Ext4FixtureBuilder::new(128, 128, 128)
            .with_block_bitmap_metadata_marked()
            .with_nv_pool(pool())
            .build()
            .unwrap();
        let mut gdt_before = [0; BLOCK_SIZE];
        let mut super_before = [0; BLOCK_SIZE];
        fixture
            .disk
            .segment()
            .read_bytes(Bid::new(1).to_offset(), &mut gdt_before)
            .unwrap();
        fixture
            .disk
            .segment()
            .read_bytes(Bid::new(0).to_offset(), &mut super_before)
            .unwrap();

        fixture.ext4.alloc_blocks(1, 0).unwrap();
        fixture.ext4.sync_metadata().unwrap();

        let mut gdt_ssd = [0; BLOCK_SIZE];
        let mut super_ssd = [0; BLOCK_SIZE];
        fixture
            .disk
            .segment()
            .read_bytes(Bid::new(1).to_offset(), &mut gdt_ssd)
            .unwrap();
        fixture
            .disk
            .segment()
            .read_bytes(Bid::new(0).to_offset(), &mut super_ssd)
            .unwrap();
        assert_eq!(gdt_ssd, gdt_before);
        assert_eq!(super_ssd, super_before);

        let state = fixture.ext4.nv_state().unwrap();
        let mut gdt_nv = [0; BLOCK_SIZE];
        state.read_meta_block(1).unwrap().read(&mut gdt_nv).unwrap();
        assert_ne!(gdt_nv, gdt_before);
        let mut super_nv = [0; BLOCK_SIZE];
        state
            .read_meta_block(0)
            .unwrap()
            .read(&mut super_nv)
            .unwrap();
        assert_ne!(super_nv, super_before);
    }

    #[ktest]
    fn mount_pins_super_gdt_and_allocation_bitmaps() {
        let fixture = Ext4FixtureBuilder::new(128, 128, 128)
            .with_block_bitmap_metadata_marked()
            .with_inode_bitmap_metadata_marked()
            .with_nv_pool(pool())
            .build()
            .unwrap();
        let state = fixture.ext4.nv_state().unwrap();
        for home in [0, 1, 2, 3] {
            assert!(state.resident_frame(home).is_some());
            assert_eq!(
                state.evict_meta_block(home).unwrap_err().error(),
                Errno::EBUSY
            );
        }
    }

    #[ktest]
    fn mount_rejects_pool_too_small_for_pinned_roots() {
        let result = Ext4FixtureBuilder::new(128, 128, 128)
            .with_block_bitmap_metadata_marked()
            .with_inode_bitmap_metadata_marked()
            .with_nv_pool(pool_with_pages(3))
            .build();
        let err = match result {
            Ok(_) => panic!("undersized NVPool unexpectedly mounted"),
            Err(err) => err,
        };
        assert_eq!(err.error(), Errno::ENOSPC);
    }
}
