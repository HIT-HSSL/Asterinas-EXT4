// SPDX-License-Identifier: MPL-2.0

//! Ext4 extent block-mapping engine.
//!
//! It maps a file's logical blocks to physical device blocks by walking an
//! on-disk extent tree rooted inline in the inode's `i_block`. `map_blocks`
//! is the single translation entry point; the `PageCache` backend drives file
//! I/O through it.
//!
//! Interior tree nodes are read directly from the device.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ostd::mm::io::util::HasVmReaderWriter;

use super::{
    BufferedWriteReservation, MapState, Mapping,
    extent_status::{ExtentStatus, ExtentStatusEntry, ExtentStatusTree},
};
use crate::{
    fs::{
        fs_impls::ext4::{
            fs::Ext4,
            inode::{Inode, RAW_BLOCK_PTRS_LEN},
            prelude::*,
            writeback::Ext4PageWritebackObserver,
        },
        nv_pool::id::NvPageId,
    },
    vm::page_cache::{
        PageCacheWritebackObserver, WritebackPostIo, submit_writeback_run_with_post_io_job,
    },
};

pub(in crate::fs::fs_impls::ext4) mod node;
mod tree;

pub(in crate::fs::fs_impls::ext4) fn recover_pending_extent_mutation(
    fs: &Ext4,
    mutation: &crate::fs::fs_impls::ext4::extent_mutation_anchor::PendingExtentMutation,
) -> Result<()> {
    tree::ExtentTree::recover_pending_mutation(fs, mutation)
}

/// 512-byte sectors per filesystem block; the unit `i_blocks` is counted in.
const SECTORS_PER_BLOCK: u64 = (BLOCK_SIZE / SECTOR_SIZE) as u64;

/// Mutable extent-tree state for one inode, protected by a single lock.
pub(super) struct ExtentTreeState {
    /// Validated tree whose root is stored in the inode's inline `i_block`.
    tree: tree::ExtentTree,
    /// Inode `i_blocks`, including data and extent metadata, in 512-byte sectors.
    sector_count: u64,
    /// Whether the tree or sector count has changed since the last writeback.
    dirty: bool,
    /// Blocks dereferenced by the new root but not reusable until that root commits.
    pending_frees: Vec<Range<Ext4Bid>>,
    /// NV pages dereferenced by the new root and released after root commit.
    pending_nv_frees: Vec<NvPageId>,
}

/// Maps an inode's logical blocks to physical blocks via its extent tree and
/// owns the tree and `i_blocks` accounting.
pub(in super::super) struct ExtentManager {
    /// The mutable extent-tree state.
    state: Arc<RwMutex<ExtentTreeState>>,
    /// Volatile logical-range cache and delayed-allocation state.
    extent_status: Arc<ExtentStatusTree>,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    disk_lookups: AtomicU64,
    /// Cached page count for the `PageCache` backend.
    npages: AtomicUsize,
    /// Back-reference to the filesystem, for the block device and allocator.
    fs: Weak<Ext4>,
    /// Directory contents are filesystem metadata and use NV metadata frames.
    metadata_data: AtomicBool,
    writeback_observer: Arc<Ext4PageWritebackObserver>,
}

impl ExtentManager {
    pub(super) fn new(
        ino: Ext4Ino,
        root: [u32; RAW_BLOCK_PTRS_LEN],
        sector_count: u64,
        fs: Weak<Ext4>,
        npages: usize,
    ) -> Result<Self> {
        let writeback_observer = Ext4PageWritebackObserver::new(fs.clone(), ino);
        let mut tree = tree::ExtentTree::try_from_root(root)?;
        tree.bind_owner(ino);
        tree.bind_allocation_observer(writeback_observer.clone());
        Ok(Self {
            state: Arc::new(RwMutex::new(ExtentTreeState {
                tree,
                sector_count,
                dirty: false,
                pending_frees: Vec::new(),
                pending_nv_frees: Vec::new(),
            })),
            extent_status: Arc::new(ExtentStatusTree::new()),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            disk_lookups: AtomicU64::new(0),
            npages: AtomicUsize::new(npages),
            fs,
            metadata_data: AtomicBool::new(false),
            writeback_observer,
        })
    }

    pub(super) fn bind_writeback_target(&self, inode: Weak<Inode>) {
        self.metadata_data.store(
            inode
                .upgrade()
                .is_some_and(|inode| inode.type_ == InodeType::Dir),
            Ordering::Release,
        );
        self.writeback_observer.bind_inode(inode);
    }

    /// Returns a strong reference to the owning filesystem.
    fn fs(&self) -> Result<Arc<Ext4>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem dropped"))
    }

    pub(super) fn uses_delalloc(&self) -> Result<bool> {
        Ok(self.fs()?.uses_delalloc())
    }

    pub(super) fn reserve_delayed(
        &self,
        start_iblock: Iblock,
        end_iblock: Iblock,
    ) -> Result<BufferedWriteReservation> {
        if start_iblock >= end_iblock {
            return Ok(BufferedWriteReservation::default());
        }

        let fs = self.fs()?;
        const MAX_RESERVATION_RETRIES: usize = 8;
        for _ in 0..MAX_RESERVATION_RETRIES {
            let mut cursor = start_iblock;
            while cursor < end_iblock {
                let mapping = self.map_blocks(cursor)?;
                cursor = cursor.saturating_add(mapping.len()).min(end_iblock);
            }

            let (version, delayed_ranges) = self
                .extent_status
                .known_holes_snapshot(start_iblock..end_iblock);
            let blocks: Iblock = delayed_ranges
                .iter()
                .map(|range| range.end - range.start)
                .sum();
            if blocks == 0 {
                return Ok(BufferedWriteReservation::default());
            }
            fs.reserve_data_blocks(blocks)?;
            if self
                .extent_status
                .mark_holes_delayed_if_version(version, &delayed_ranges)
            {
                return Ok(BufferedWriteReservation { delayed_ranges });
            }
            fs.release_reserved_data_blocks(blocks);
        }
        return_errno_with_message!(
            Errno::EAGAIN,
            "extent status kept changing during delayed-allocation reservation"
        );
    }

    pub(super) fn cancel_delayed(&self, reservation: BufferedWriteReservation) {
        if reservation.delayed_ranges.is_empty() {
            return;
        }
        let Ok(fs) = self.fs() else {
            return;
        };
        let mut released = 0;
        for range in reservation.delayed_ranges {
            released += range.end - range.start;
            self.extent_status.insert(range, ExtentStatus::Hole);
        }
        fs.release_reserved_data_blocks(released);
    }

    /// Maps logical block `iblock` to a physical run.
    ///
    /// The returned length spans from `iblock` to the end of the covering
    /// extent, so callers can batch contiguous reads.
    pub(super) fn map_blocks(&self, iblock: Iblock) -> Result<Mapping> {
        loop {
            if let Some(entry) = self.extent_status.lookup(iblock) {
                self.cache_hits.fetch_add(1, Ordering::Relaxed);
                return Ok(Self::mapping_from_status(entry, iblock));
            }

            self.cache_misses.fetch_add(1, Ordering::Relaxed);
            let observed_version = self.extent_status.version();
            let fs = self.fs()?;
            let device = fs.as_ref();
            let disk_status = {
                let state = self.state.read();
                self.disk_lookups.fetch_add(1, Ordering::Relaxed);
                match state.tree.find_or_hole(device, iblock)? {
                    (Some(extent), range) => {
                        let status = if extent.is_unwritten() {
                            ExtentStatus::Unwritten {
                                pblock: extent.start(),
                            }
                        } else {
                            ExtentStatus::Written {
                                pblock: extent.start(),
                            }
                        };
                        (range, status)
                    }
                    (None, range) => (range, ExtentStatus::Hole),
                }
            };

            if self
                .extent_status
                .insert_if_version(observed_version, disk_status.0, disk_status.1)
            {
                let entry = self
                    .extent_status
                    .lookup(iblock)
                    .expect("the just-cached mapping must cover the queried block");
                return Ok(Self::mapping_from_status(entry, iblock));
            }
        }
    }

    fn mapping_from_status(entry: ExtentStatusEntry, iblock: Iblock) -> Mapping {
        let len = entry.range().end - iblock;
        match entry.status_at(iblock) {
            ExtentStatus::Written { pblock } => Mapping::Mapped {
                pblock,
                len,
                state: MapState::Written,
            },
            ExtentStatus::Unwritten { pblock } => Mapping::Mapped {
                pblock,
                len,
                state: MapState::Unwritten,
            },
            ExtentStatus::Delayed => Mapping::Delayed { len },
            ExtentStatus::Hole => Mapping::Hole { len },
        }
    }

    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn shrink_extent_cache(&self, limit: usize) -> usize {
        self.extent_status.shrink(limit)
    }

    #[cfg(ktest)]
    fn cache_stats(&self) -> (u64, u64, u64) {
        (
            self.cache_hits.load(Ordering::Relaxed),
            self.cache_misses.load(Ordering::Relaxed),
            self.disk_lookups.load(Ordering::Relaxed),
        )
    }

    /// Returns the inode's `i_blocks` (512-byte sectors) accounting.
    pub(super) fn sector_count(&self) -> u64 {
        self.state.read().sector_count
    }

    /// Returns a copy of the inode's 60-byte `i_block` (extent-tree root).
    pub(super) fn root_snapshot(&self) -> [u32; RAW_BLOCK_PTRS_LEN] {
        self.state.read().tree.root()
    }

    /// Returns whether the tree or `i_blocks` has changed since last writeback.
    pub(super) fn is_dirty(&self) -> bool {
        self.state.read().dirty
    }

    /// Clears the dirty flag after a successful inode writeback.
    pub(super) fn clear_dirty(&self) {
        self.state.write().dirty = false;
    }

    /// Updates the cached page-cache capacity bound.
    pub(super) fn set_npages(&self, npages: usize) {
        self.npages.store(npages, Ordering::Release);
    }

    /// Allocates data blocks for every hole in `[start_iblock, end_iblock)`,
    /// converts any unwritten (preallocated) extent overlapping the range to
    /// written, and records both in the extent tree.
    ///
    /// Planning is done from a single snapshot of the current tree: existing
    /// written extents are left untouched (overwrites reuse the mapped block),
    /// unwritten extents in range are flipped to written (so the data the caller
    /// is about to write becomes readable), and blocks are allocated only where
    /// the snapshot showed a true hole. `i_blocks` is grown by every data block
    /// allocated plus the net extent-tree metadata blocks; the conversion adds
    /// no data sectors (the blocks were already counted at allocation time).
    ///
    /// On an allocation error mid-way the partial allocation stays in `state`;
    /// the caller's `rollback_write` truncates it away. A successful conversion
    /// that precedes a failed page-cache write also stays (the blocks were
    /// already allocated, so nothing leaks): leaving them written is benign.
    pub(super) fn ensure_allocated(&self, start_iblock: Iblock, end_iblock: Iblock) -> Result<()> {
        if start_iblock >= end_iblock {
            return Ok(());
        }
        let fs = self.fs()?;
        let device = fs.as_ref();
        let mut s = self.state.write();
        self.extent_status.invalidate(start_iblock..end_iblock);

        // Flip any unwritten extent that overlaps the write range to written so
        // the blocks `submit_write_bio` fills read back the real data. The
        // physical mapping is preserved; only metadata blocks (a split may grow
        // the tree) move, so `i_blocks` changes by the net metadata delta only.
        let mut cursor = start_iblock;
        let mut has_unwritten = false;
        while cursor < end_iblock {
            let (extent, known_range) = s.tree.find_or_hole(device, cursor)?;
            if let Some(extent) = extent
                && extent.is_unwritten()
            {
                has_unwritten = true;
                break;
            }
            cursor = end_iblock.min(known_range.end);
        }
        if has_unwritten {
            let delta = s
                .tree
                .convert_unwritten(&fs, start_iblock, end_iblock - start_iblock)?;
            touch_tree_delta(&self.writeback_observer, &fs, &delta);
            let net_meta = delta.meta_allocated as i64 - delta.meta_freed as i64;
            s.pending_frees
                .extend(delta.deferred_frees.into_iter().map(|bid| bid..bid + 1));
            s.sector_count =
                (s.sector_count as i64 + net_meta * SECTORS_PER_BLOCK as i64).max(0) as u64;
            s.dirty = true;
        }

        Self::allocate_holes(
            &fs,
            &self.writeback_observer,
            &mut s,
            None,
            start_iblock,
            end_iblock,
            node::ExtentKind::Written,
            false,
        )
    }

    /// Allocates blocks for every hole in `[start_iblock, end_iblock)` and
    /// records them as *unwritten* extents, so they read as zeros until
    /// written. Backs `fallocate`; file size is the caller's business.
    ///
    /// Existing extents (written or unwritten) in the range are left as they
    /// are. On a mid-way allocation error the partial allocation stays in
    /// `state`; the caller's `rollback_fallocate` truncates it away.
    pub(super) fn preallocate(&self, start_iblock: Iblock, end_iblock: Iblock) -> Result<()> {
        if start_iblock >= end_iblock {
            return Ok(());
        }
        let fs = self.fs()?;
        let mut s = self.state.write();
        self.extent_status.invalidate(start_iblock..end_iblock);
        Self::allocate_holes(
            &fs,
            &self.writeback_observer,
            &mut s,
            None,
            start_iblock,
            end_iblock,
            node::ExtentKind::Unwritten,
            false,
        )
    }

    pub(super) fn materialize_delayed(
        &self,
        start_iblock: Iblock,
        end_iblock: Iblock,
    ) -> Result<()> {
        let mut cursor = start_iblock;
        while let Some(range) = self.extent_status.next_delayed_range(cursor..end_iblock) {
            cursor = range.end;
            self.allocate_delayed(range.start, range.end)?;
        }
        Ok(())
    }

    /// Fills every hole in `[start_iblock, end_iblock)` with freshly allocated
    /// blocks recorded as `kind` extents, updating `i_blocks` accounting.
    fn allocate_holes(
        fs: &Arc<Ext4>,
        observer: &Ext4PageWritebackObserver,
        s: &mut ExtentTreeState,
        extent_status: Option<&ExtentStatusTree>,
        start_iblock: Iblock,
        end_iblock: Iblock,
        kind: node::ExtentKind,
        consume_reservation: bool,
    ) -> Result<()> {
        let device = fs.as_ref();
        let mut ib = start_iblock;
        while ib < end_iblock {
            let (extent, known_range) = s.tree.find_or_hole(device, ib)?;
            if let Some(extent) = extent {
                ib = end_iblock.min(extent.block() + u32::from(extent.len()));
                continue;
            }
            let hole_end = end_iblock.min(known_range.end);
            let goal = s.tree.allocation_goal(device, ib)?;
            Self::allocate_hole_range(
                fs,
                observer,
                s,
                extent_status,
                ib..hole_end,
                kind,
                consume_reservation,
                goal,
            )?;
            ib = hole_end;
        }
        Ok(())
    }

    fn allocate_hole_range(
        fs: &Arc<Ext4>,
        observer: &Ext4PageWritebackObserver,
        s: &mut ExtentTreeState,
        extent_status: Option<&ExtentStatusTree>,
        range: Range<Iblock>,
        kind: node::ExtentKind,
        consume_reservation: bool,
        mut goal: Ext4Bid,
    ) -> Result<()> {
        let device = fs.as_ref();
        let mut ib = range.start;
        while ib < range.end {
            let want = range.end - ib;
            let metadata_needed = s.tree.metadata_blocks_needed_for_insert(device, ib)?;
            let mut metadata_reservation = consume_reservation
                .then(|| fs.reserve_extent_metadata(metadata_needed))
                .transpose()?;
            let allocated = if consume_reservation {
                fs.alloc_reserved_blocks(want, goal)?
            } else {
                fs.alloc_blocks(want, goal)?
            };
            observer.touch_metadata_range(fs, allocated.clone());
            let got = u32::try_from(allocated.end - allocated.start).map_err(|_| {
                Error::with_message(Errno::EOVERFLOW, "allocated extent is too long")
            })?;
            debug_assert!(got > 0 && got <= want);
            let len = u16::try_from(got)
                .map_err(|_| Error::with_message(Errno::EOVERFLOW, "extent is too long"))?;
            let insert_result = match metadata_reservation.as_mut() {
                Some(reservation) => {
                    s.tree
                        .insert_reserved(fs, ib, allocated.start, len, kind, reservation)
                }
                None => s.tree.insert(fs, ib, allocated.start, len, kind),
            };
            let delta = match insert_result {
                Ok(delta) => delta,
                Err(err) => {
                    let _ = fs.free_blocks(allocated.start, got);
                    if consume_reservation {
                        let _ = fs.reserve_data_blocks(got);
                    }
                    return Err(err);
                }
            };
            touch_tree_delta(observer, fs, &delta);
            if let Some(extent_status) = extent_status {
                let status = match kind {
                    node::ExtentKind::Written => ExtentStatus::Written {
                        pblock: allocated.start,
                    },
                    node::ExtentKind::Unwritten => ExtentStatus::Unwritten {
                        pblock: allocated.start,
                    },
                };
                let materialized = extent_status.materialize_delayed(ib..ib + got, status);
                if materialized != got {
                    return_errno_with_message!(
                        Errno::EIO,
                        "allocated range no longer matches delayed reservation"
                    );
                }
            }
            let net_meta = delta.meta_allocated as i64 - delta.meta_freed as i64;
            s.pending_frees
                .extend(delta.deferred_frees.into_iter().map(|bid| bid..bid + 1));
            let added_blocks = got as i64 + net_meta;
            s.sector_count =
                (s.sector_count as i64 + added_blocks * SECTORS_PER_BLOCK as i64) as u64;
            s.dirty = true;
            ib += got;
            goal = allocated.end;
        }
        Ok(())
    }

    fn allocate_delayed(&self, start_iblock: Iblock, end_iblock: Iblock) -> Result<()> {
        if start_iblock >= end_iblock {
            return Ok(());
        }
        let fs = self.fs()?;
        let mut s = self.state.write();
        let device = fs.as_ref();
        let goal = s.tree.allocation_goal(device, start_iblock)?;
        Self::allocate_hole_range(
            &fs,
            &self.writeback_observer,
            &mut s,
            Some(&self.extent_status),
            start_iblock..end_iblock,
            node::ExtentKind::Unwritten,
            true,
            goal,
        )
    }

    /// Allocates a single data block for logical block `iblock` (assumed a hole)
    /// and records it. Used by the `submit_write_bio` hole fallback.
    fn allocate_one(&self, iblock: Iblock) -> Result<Ext4Bid> {
        let fs = self.fs()?;
        let mut s = self.state.write();
        let end_iblock = iblock
            .checked_add(1)
            .ok_or_else(|| Error::with_message(Errno::EOVERFLOW, "logical block overflow"))?;
        self.extent_status.invalidate(iblock..end_iblock);
        let range = fs.alloc_blocks(1, 0)?;
        self.writeback_observer
            .touch_metadata_range(&fs, range.clone());
        let pblock = range.start;
        let delta = match s
            .tree
            .insert(&fs, iblock, pblock, 1, node::ExtentKind::Written)
        {
            Ok(delta) => delta,
            Err(err) => {
                // Free the just-allocated block rather than leak it.
                let _ = fs.free_blocks(pblock, 1);
                return Err(err);
            }
        };
        touch_tree_delta(&self.writeback_observer, &fs, &delta);
        let net_meta = delta.meta_allocated as i64 - delta.meta_freed as i64;
        s.pending_frees
            .extend(delta.deferred_frees.into_iter().map(|bid| bid..bid + 1));
        let added_blocks = 1 + net_meta;
        s.sector_count = (s.sector_count as i64 + added_blocks * SECTORS_PER_BLOCK as i64) as u64;
        s.dirty = true;
        self.extent_status
            .insert(iblock..end_iblock, ExtentStatus::Written { pblock });
        Ok(pblock)
    }

    /// Frees every data block and extent-tree metadata block mapping a logical
    /// region at or beyond `new_size` bytes, rewriting the tree and updating
    /// `i_blocks`.
    pub(super) fn truncate_to_byte_len(&self, new_size: usize) -> Result<()> {
        let fs = self.fs()?;
        let keep_blocks = new_size.div_ceil(BLOCK_SIZE) as Iblock;
        let mut s = self.state.write();
        let delayed_ranges = self.extent_status.delayed_ranges(keep_blocks..Iblock::MAX);
        let delayed_to_release: Iblock = delayed_ranges
            .iter()
            .map(|range| range.end - range.start)
            .sum();
        self.extent_status.invalidate(keep_blocks..Iblock::MAX);

        let truncate_result = (|| {
            let (delta, removed) = s.tree.remove_range(&fs, keep_blocks..Iblock::MAX)?;
            let mut freed_data: u64 = 0;
            let mut deferred_data = Vec::new();
            let mut deferred_nv = Vec::new();
            for extent in removed {
                match extent.tier_start() {
                    node::TierBlock::Ssd(start) => {
                        deferred_data.push(start..start + Ext4Bid::from(extent.len()));
                    }
                    node::TierBlock::Nv(page) => {
                        for offset in 0..u32::from(extent.len()) {
                            let node::TierBlock::Nv(page) =
                                node::TierBlock::Nv(page).offset(offset)?
                            else {
                                unreachable!()
                            };
                            let target = fs
                                .nv_state()
                                .ok_or_else(|| Error::new(Errno::EIO))?
                                .data_page_target(page)?;
                            deferred_data.push(target..target + 1);
                            deferred_nv.push(page);
                        }
                    }
                }
                freed_data = freed_data
                    .checked_add(u64::from(extent.len()))
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            }

            touch_tree_delta(&self.writeback_observer, &fs, &delta);
            let net_meta = delta.meta_allocated as i64 - delta.meta_freed as i64;
            s.pending_frees.extend(deferred_data);
            s.pending_nv_frees.extend(deferred_nv);
            s.pending_frees
                .extend(delta.deferred_frees.into_iter().map(|bid| bid..bid + 1));
            let removed_sectors = (freed_data as i64 - net_meta) * SECTORS_PER_BLOCK as i64;
            debug_assert!(s.sector_count as i64 >= removed_sectors);
            s.sector_count = (s.sector_count as i64 - removed_sectors).max(0) as u64;
            s.dirty = true;
            Ok(())
        })();

        if let Err(err) = truncate_result {
            for range in delayed_ranges {
                self.extent_status.insert(range, ExtentStatus::Delayed);
            }
            return Err(err);
        }
        fs.release_reserved_data_blocks(delayed_to_release);
        Ok(())
    }

    pub(super) fn commit_pending_frees(&self) -> Result<()> {
        let fs = self.fs()?;
        let pending = {
            let mut state = self.state.write();
            core::mem::take(&mut state.pending_frees)
        };
        for (idx, range) in pending.iter().enumerate() {
            let count =
                u32::try_from(range.end - range.start).map_err(|_| Error::new(Errno::EOVERFLOW))?;
            if let Err(err) = fs.free_blocks(range.start, count) {
                self.state
                    .write()
                    .pending_frees
                    .extend_from_slice(&pending[idx..]);
                return Err(err);
            }
            self.writeback_observer
                .touch_released_metadata_range(&fs, range.clone());
        }
        let pending_nv = {
            let mut state = self.state.write();
            core::mem::take(&mut state.pending_nv_frees)
        };
        for (idx, page) in pending_nv.iter().copied().enumerate() {
            if let Err(err) = fs.release_nv_data_page(page) {
                self.state
                    .write()
                    .pending_nv_frees
                    .extend_from_slice(&pending_nv[idx..]);
                return Err(err);
            }
        }
        Ok(())
    }

    pub(super) fn drain_nv_data(&self) -> Result<()> {
        let fs = self.fs()?;
        let publisher = NvWritebackPublisher::new(self);
        let mut cursor = 0;
        loop {
            let extent = {
                self.state
                    .read()
                    .tree
                    .next_extent_at_or_after(&*fs, cursor)?
            };
            let Some(extent) = extent else {
                break;
            };
            cursor = extent
                .block()
                .checked_add(u32::from(extent.len()))
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            let node::TierBlock::Nv(start) = extent.tier_start() else {
                continue;
            };
            for offset in 0..u32::from(extent.len()) {
                let node::TierBlock::Nv(page) = node::TierBlock::Nv(start).offset(offset)? else {
                    unreachable!()
                };
                let iblock = extent
                    .block()
                    .checked_add(offset)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                let nv_state = fs.nv_state().ok_or_else(|| Error::new(Errno::EIO))?;
                let target = nv_state.data_page_target(page)?;
                nv_state.write_data_page_to_ssd(page, target)?;
                publisher.publish_range(iblock, 1, node::TierBlock::Ssd(target))?;
            }
        }
        Ok(())
    }
}

struct MappedWritebackPage {
    page: WritebackPage,
    iblock: Iblock,
    pblock: Ext4Bid,
    state: MapState,
}

#[derive(Clone, Copy)]
struct MappedSpan {
    iblock: Iblock,
    pblock: Ext4Bid,
    len: u32,
    state: MapState,
}

impl MappedSpan {
    fn map(&self, iblock: Iblock) -> Option<(Ext4Bid, MapState)> {
        let offset = iblock.checked_sub(self.iblock)?;
        (offset < self.len).then(|| (self.pblock + Ext4Bid::from(offset), self.state))
    }
}

impl BlockAsPageCacheBackend for ExtentManager {
    fn writeback_observer(&self) -> Option<Arc<dyn PageCacheWritebackObserver>> {
        Some(self.writeback_observer.clone())
    }

    fn submit_read_bio(
        &self,
        idx: usize,
        bio_segment: BioSegment,
        complete_fn: BioCompleteFn,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        if idx >= self.npages.load(Ordering::Acquire) {
            return_errno_with_message!(Errno::EINVAL, "read past end of inode");
        }
        let iblock = Iblock::try_from(idx)
            .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;

        let mapping = self.map_blocks(iblock)?;
        if mapping.reads_as_zeros() {
            // Holes and unwritten extents read as zeros without device I/O.
            complete_fn(BioStatus::Zeros);
            return Ok(());
        }

        let Some(tier_block) = mapping.tier_block() else {
            return_errno_with_message!(Errno::EIO, "allocated mapping has no physical block");
        };
        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem dropped"))?;
        match tier_block {
            node::TierBlock::Nv(page) => fs
                .nv_state()
                .ok_or_else(|| Error::with_message(Errno::EIO, "NV extent without NVPool"))?
                .read_data_segment(page, bio_segment, complete_fn),
            node::TierBlock::Ssd(pblock) if self.metadata_data.load(Ordering::Acquire) => {
                fs.read_metadata_segment(pblock, bio_segment, complete_fn)
            }
            node::TierBlock::Ssd(pblock) => {
                fs.read_blocks_async(pblock, bio_segment, Some(complete_fn), io_batch)
            }
        }
    }

    fn submit_write_bio(
        &self,
        idx: usize,
        bio_segment: BioSegment,
        complete_fn: BioCompleteFn,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        if idx >= self.npages.load(Ordering::Acquire) {
            return_errno_with_message!(Errno::EINVAL, "write past end of inode");
        }
        let iblock = Iblock::try_from(idx)
            .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;
        let fs = self.fs()?;

        // Buffered writes pre-allocate in `prepare_write`, so the block is
        // usually already mapped (written or unwritten). The hole branch is the
        // defensive fallback for mmap-dirtied pages, which the upper layer does
        // not pre-allocate.
        let mapping = self.map_blocks(iblock)?;
        let pblock = match mapping {
            Mapping::Mapped { pblock, .. } => match node::TierBlock::decode(pblock)? {
                node::TierBlock::Ssd(block) => block,
                node::TierBlock::Nv(page) => fs
                    .nv_state()
                    .ok_or_else(|| Error::with_message(Errno::EIO, "NV extent without NVPool"))?
                    .data_page_target(page)?,
            },
            Mapping::Hole { .. } => self.allocate_one(iblock)?,
            Mapping::Delayed { .. } => {
                self.allocate_delayed(iblock, iblock + 1)?;
                self.map_blocks(iblock)?
                    .pblock()
                    .ok_or_else(|| Error::with_message(Errno::EIO, "allocation stayed delayed"))?
            }
        };
        if fs.uses_nv_pool() && !self.metadata_data.load(Ordering::Acquire) {
            fs.enable_nv_tier()?;
            let page = fs
                .nv_state()
                .unwrap()
                .stage_data_segment(pblock, &bio_segment)?;
            if let Err(error) =
                NvWritebackPublisher::new(self).publish_range(iblock, 1, node::TierBlock::Nv(page))
            {
                let _ = fs.release_nv_data_page(page);
                return Err(error);
            }
            complete_fn(BioStatus::Complete);
            return Ok(());
        }
        if self.metadata_data.load(Ordering::Acquire) {
            fs.write_metadata_segments(pblock, vec![bio_segment], complete_fn)
        } else {
            fs.write_blocks_async_with_path(
                pblock,
                bio_segment,
                Some(complete_fn),
                io_batch,
                crate::fs::fs_impls::ext4::stats::Ext4IoPath::ExtentWriteback,
            )
        }
    }

    fn submit_write_pages(&self, pages: Vec<WritebackPage>, io_batch: &mut IoBatch) -> Result<()> {
        let fs = self.fs()?;
        let npages = self.npages.load(Ordering::Acquire);
        let mut page_runs: Vec<Range<Iblock>> = Vec::new();
        for page in &pages {
            let iblock = Iblock::try_from(page.index())
                .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;
            match page_runs.last_mut() {
                Some(run) if run.end == iblock => run.end += 1,
                _ => page_runs.push(iblock..iblock + 1),
            }
        }
        for delayed in self.extent_status.delayed_intersections(&page_runs) {
            self.allocate_delayed(delayed.start, delayed.end)?;
        }
        let mut mapped_pages = Vec::with_capacity(pages.len());
        let mut cached_span: Option<MappedSpan> = None;
        for page in pages {
            let idx = page.index();
            if idx >= npages {
                return_errno_with_message!(Errno::EINVAL, "write past end of inode");
            }
            let iblock = Iblock::try_from(idx)
                .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;
            let (pblock, state) =
                if let Some(mapped) = cached_span.and_then(|span| span.map(iblock)) {
                    mapped
                } else {
                    match self.map_blocks(iblock)? {
                        Mapping::Mapped { pblock, len, state } => {
                            let target = match node::TierBlock::decode(pblock)? {
                                node::TierBlock::Ssd(block) => block,
                                node::TierBlock::Nv(page) => fs
                                    .nv_state()
                                    .ok_or_else(|| {
                                        Error::with_message(Errno::EIO, "NV extent without NVPool")
                                    })?
                                    .data_page_target(page)?,
                            };
                            cached_span = Some(MappedSpan {
                                iblock,
                                pblock: target,
                                len,
                                state,
                            });
                            (target, state)
                        }
                        Mapping::Hole { .. } => return_errno_with_message!(
                            Errno::EIO,
                            "writeback page has no block mapping"
                        ),
                        Mapping::Delayed { .. } => return_errno_with_message!(
                            Errno::EIO,
                            "writeback page stayed delayed after allocation"
                        ),
                    }
                };
            mapped_pages.push(MappedWritebackPage {
                page,
                iblock,
                pblock,
                state,
            });
        }
        let max_segments = fs.block_device().metadata().max_nr_segments_per_bio;
        if max_segments == 0 {
            return_errno_with_message!(Errno::EIO, "block device accepts no BIO segments");
        }

        let mut run: Vec<MappedWritebackPage> = Vec::new();
        for mapped_page in mapped_pages {
            let can_append = run.last().is_none_or(|previous| {
                run.len() < max_segments
                    && mapped_page.iblock == previous.iblock + 1
                    && mapped_page.pblock == previous.pblock + Ext4Bid::from(1u32)
                    && mapped_page.state == previous.state
            });
            if !can_append {
                submit_mapped_writeback_run(self, core::mem::take(&mut run), io_batch)?;
            }
            run.push(mapped_page);
        }
        if !run.is_empty() {
            submit_mapped_writeback_run(self, run, io_batch)?;
        }
        Ok(())
    }
}

fn submit_mapped_writeback_run(
    manager: &ExtentManager,
    run: Vec<MappedWritebackPage>,
    io_batch: &mut IoBatch,
) -> Result<()> {
    let first = run.first().unwrap();
    let start_iblock = first.iblock;
    let start_pblock = first.pblock;
    let state = first.state;
    let len = u32::try_from(run.len())
        .map_err(|_| Error::with_message(Errno::EOVERFLOW, "writeback run is too long"))?;
    let pages = run.into_iter().map(|mapped| mapped.page).collect();
    let fs = manager.fs()?;
    let metadata_data = manager.metadata_data.load(Ordering::Acquire);

    if fs.uses_nv_pool() && !metadata_data {
        fs.enable_nv_tier()?;
        let publisher = NvWritebackPublisher::new(manager);
        let nv_state = fs.nv_state().unwrap().clone();
        return submit_writeback_run(pages, io_batch, move |segments, complete_fn, _io_batch| {
            for segment in &segments {
                if segment.offset_within_first_block() != 0 || segment.nbytes() % BLOCK_SIZE != 0 {
                    return_errno_with_message!(
                        Errno::EINVAL,
                        "NV data write must contain full pages"
                    );
                }
            }
            let mut staged = Vec::with_capacity(usize::try_from(len).unwrap());
            let mut offset = 0u32;
            for segment in &segments {
                let mut reader = segment.inner_dma_slice().reader().unwrap();
                for _ in 0..segment.nblocks() {
                    let target = start_pblock
                        .checked_add(Ext4Bid::from(offset))
                        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                    let mut content = [0; BLOCK_SIZE];
                    reader.read(&mut VmWriter::from(&mut content[..]));
                    match nv_state.stage_data_page(target, &content) {
                        Ok(page) => staged.push(page),
                        Err(error) => {
                            for page in staged {
                                let _ = nv_state.release_data_page(page);
                            }
                            return Err(error);
                        }
                    }
                    offset = offset
                        .checked_add(1)
                        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                }
            }
            debug_assert_eq!(offset, len);
            for run in coalesce_nv_publish_runs(&staged)? {
                let iblock = start_iblock
                    .checked_add(run.offset)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                if let Err(error) =
                    publisher.publish_range(iblock, run.len, node::TierBlock::Nv(run.start))
                {
                    let unpublished = run
                        .offset
                        .checked_add(run.len)
                        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                    for &page in &staged[usize::try_from(unpublished).unwrap()..] {
                        let _ = nv_state.release_data_page(page);
                    }
                    return Err(error);
                }
            }
            complete_fn(BioStatus::Complete);
            Ok(())
        });
    }

    if state == MapState::Unwritten {
        submit_writeback_run_with_post_io_job(
            pages,
            io_batch,
            move |segments, complete_fn, io_batch| {
                if metadata_data {
                    fs.write_metadata_segments(start_pblock, segments, complete_fn)
                } else {
                    fs.write_blocks_async_segments_with_path(
                        start_pblock,
                        segments,
                        Some(complete_fn),
                        io_batch,
                        crate::fs::fs_impls::ext4::stats::Ext4IoPath::ExtentWriteback,
                    )
                }
            },
            Box::new(ExtentWritebackPostIo::new(
                manager,
                start_iblock,
                start_pblock,
                len,
            )),
        )
    } else {
        submit_writeback_run(pages, io_batch, move |segments, complete_fn, io_batch| {
            if metadata_data {
                fs.write_metadata_segments(start_pblock, segments, complete_fn)
            } else {
                fs.write_blocks_async_segments_with_path(
                    start_pblock,
                    segments,
                    Some(complete_fn),
                    io_batch,
                    crate::fs::fs_impls::ext4::stats::Ext4IoPath::ExtentWriteback,
                )
            }
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NvPublishRun {
    offset: u32,
    len: u32,
    start: NvPageId,
}

fn coalesce_nv_publish_runs(pages: &[NvPageId]) -> Result<Vec<NvPublishRun>> {
    let mut runs: Vec<NvPublishRun> = Vec::new();
    for (offset, &page) in pages.iter().enumerate() {
        let offset = u32::try_from(offset).map_err(|_| Error::new(Errno::EOVERFLOW))?;
        if let Some(run) = runs.last_mut()
            && run.start.index().checked_add(run.len as usize) == Some(page.index())
        {
            run.len = run
                .len
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            continue;
        }
        runs.push(NvPublishRun {
            offset,
            len: 1,
            start: page,
        });
    }
    Ok(runs)
}

#[derive(Clone)]
struct NvWritebackPublisher {
    state: Arc<RwMutex<ExtentTreeState>>,
    extent_status: Arc<ExtentStatusTree>,
    weak_fs: Weak<Ext4>,
    observer: Arc<Ext4PageWritebackObserver>,
}

impl NvWritebackPublisher {
    fn new(manager: &ExtentManager) -> Self {
        Self {
            state: manager.state.clone(),
            extent_status: manager.extent_status.clone(),
            weak_fs: manager.fs.clone(),
            observer: manager.writeback_observer.clone(),
        }
    }

    fn publish_range(&self, iblock: Iblock, len: u32, tier: node::TierBlock) -> Result<()> {
        let fs = self
            .weak_fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem dropped"))?;
        let mut state = self.state.write();
        let (delta, old_nv) = state.tree.publish_tiered(&fs, iblock, len, tier)?;
        touch_tree_delta(&self.observer, &fs, &delta);
        let net_meta = delta.meta_allocated as i64 - delta.meta_freed as i64;
        state
            .pending_frees
            .extend(delta.deferred_frees.into_iter().map(|bid| bid..bid + 1));
        state.pending_nv_frees.extend(old_nv);
        state.sector_count =
            (state.sector_count as i64 + net_meta * SECTORS_PER_BLOCK as i64).max(0) as u64;
        state.dirty = true;
        drop(state);
        let end = iblock
            .checked_add(len)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.extent_status.invalidate(iblock..end);
        Ok(())
    }
}

struct ExtentWritebackPostIo {
    state: Arc<RwMutex<ExtentTreeState>>,
    extent_status: Arc<ExtentStatusTree>,
    weak_fs: Weak<Ext4>,
    observer: Arc<Ext4PageWritebackObserver>,
    start_iblock: Iblock,
    start_pblock: Ext4Bid,
    len: u32,
}

impl ExtentWritebackPostIo {
    fn new(manager: &ExtentManager, start_iblock: Iblock, start_pblock: Ext4Bid, len: u32) -> Self {
        Self {
            state: manager.state.clone(),
            extent_status: manager.extent_status.clone(),
            weak_fs: manager.fs.clone(),
            observer: manager.writeback_observer.clone(),
            start_iblock,
            start_pblock,
            len,
        }
    }
}

impl WritebackPostIo for ExtentWritebackPostIo {
    fn finish(self: Box<Self>, status: BioStatus) -> core::result::Result<(), io_util::IoError> {
        if status != BioStatus::Complete {
            return Err(io_util::IoError::Failed);
        }
        finish_unwritten_writeback(
            &self.state,
            &self.extent_status,
            &self.weak_fs,
            &self.observer,
            self.start_iblock,
            self.start_pblock,
            self.len,
        )
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn try_merge(
        &mut self,
        next: Box<dyn WritebackPostIo>,
    ) -> core::result::Result<(), Box<dyn WritebackPostIo>> {
        let Some(next_extent) = next.as_any().downcast_ref::<Self>() else {
            return Err(next);
        };
        let Some(next_logical) = self.start_iblock.checked_add(self.len) else {
            return Err(next);
        };
        let Some(next_physical) = self.start_pblock.checked_add(u64::from(self.len)) else {
            return Err(next);
        };
        let Some(merged_len) = self.len.checked_add(next_extent.len) else {
            return Err(next);
        };
        if !Arc::ptr_eq(&self.state, &next_extent.state)
            || !Arc::ptr_eq(&self.extent_status, &next_extent.extent_status)
            || next_logical != next_extent.start_iblock
            || next_physical != next_extent.start_pblock
        {
            return Err(next);
        }
        self.len = merged_len;
        Ok(())
    }
}

fn finish_unwritten_writeback(
    state: &RwMutex<ExtentTreeState>,
    extent_status: &ExtentStatusTree,
    weak_fs: &Weak<Ext4>,
    observer: &Ext4PageWritebackObserver,
    start_iblock: Iblock,
    start_pblock: Ext4Bid,
    len: u32,
) -> core::result::Result<(), io_util::IoError> {
    let fs = weak_fs.upgrade().ok_or(io_util::IoError::Failed)?;
    let end_iblock = start_iblock
        .checked_add(len)
        .ok_or(io_util::IoError::Failed)?;
    let mut state = state.write();
    extent_status.invalidate(start_iblock..end_iblock);
    let delta = state
        .tree
        .convert_unwritten(&fs, start_iblock, len)
        .map_err(|_| io_util::IoError::Failed)?;
    touch_tree_delta(observer, &fs, &delta);
    let net_meta = delta.meta_allocated as i64 - delta.meta_freed as i64;
    state
        .pending_frees
        .extend(delta.deferred_frees.into_iter().map(|bid| bid..bid + 1));
    state.sector_count =
        (state.sector_count as i64 + net_meta * SECTORS_PER_BLOCK as i64).max(0) as u64;
    state.dirty = true;
    extent_status.insert(
        start_iblock..end_iblock,
        ExtentStatus::Written {
            pblock: start_pblock,
        },
    );
    Ok(())
}

fn touch_tree_delta(observer: &Ext4PageWritebackObserver, fs: &Ext4, delta: &tree::TreeDelta) {
    for &bid in &delta.allocated {
        observer.touch_metadata_range(fs, bid..bid + 1);
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{
        node::{EXTENT_MAGIC, RawExtent, RawExtentHeader},
        *,
    };
    use crate::fs::fs_impls::ext4::{fs::ROOT_INO, test_utils::Ext4FixtureBuilder};

    fn inline_root(extents: &[RawExtent]) -> [u32; RAW_BLOCK_PTRS_LEN] {
        let mut block = [0u32; RAW_BLOCK_PTRS_LEN];
        let bytes = block.as_mut_bytes();
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: extents.len() as u16,
            max: 4,
            depth: 0,
            generation: 0,
        };
        bytes[0..12].copy_from_slice(header.as_bytes());
        for (i, extent) in extents.iter().enumerate() {
            let off = 12 * (1 + i);
            bytes[off..off + 12].copy_from_slice(extent.as_bytes());
        }
        block
    }

    #[ktest]
    fn coalesces_contiguous_nv_pages_into_publish_runs() {
        let pages = [7usize, 8, 12, 13, 14]
            .into_iter()
            .map(|index| NvPageId::try_from(index).unwrap())
            .collect::<Vec<_>>();

        let runs = coalesce_nv_publish_runs(&pages).unwrap();

        assert_eq!(runs.len(), 2);
        assert_eq!(
            (runs[0].offset, runs[0].len, runs[0].start.index()),
            (0, 2, 7)
        );
        assert_eq!(
            (runs[1].offset, runs[1].len, runs[1].start.index()),
            (2, 3, 12)
        );
    }

    #[ktest]
    fn map_unwritten_extent_reads_as_zeros() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let root = inline_root(&[RawExtent {
            block: 0,
            len: 32768 + 2, // unwritten, length 2
            start_hi: 0,
            start_lo: 500,
        }]);
        let em = ExtentManager::new(ROOT_INO, root, 2 * 8, f.ext4.this(), 2).unwrap();
        let m = em.map_blocks(0).unwrap();
        assert_eq!(m.state(), Some(MapState::Unwritten));
        assert!(m.reads_as_zeros());
        assert_eq!(m.pblock(), Some(500));
    }

    #[ktest]
    fn unwritten_post_io_converts_only_after_success() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let root = inline_root(&[RawExtent {
            block: 0,
            len: 32768 + 2,
            start_hi: 0,
            start_lo: 500,
        }]);
        let em = ExtentManager::new(ROOT_INO, root, 2 * 8, f.ext4.this(), 2).unwrap();
        let post_io = Box::new(ExtentWritebackPostIo::new(&em, 0, Ext4Bid::from(500u32), 2));

        assert_eq!(em.map_blocks(0).unwrap().state(), Some(MapState::Unwritten));
        post_io.finish(BioStatus::Complete).unwrap();
        assert_eq!(em.map_blocks(0).unwrap().state(), Some(MapState::Written));
        assert_eq!(em.map_blocks(1).unwrap().state(), Some(MapState::Written));
    }

    #[ktest]
    fn failed_unwritten_post_io_does_not_convert_extent() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let root = inline_root(&[RawExtent {
            block: 0,
            len: 32768 + 2,
            start_hi: 0,
            start_lo: 500,
        }]);
        let em = ExtentManager::new(ROOT_INO, root, 2 * 8, f.ext4.this(), 2).unwrap();
        let post_io = Box::new(ExtentWritebackPostIo::new(&em, 0, Ext4Bid::from(500u32), 2));

        assert!(post_io.finish(BioStatus::IoError).is_err());
        assert_eq!(em.map_blocks(0).unwrap().state(), Some(MapState::Unwritten));
    }

    #[ktest]
    fn adjacent_unwritten_post_io_ranges_merge_into_one_tree_conversion() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let root = inline_root(&[RawExtent {
            block: 0,
            len: 32768 + 4,
            start_hi: 0,
            start_lo: 500,
        }]);
        let em = ExtentManager::new(ROOT_INO, root, 4 * 8, f.ext4.this(), 4).unwrap();
        let mut first: Box<dyn WritebackPostIo> =
            Box::new(ExtentWritebackPostIo::new(&em, 0, Ext4Bid::from(500u32), 2));
        let second: Box<dyn WritebackPostIo> =
            Box::new(ExtentWritebackPostIo::new(&em, 2, Ext4Bid::from(502u32), 2));

        assert!(first.try_merge(second).is_ok());
        let merged = first
            .as_any()
            .downcast_ref::<ExtentWritebackPostIo>()
            .unwrap();
        assert_eq!(merged.start_iblock, 0);
        assert_eq!(merged.start_pblock, 500);
        assert_eq!(merged.len, 4);
        first.finish(BioStatus::Complete).unwrap();

        assert_eq!(em.map_blocks(0).unwrap().state(), Some(MapState::Written));
        assert_eq!(em.map_blocks(3).unwrap().state(), Some(MapState::Written));
    }

    #[ktest]
    fn map_blocks_populates_extent_and_hole_cache() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let root = inline_root(&[RawExtent {
            block: 4,
            len: 3,
            start_hi: 0,
            start_lo: 500,
        }]);
        let em = ExtentManager::new(ROOT_INO, root, 3 * 8, f.ext4.this(), 8).unwrap();

        assert_eq!(em.extent_status.node_count(), 0);
        assert_eq!(em.map_blocks(4).unwrap().pblock(), Some(500));
        assert_eq!(em.extent_status.node_count(), 1);
        assert_eq!(em.map_blocks(5).unwrap().pblock(), Some(501));
        assert_eq!(em.extent_status.node_count(), 1);
        assert_eq!(em.cache_stats(), (1, 1, 1));

        assert!(em.map_blocks(0).unwrap().reads_as_zeros());
        assert_eq!(em.extent_status.node_count(), 2);
        assert_eq!(em.cache_stats(), (1, 2, 2));

        assert!(em.map_blocks(1).unwrap().reads_as_zeros());
        assert_eq!(em.cache_stats(), (2, 2, 2));

        assert_eq!(em.shrink_extent_cache(usize::MAX), 2);
        assert_eq!(em.extent_status.node_count(), 0);
        assert_eq!(em.map_blocks(4).unwrap().pblock(), Some(500));
        assert_eq!(em.cache_stats(), (2, 3, 3));
    }

    /// Regression: when `allocate_one` allocates a data block but the following
    /// tree insertion fails (here the inline→depth-1 grow needs a leaf block and
    /// the disk is out of space), the data block must be freed, not leaked.
    #[ktest]
    fn allocate_one_frees_block_when_insert_fails() {
        // Exactly one free block: enough for the data block, not the tree leaf.
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_free_blocks(1)
            .build()
            .unwrap();
        // A full inline root (4 extents); inserting a 5th forces a depth-1 grow.
        let root = inline_root(&[
            RawExtent {
                block: 0,
                len: 1,
                start_hi: 0,
                start_lo: 100,
            },
            RawExtent {
                block: 2,
                len: 1,
                start_hi: 0,
                start_lo: 200,
            },
            RawExtent {
                block: 4,
                len: 1,
                start_hi: 0,
                start_lo: 300,
            },
            RawExtent {
                block: 6,
                len: 1,
                start_hi: 0,
                start_lo: 400,
            },
        ]);
        let em = ExtentManager::new(ROOT_INO, root, 4 * 8, f.ext4.this(), 8).unwrap();

        let free_before = f.ext4.super_block().free_blocks_count();
        assert_eq!(free_before, 1);

        // The 5th mapping triggers inline→depth-1; the leaf allocation hits ENOSPC.
        assert!(em.allocate_one(8).is_err());

        // The data block allocated before the failed insert was reclaimed.
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before);
    }

    #[ktest]
    fn delayed_materialization_does_not_scan_the_full_extent_tree() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let em = ExtentManager::new(
            ROOT_INO,
            inline_root(&[RawExtent {
                block: 0,
                len: 1,
                start_hi: 0,
                start_lo: 100,
            }]),
            8,
            f.ext4.this(),
            8,
        )
        .unwrap();

        assert!(matches!(em.map_blocks(2).unwrap(), Mapping::Hole { .. }));
        let reservation = em.reserve_delayed(2, 4).unwrap();
        assert_eq!(reservation.delayed_ranges, vec![2..4]);
        let scans_before = em.state.read().tree.full_scan_count();

        em.materialize_delayed(2, 4).unwrap();

        assert_eq!(em.state.read().tree.full_scan_count(), scans_before);
        assert!(matches!(
            em.map_blocks(2).unwrap(),
            Mapping::Mapped {
                state: MapState::Unwritten,
                ..
            }
        ));
    }

    #[ktest]
    fn truncate_does_not_scan_or_rebuild_the_full_extent_tree() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let em = ExtentManager::new(
            ROOT_INO,
            inline_root(&[RawExtent {
                block: 0,
                len: 8,
                start_hi: 0,
                start_lo: 500,
            }]),
            8 * 8,
            f.ext4.this(),
            8,
        )
        .unwrap();

        em.truncate_to_byte_len(2 * BLOCK_SIZE).unwrap();

        assert_eq!(em.state.read().tree.full_scan_count(), 0);
        assert_eq!(em.map_blocks(1).unwrap().pblock(), Some(501));
        assert!(em.map_blocks(2).unwrap().reads_as_zeros());
        assert_eq!(em.state.read().pending_frees, vec![502..508]);
    }

    #[ktest]
    fn hole_allocation_paths_do_not_scan_the_full_extent_tree() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let em = ExtentManager::new(
            ROOT_INO,
            inline_root(&[RawExtent {
                block: 0,
                len: 1,
                start_hi: 0,
                start_lo: 100,
            }]),
            8,
            f.ext4.this(),
            8,
        )
        .unwrap();
        let scans_before = em.state.read().tree.full_scan_count();

        em.ensure_allocated(2, 4).unwrap();
        em.preallocate(6, 8).unwrap();

        assert_eq!(em.state.read().tree.full_scan_count(), scans_before);
        assert!(matches!(
            em.map_blocks(2).unwrap(),
            Mapping::Mapped {
                state: MapState::Written,
                ..
            }
        ));
        assert!(matches!(
            em.map_blocks(6).unwrap(),
            Mapping::Mapped {
                state: MapState::Unwritten,
                ..
            }
        ));
    }
}
