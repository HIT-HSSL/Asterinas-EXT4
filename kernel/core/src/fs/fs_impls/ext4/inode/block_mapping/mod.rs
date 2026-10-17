// SPDX-License-Identifier: MPL-2.0

//! Per-inode logical-to-physical block mapping.
//!
//! `BlockMapping` is the single dispatch point between an inode's file view
//! and the engine that maps its logical blocks to device blocks. The variant
//! is selected once, when the inode is loaded or created, and never changes
//! for the life of the inode. Every engine keeps its own interior lock; this
//! layer adds none, so the cross-layer lock order documented in
//! [`super`] is unchanged.

use core::ops::Range;

use super::{super::prelude::*, Inode, InodeDesc, RAW_BLOCK_PTRS_LEN, empty_extent_root};

pub(in crate::fs::fs_impls::ext4) mod extent;
#[cfg_attr(not(ktest), expect(dead_code))]
mod extent_status;
mod indirect;

use self::{
    extent::{ExtentManager, node::TierBlock},
    indirect::IndirectManager,
};
use super::super::fs::Ext4;
use crate::vm::page_cache::PageCacheWritebackObserver;

/// State of an allocated logical block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MapState {
    /// Backed by written data on disk.
    Written,
    /// Allocated but never written; reads as zeros.
    Unwritten,
}

/// The result of mapping a contiguous logical-block run.
///
/// This inherits the direct-I/O run classification role of ext2's old
/// `IoRange` (mapped vs. hole), generalized into this per-run `map_blocks`
/// form so the indirect and extent engines can share one interface and express
/// the extent engine's unwritten state -- the same shape as Linux's
/// `ext4_map_blocks`.
#[derive(Clone, Copy, Debug)]
pub(super) enum Mapping {
    /// A run backed by physical blocks.
    Mapped {
        pblock: Ext4Bid,
        len: u32,
        state: MapState,
    },
    /// A run with no physical allocation.
    Hole { len: u32 },
    /// A dirty page-cache run with reserved but not yet allocated blocks.
    Delayed { len: u32 },
}

impl Mapping {
    /// Returns the starting physical block for an allocated run.
    pub(super) fn pblock(&self) -> Option<Ext4Bid> {
        match self {
            Self::Mapped { pblock, .. } => match TierBlock::decode(*pblock) {
                Ok(TierBlock::Ssd(block)) => Some(block),
                _ => None,
            },
            Self::Hole { .. } | Self::Delayed { .. } => None,
        }
    }

    pub(super) fn tier_block(&self) -> Option<TierBlock> {
        match self {
            Self::Mapped { pblock, .. } => TierBlock::decode(*pblock).ok(),
            Self::Hole { .. } | Self::Delayed { .. } => None,
        }
    }

    /// Returns the number of contiguous logical blocks this mapping describes.
    pub(super) const fn len(&self) -> u32 {
        match self {
            Self::Mapped { len, .. } | Self::Hole { len } | Self::Delayed { len } => *len,
        }
    }

    #[cfg(ktest)]
    pub(super) const fn state(&self) -> Option<MapState> {
        match self {
            Self::Mapped { state, .. } => Some(*state),
            Self::Hole { .. } | Self::Delayed { .. } => None,
        }
    }

    /// Returns whether reading these blocks must return zeros without device I/O.
    pub(super) const fn reads_as_zeros(&self) -> bool {
        matches!(
            self,
            Self::Hole { .. }
                | Self::Delayed { .. }
                | Self::Mapped {
                    state: MapState::Unwritten,
                    ..
                }
        )
    }
}

/// A data-backed inode's block-mapping engine.
///
/// The variant follows the inode's `EXTENTS` flag: ext4-native inodes map
/// through an extent tree rooted inline in `i_block`, while ext2-style inodes
/// map through the classic direct/indirect pointer tree stored in the same
/// bytes. All engine state, including the interior lock, lives in the
/// variant; callers treat this enum as the engine.
pub(super) enum BlockMapping {
    Extent(ExtentManager),
    Indirect(IndirectManager),
}

#[derive(Default)]
pub(super) struct BufferedWriteReservation {
    delayed_ranges: Vec<Range<Iblock>>,
}

impl BlockMapping {
    /// Builds the mapping engine for a data-backed inode from its descriptor.
    pub(super) fn new(
        ino: Ext4Ino,
        desc: &InodeDesc,
        fs: Weak<Ext4>,
        npages: usize,
    ) -> Result<Self> {
        let root = *desc.raw_block();
        let sector_count = desc.sector_count();
        if desc.is_extent_based() {
            let manager = ExtentManager::new(ino, root, sector_count, fs, npages)?;
            Ok(Self::Extent(manager))
        } else {
            let manager = IndirectManager::new(ino, root, sector_count, fs, npages)?;
            Ok(Self::Indirect(manager))
        }
    }

    /// Builds an empty mapping in the volume's native format.
    pub(super) fn new_empty(
        ino: Ext4Ino,
        fs: Weak<Ext4>,
        extent_based: bool,
        npages: usize,
    ) -> Result<Self> {
        if extent_based {
            Ok(Self::Extent(ExtentManager::new(
                ino,
                empty_extent_root(),
                0,
                fs,
                npages,
            )?))
        } else {
            Ok(Self::Indirect(IndirectManager::new(
                ino,
                [0u32; RAW_BLOCK_PTRS_LEN],
                0,
                fs,
                npages,
            )?))
        }
    }

    pub(super) fn bind_writeback_target(&self, inode: Weak<Inode>) {
        match self {
            Self::Extent(manager) => manager.bind_writeback_target(inode),
            Self::Indirect(manager) => manager.bind_writeback_target(inode),
        }
    }

    /// Maps logical block `iblock` to a physical run.
    ///
    /// The returned length spans from `iblock` to the end of the covering
    /// mapped (or hole) run, so callers can batch contiguous I/O.
    pub(super) fn map_blocks(&self, iblock: Iblock) -> Result<Mapping> {
        match self {
            Self::Extent(m) => m.map_blocks(iblock),
            Self::Indirect(m) => m.map_blocks(iblock),
        }
    }

    /// Reclaims volatile extent-cache entries while retaining delayed state.
    #[expect(dead_code)]
    pub(super) fn shrink_extent_cache(&self, limit: usize) -> usize {
        match self {
            Self::Extent(m) => m.shrink_extent_cache(limit),
            Self::Indirect(_) => 0,
        }
    }

    /// Allocates data blocks for every hole in `[start_iblock, end_iblock)`
    /// and records them in the mapping.
    pub(super) fn ensure_allocated(&self, start_iblock: Iblock, end_iblock: Iblock) -> Result<()> {
        match self {
            Self::Extent(m) => m.ensure_allocated(start_iblock, end_iblock),
            Self::Indirect(m) => {
                m.allocate_range_blocks(start_iblock as usize, end_iblock as usize)
            }
        }
    }

    pub(super) fn prepare_buffered_write(
        &self,
        start_iblock: Iblock,
        end_iblock: Iblock,
    ) -> Result<BufferedWriteReservation> {
        match self {
            Self::Extent(m) if m.uses_delalloc()? => m.reserve_delayed(start_iblock, end_iblock),
            _ => {
                self.ensure_allocated(start_iblock, end_iblock)?;
                Ok(BufferedWriteReservation::default())
            }
        }
    }

    pub(super) fn cancel_buffered_write(&self, reservation: BufferedWriteReservation) {
        if let Self::Extent(m) = self {
            m.cancel_delayed(reservation);
        }
    }

    /// Preallocates blocks for every hole in `[start_iblock, end_iblock)`
    /// without touching file size. Backs `fallocate`.
    ///
    /// The extent engine records the new blocks as unwritten extents (they
    /// read as zeros until written); the indirect engine has no unwritten
    /// state, so it allocates real zeroed blocks (ext2 semantics).
    pub(super) fn preallocate(&self, start_iblock: Iblock, end_iblock: Iblock) -> Result<()> {
        match self {
            Self::Extent(m) => {
                m.materialize_delayed(start_iblock, end_iblock)?;
                m.preallocate(start_iblock, end_iblock)
            }
            Self::Indirect(m) => {
                m.allocate_range_blocks(start_iblock as usize, end_iblock as usize)
            }
        }
    }

    /// Frees every data and engine-metadata block mapping a logical region at
    /// or beyond `new_size` bytes, updating `i_blocks`.
    pub(super) fn truncate_to_byte_len(&self, new_size: usize) -> Result<()> {
        match self {
            Self::Extent(m) => m.truncate_to_byte_len(new_size),
            Self::Indirect(m) => {
                // The indirect engine truncates best-effort (errors are logged
                // and the freed prefix is kept); the inode reclaim path relies
                // on truncation never failing.
                m.truncate_to_byte_len(new_size);
                Ok(())
            }
        }
    }

    /// Returns a copy of the inode's 60-byte `i_block` as the engine persists
    /// it (the extent-tree root or the pointer array).
    pub(super) fn root_snapshot(&self) -> [u32; RAW_BLOCK_PTRS_LEN] {
        match self {
            Self::Extent(m) => m.root_snapshot(),
            Self::Indirect(m) => m.root_snapshot(),
        }
    }

    /// Returns the inode's `i_blocks` (512-byte sectors) accounting.
    pub(super) fn sector_count(&self) -> u64 {
        match self {
            Self::Extent(m) => m.sector_count(),
            Self::Indirect(m) => m.sector_count(),
        }
    }

    /// Flushes mapping metadata before the inode that references it.
    pub(super) fn sync_metadata(&self) -> Result<()> {
        match self {
            Self::Extent(_) => Ok(()),
            Self::Indirect(m) => m.sync_indirect_blocks(),
        }
    }

    /// Releases blocks only after the inode no longer references them durably.
    pub(super) fn commit_pending_frees(&self) -> Result<()> {
        match self {
            Self::Extent(m) => m.commit_pending_frees(),
            Self::Indirect(_) => Ok(()),
        }
    }

    pub(super) fn drain_nv_data(&self) -> Result<()> {
        match self {
            Self::Extent(m) => m.drain_nv_data(),
            Self::Indirect(_) => Ok(()),
        }
    }

    pub(in crate::fs::fs_impls::ext4) fn reclaim_extent_root(
        &self,
    ) -> Result<[u32; RAW_BLOCK_PTRS_LEN]> {
        match self {
            Self::Extent(m) => Ok(m.root_snapshot()),
            Self::Indirect(_) => return_errno_with_message!(
                Errno::EOPNOTSUPP,
                "NVPool recovery requires extent-mapped inodes"
            ),
        }
    }

    /// Returns whether the mapping or `i_blocks` changed since last writeback.
    pub(super) fn is_dirty(&self) -> bool {
        match self {
            Self::Extent(m) => m.is_dirty(),
            Self::Indirect(m) => m.is_dirty(),
        }
    }

    /// Clears the dirty flag after a successful inode writeback.
    pub(super) fn clear_dirty(&self) {
        match self {
            Self::Extent(m) => m.clear_dirty(),
            Self::Indirect(m) => m.clear_dirty(),
        }
    }

    /// Updates the cached page-cache capacity bound.
    pub(super) fn set_npages(&self, npages: usize) {
        match self {
            Self::Extent(m) => m.set_npages(npages),
            Self::Indirect(m) => m.set_npages(npages),
        }
    }

    /// Returns the largest byte size a file with this mapping type can reach.
    ///
    /// An extent maps a 32-bit logical block index, so an extent-mapped file
    /// spans at most `2^32 - 1` blocks, clamped to `i64::MAX` (the VFS size
    /// limit); the mutation path has a stricter practical limit because it
    /// only rebuilds extent trees up to depth 1. Indirect-mapped files follow
    /// the ext2 geometry and `i_blocks` accounting limits.
    pub(super) fn max_file_size(&self) -> usize {
        match self {
            Self::Extent(_) => {
                let by_blocks = (u32::MAX as u64) * BLOCK_SIZE as u64;
                usize::try_from(by_blocks.min(i64::MAX as u64)).unwrap_or(usize::MAX)
            }
            Self::Indirect(_) => indirect::max_file_size(),
        }
    }
}

impl BlockAsPageCacheBackend for BlockMapping {
    fn writeback_observer(&self) -> Option<Arc<dyn PageCacheWritebackObserver>> {
        match self {
            Self::Extent(m) => BlockAsPageCacheBackend::writeback_observer(m),
            Self::Indirect(m) => BlockAsPageCacheBackend::writeback_observer(m),
        }
    }

    fn submit_read_bio(
        &self,
        idx: usize,
        bio_segment: BioSegment,
        complete_fn: BioCompleteFn,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        match self {
            Self::Extent(m) => m.submit_read_bio(idx, bio_segment, complete_fn, io_batch),
            Self::Indirect(m) => m.submit_read_bio(idx, bio_segment, complete_fn, io_batch),
        }
    }

    fn submit_write_bio(
        &self,
        idx: usize,
        bio_segment: BioSegment,
        complete_fn: BioCompleteFn,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        match self {
            Self::Extent(m) => m.submit_write_bio(idx, bio_segment, complete_fn, io_batch),
            Self::Indirect(m) => m.submit_write_bio(idx, bio_segment, complete_fn, io_batch),
        }
    }

    fn submit_write_pages(&self, pages: Vec<WritebackPage>, io_batch: &mut IoBatch) -> Result<()> {
        match self {
            Self::Extent(m) => m.submit_write_pages(pages, io_batch),
            Self::Indirect(m) => m.submit_write_pages(pages, io_batch),
        }
    }
}
