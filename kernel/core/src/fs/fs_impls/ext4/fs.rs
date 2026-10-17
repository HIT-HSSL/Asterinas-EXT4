// SPDX-License-Identifier: MPL-2.0

//! The `Ext4` filesystem object: mount, geometry, block allocation, and inode
//! lookup.
//!
//! Mutable filesystem metadata uses dirty tracking.
//! Each block group caches its allocation bitmaps and loaded inodes, while
//! `Ext4` coordinates allocation and persists the resulting metadata updates.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use device_id::DeviceId;
use ostd::mm::io::util::HasVmReaderWriter;

#[cfg(ktest)]
use super::stats::Ext4IoStatsSnapshot;
#[cfg(any(feature = "ext4-research-stats", ktest))]
use super::stats::{Ext4IoStats, Ext4SyncStage};
use super::{
    block_group::{BlockGroup, RawBlockGroup},
    extent_mutation_anchor::PendingExtentMutation,
    feature::{self, FeatureIncompatSet},
    inode::{FilePerm, Inode, InodeDesc, RawInode, recover_rename},
    journal::Journal,
    metadata_cache::{MetadataBlockCache, MetadataBlockHandle},
    nv::Ext4NvState,
    prelude::*,
    references::Ext4ReferenceWalker,
    rename_anchor::RenameAnchor,
    stats::{Ext4IoPath, sync_clock_now},
    super_block::{RawSuperBlock, SUPER_BLOCK_OFFSET, SuperBlock},
    utils,
    writeback::{PageCacheWritebackController, WritebackConfig},
};
#[cfg(not(ktest))]
use crate::thread::kernel_thread::ThreadOptions;
use crate::{
    fs::{
        nv_pool::reclaim::{ObjectId as ReclaimObjectId, ReclaimEpoch, ReclaimTracker},
        vfs::file_system::{AtomicFsFlags, FsEventSubscriberStats, FsFlags},
    },
    process::{Gid, UserNamespace, credentials::capabilities::CapSet, posix_thread::AsPosixThread},
    security::lsm::hooks as lsm_hooks,
    thread::Thread,
};

/// Root directory inode number.
pub(super) const ROOT_INO: Ext4Ino = 2;

/// An ext4 filesystem instance.
pub(crate) struct Ext4 {
    block_device: Arc<dyn BlockDevice>,
    metadata_cache: Arc<MetadataBlockCache>,
    /// Runtime JBD2 state for journaled mounts; absent for nojournal volumes.
    journal: Mutex<Option<Arc<Journal>>>,
    /// NV-backed metadata access for this mount.
    nv_state: Option<Arc<Ext4NvState>>,
    /// Superblock with dirty tracking.
    super_block: RwMutex<Dirty<SuperBlock>>,
    /// Per-group block-side metadata (descriptor + block bitmap).
    block_groups: Vec<BlockGroup>,
    /// Inodes per group, cached once at mount to avoid locking `super_block` on
    /// the inode read path.
    nr_inodes_per_group: u32,
    /// The filesystem-type name this instance was mounted as.
    flavor: MountFlavor,
    /// Runtime mount options that affect block-count reporting.
    mount_options: ExtMountOptions,
    /// Data blocks promised to delayed-allocation extents but not yet allocated.
    reserved_data_blocks: AtomicU64,
    /// Blocks reserved for an in-progress extent-tree copy-on-write mutation.
    reserved_metadata_blocks: AtomicU64,
    /// Recorded VFS filesystem flags.
    flags: AtomicFsFlags,
    /// Monotonic source for the `i_generation` stamped onto each newly created
    /// inode. Seeded from the mount time, like ext2.
    next_generation: AtomicU32,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    io_stats: Arc<Ext4IoStats>,
    writeback_controller: Arc<PageCacheWritebackController>,
    nv_tier_transition: Mutex<()>,
    reclaim_tracker: ReclaimTracker,
    fs_event_subscriber_stats: FsEventSubscriberStats,
    self_ref: Weak<Ext4>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ReclaimStats {
    pub(super) reclaimed_blocks: u64,
    pub(super) reclaimed_inodes: u64,
    pub(super) reclaimed_resident_frames: u64,
    pub(super) skipped_objects: u64,
}

impl Drop for Ext4 {
    fn drop(&mut self) {
        self.writeback_controller.stop();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockReservation {
    None,
    Data,
    Metadata,
}

/// A short-lived reservation for extent-tree copy-on-write blocks.
pub(super) struct ExtentMetadataReservation<'a> {
    fs: &'a Ext4,
    remaining: u32,
}

impl ExtentMetadataReservation<'_> {
    pub(super) fn alloc_block(&mut self, goal: Ext4Bid) -> Result<Ext4Bid> {
        let range = self
            .fs
            .alloc_blocks_inner(1, goal, BlockReservation::Metadata)?;
        self.remaining = self
            .remaining
            .checked_sub(1)
            .expect("metadata allocation is covered by its reservation");
        Ok(range.start)
    }

    /// Restores one reservation unit after its allocated block was freed.
    pub(super) fn restore_block(&mut self) {
        let _sb = self.fs.super_block.write();
        self.fs
            .reserved_metadata_blocks
            .fetch_add(1, Ordering::AcqRel);
        self.remaining = self
            .remaining
            .checked_add(1)
            .expect("extent metadata reservation count overflow");
    }
}

impl Drop for ExtentMetadataReservation<'_> {
    fn drop(&mut self) {
        self.fs.release_reserved_metadata_blocks(self.remaining);
    }
}

/// Which filesystem-type name this instance was mounted as.
///
/// The unified driver registers both the `ext2` and the `ext4` type names.
/// The flavor controls the mount-time feature validation (an `ext2` mount
/// only accepts true ext2-format volumes, like Linux's `IS_EXT2_SB` rule)
/// and the name reported back to the VFS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MountFlavor {
    Ext2,
    Ext4,
}

impl MountFlavor {
    /// The filesystem-type name this flavor was mounted as.
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Ext2 => "ext2",
            Self::Ext4 => "ext4",
        }
    }
}

/// Policy for how `statfs` reports the total block count.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum StatBlockAccounting {
    /// Excludes filesystem overhead (superblock, group descriptors, bitmaps,
    /// inode tables) from the reported total.
    ///
    /// This is the default and corresponds to Linux's `bsddf` mount option.
    #[default]
    ExcludeOverhead,
    /// Includes all blocks on the device in the reported total, regardless of
    /// whether they hold metadata.
    ///
    /// This corresponds to Linux's `minixdf` mount option.
    IncludeOverhead,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExtMountOptions {
    stat_block_accounting: StatBlockAccounting,
    delalloc: bool,
    nvpool: bool,
    writeback_capacity_bytes: Option<usize>,
}

impl Default for ExtMountOptions {
    fn default() -> Self {
        Self {
            stat_block_accounting: StatBlockAccounting::default(),
            delalloc: true,
            nvpool: false,
            writeback_capacity_bytes: None,
        }
    }
}

impl ExtMountOptions {
    /// Parses mount options that control block-count reporting.
    fn parse(data: Option<&str>) -> Self {
        let mut options = Self::default();
        let Some(data) = data else {
            return options;
        };

        for token in data.split(',') {
            let token = token.trim();
            match token {
                "bsddf" => options.stat_block_accounting = StatBlockAccounting::ExcludeOverhead,
                "minixdf" => options.stat_block_accounting = StatBlockAccounting::IncludeOverhead,
                "delalloc" => options.delalloc = true,
                "nodelalloc" => options.delalloc = false,
                "nvpool" => options.nvpool = true,
                "nonvpool" => options.nvpool = false,
                _ => {
                    if let Some(value) = token.strip_prefix("writeback_capacity_mb=") {
                        options.writeback_capacity_bytes = value
                            .parse::<usize>()
                            .ok()
                            .filter(|value| *value > 0)
                            .and_then(|value| value.checked_mul(1024 * 1024));
                    }
                }
            }
        }

        options
    }
}

impl Ext4 {
    /// Mounts a volume from a block device under the given type name.
    pub(super) fn open(
        device: Arc<dyn BlockDevice>,
        flags: FsFlags,
        flavor: MountFlavor,
        data: Option<&str>,
    ) -> Result<Arc<Self>> {
        let mount_options = ExtMountOptions::parse(data);
        let pool = if mount_options.nvpool {
            Some(
                crate::fs::nv_pool::pool()
                    .cloned()
                    .ok_or_else(|| Error::with_message(Errno::ENODEV, "NVPool is not available"))?,
            )
        } else {
            None
        };
        Self::open_with_pool(device, flags, flavor, data, pool)
    }

    fn open_with_pool(
        device: Arc<dyn BlockDevice>,
        flags: FsFlags,
        flavor: MountFlavor,
        data: Option<&str>,
        nv_pool: Option<Arc<crate::fs::nv_pool::frame::NvPool>>,
    ) -> Result<Arc<Self>> {
        let bootstrap_raw = device.read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)?;
        let bootstrap = SuperBlock::try_from(bootstrap_raw)?;
        let nv_state = nv_pool
            .map(|pool| {
                Ext4NvState::new(
                    pool,
                    device.clone(),
                    bootstrap.block_size(),
                    bootstrap_raw.uuid,
                )
                .map(Arc::new)
            })
            .transpose()?;
        #[cfg(not(ktest))]
        let nv_recovery_start = nv_state.as_ref().map(|_| sync_clock_now());
        let raw_super_block = if let Some(nv_state) = &nv_state {
            nv_state.read_meta_at::<RawSuperBlock>(SUPER_BLOCK_OFFSET)?
        } else {
            bootstrap_raw
        };
        let super_block = SuperBlock::try_from(raw_super_block)?;
        feature::check_flavor(&super_block, flavor, nv_state.is_some())?;
        if nv_state.is_some()
            && super_block
                .feature_compat()
                .contains(feature::FeatureCompatSet::HAS_JOURNAL)
        {
            return_errno_with_message!(
                Errno::EINVAL,
                "NVPool and JBD2 are separate persistence modes"
            );
        }
        let nr_inodes_per_group = super_block.nr_inodes_per_group();
        if let Some(nv_state) = &nv_state {
            nv_state.preflight_pinned_roots(
                usize::try_from(super_block.nr_block_groups())
                    .map_err(|_| Error::new(Errno::EOVERFLOW))?,
                size_of::<RawBlockGroup>(),
            )?;
        }
        let metadata_cache = Arc::new(MetadataBlockCache::new(
            device.clone(),
            nv_state.clone(),
            super_block.block_size(),
            usize::try_from(super_block.nr_block_groups())
                .unwrap_or(1)
                .saturating_mul(8)
                .max(8),
        ));
        let block_groups = Self::load_block_groups(
            device.clone(),
            metadata_cache.clone(),
            nv_state.clone(),
            &super_block,
        )?;
        if let Some(nv_state) = &nv_state {
            let roots = core::iter::once(
                Ext4Bid::try_from(SUPER_BLOCK_OFFSET / super_block.block_size())
                    .map_err(|_| Error::new(Errno::EOVERFLOW))?,
            )
            .chain(
                block_groups
                    .iter()
                    .flat_map(BlockGroup::pinned_metadata_blocks),
            );
            nv_state.pin_metadata_roots(roots)?;
        }

        let mount_options = ExtMountOptions::parse(data);
        let writeback_config = mount_options
            .writeback_capacity_bytes
            .map(WritebackConfig::from_capacity_bytes)
            .unwrap_or_else(|| WritebackConfig::from_memory_bytes(crate::vm::mem_total()));
        let fs = Arc::new_cyclic(|weak| Ext4 {
            block_device: device,
            metadata_cache,
            journal: Mutex::new(None),
            nv_state,
            super_block: RwMutex::new(Dirty::new(super_block)),
            block_groups,
            nr_inodes_per_group,
            flavor,
            mount_options,
            reserved_data_blocks: AtomicU64::new(0),
            reserved_metadata_blocks: AtomicU64::new(0),
            flags: AtomicFsFlags::new(flags),
            next_generation: AtomicU32::new(
                u32::try_from(utils::now().as_secs() & u64::from(u32::MAX))
                    .expect("masked generation fits u32"),
            ),
            #[cfg(any(feature = "ext4-research-stats", ktest))]
            io_stats: Arc::new(Ext4IoStats::default()),
            writeback_controller: Arc::new(PageCacheWritebackController::new(writeback_config)),
            nv_tier_transition: Mutex::new(()),
            reclaim_tracker: ReclaimTracker::new(),
            fs_event_subscriber_stats: FsEventSubscriberStats::new(),
            self_ref: weak.clone(),
        });
        let journal = Journal::load(&fs)?;
        if let Some(journal) = &journal {
            fs.metadata_cache.attach_journal(journal);
        }
        *fs.journal.lock() = journal;
        if let Some(nv_state) = &fs.nv_state {
            for ino in nv_state.pending_extent_mutation_inodes()? {
                if let Some(mutation) = PendingExtentMutation::load(nv_state, ino)? {
                    super::inode::block_mapping::extent::recover_pending_extent_mutation(
                        &fs, &mutation,
                    )?;
                }
            }
        }
        if let Some(nv_state) = &fs.nv_state
            && let Some(anchor) = RenameAnchor::load(nv_state)?
        {
            recover_rename(&fs, anchor)?;
        }
        #[cfg(not(ktest))]
        if let Some(start) = nv_recovery_start {
            info!(
                "ext4 NV mount recovery completed in {} ns",
                sync_clock_now().saturating_sub(start).as_nanos()
            );
        }
        fs.writeback_controller.start()?;
        #[cfg(not(ktest))]
        if fs.nv_state.is_some() {
            let weak = Arc::downgrade(&fs);
            ThreadOptions::new(move || {
                let Some(fs) = weak.upgrade() else {
                    return;
                };
                let start = sync_clock_now();
                match fs.reclaim_unreachable() {
                    Ok(stats) => info!(
                        "ext4 NV reclaim completed in {} ns: blocks={}, inodes={}, resident_frames={}, skipped={}",
                        sync_clock_now().saturating_sub(start).as_nanos(),
                        stats.reclaimed_blocks,
                        stats.reclaimed_inodes,
                        stats.reclaimed_resident_frames,
                        stats.skipped_objects,
                    ),
                    Err(error) => warn!("ext4 NV reclaim stopped safely: {:?}", error),
                }
            })
            .spawn();
        }
        Ok(fs)
    }

    /// Loads every block group from the descriptor table, which immediately
    /// follows the block holding the superblock.
    fn load_block_groups(
        device: Arc<dyn BlockDevice>,
        metadata_cache: Arc<MetadataBlockCache>,
        nv_state: Option<Arc<Ext4NvState>>,
        super_block: &SuperBlock,
    ) -> Result<Vec<BlockGroup>> {
        let nr_groups = usize::try_from(super_block.nr_block_groups())
            .map_err(|_| Error::with_message(Errno::EOVERFLOW, "block group count overflow"))?;
        let gdt_block = super_block
            .first_data_block()
            .checked_add(1)
            .ok_or_else(|| Error::with_message(Errno::EOVERFLOW, "descriptor block overflow"))?;
        let gdt_base_offset = utils::block_offset(gdt_block, super_block.block_size())?;

        let mut block_groups = Vec::with_capacity(nr_groups);
        for group_idx in 0..nr_groups {
            let group = BlockGroup::load(
                device.clone(),
                metadata_cache.clone(),
                nv_state.clone(),
                group_idx,
                super_block,
                gdt_base_offset,
            )?;
            block_groups.push(group);
        }
        Ok(block_groups)
    }

    pub(super) fn block_device(&self) -> &Arc<dyn BlockDevice> {
        &self.block_device
    }

    pub(super) fn journal(&self) -> Option<Arc<Journal>> {
        self.journal.lock().clone()
    }

    pub(super) fn uses_journal(&self) -> bool {
        self.journal.lock().is_some()
    }

    pub(super) fn commit_journal(&self) -> Result<()> {
        if let Some(journal) = self.journal() {
            if let Some(receipt) = journal.commit()? {
                self.metadata_cache.finish_journal_commit(&receipt);
            }
        }
        Ok(())
    }

    pub(super) fn read_metadata_block(&self, bid: Ext4Bid) -> Result<[u8; BLOCK_SIZE]> {
        self.metadata_cache.read(bid, |bytes| {
            bytes
                .try_into()
                .map_err(|_| Error::with_message(Errno::EIO, "metadata block size mismatch"))
        })
    }

    pub(super) fn write_metadata_block(
        &self,
        bid: Ext4Bid,
        block: &[u8; BLOCK_SIZE],
    ) -> Result<()> {
        let handle = self.metadata_cache.modify(bid, |content| {
            if content.len() != block.len() {
                return_errno_with_message!(Errno::EIO, "metadata block size mismatch");
            }
            content.copy_from_slice(block);
            Ok(())
        })?;
        self.metadata_cache.sync(&handle)
    }

    pub(super) fn read_metadata_segment(
        &self,
        bid: Ext4Bid,
        segment: BioSegment,
        complete_fn: BioCompleteFn,
    ) -> Result<()> {
        if segment.nbytes() != BLOCK_SIZE || segment.offset_within_first_block() != 0 {
            return_errno_with_message!(Errno::EINVAL, "metadata read must cover one full block");
        }
        let block = self.read_metadata_block(bid)?;
        segment
            .inner_dma_slice()
            .writer()
            .unwrap()
            .write(&mut VmReader::from(&block[..]));
        complete_fn(BioStatus::Complete);
        Ok(())
    }

    pub(super) fn write_metadata_segments(
        &self,
        start_bid: Ext4Bid,
        segments: Vec<BioSegment>,
        complete_fn: BioCompleteFn,
    ) -> Result<()> {
        let mut bid = start_bid;
        for segment in segments {
            if segment.nbytes() % BLOCK_SIZE != 0 || segment.offset_within_first_block() != 0 {
                return_errno_with_message!(
                    Errno::EINVAL,
                    "metadata write must contain full blocks"
                );
            }
            let mut reader = segment.inner_dma_slice().reader().unwrap();
            for _ in 0..segment.nblocks() {
                let mut block = [0; BLOCK_SIZE];
                reader.read(&mut VmWriter::from(&mut block[..]));
                self.write_metadata_block(bid, &block)?;
                bid = bid
                    .checked_add(1)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            }
        }
        complete_fn(BioStatus::Complete);
        Ok(())
    }

    #[cfg(ktest)]
    pub(super) fn open_with_nv_pool(
        device: Arc<dyn BlockDevice>,
        flags: FsFlags,
        flavor: MountFlavor,
        data: Option<&str>,
        pool: Arc<crate::fs::nv_pool::frame::NvPool>,
    ) -> Result<Arc<Self>> {
        Self::open_with_pool(device, flags, flavor, data, Some(pool))
    }

    #[cfg(any(feature = "ext4-research-stats", ktest))]
    pub(super) fn io_stats(&self) -> &Ext4IoStats {
        &self.io_stats
    }

    pub(super) fn writeback_controller(&self) -> &Arc<PageCacheWritebackController> {
        &self.writeback_controller
    }

    pub(super) fn nv_state(&self) -> Option<&Arc<Ext4NvState>> {
        self.nv_state.as_ref()
    }

    pub(super) fn uses_nv_pool(&self) -> bool {
        self.nv_state.is_some()
    }

    pub(super) fn release_nv_data_page(
        &self,
        page: crate::fs::nv_pool::id::NvPageId,
    ) -> Result<()> {
        self.nv_state
            .as_ref()
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "NVPool is not enabled"))?
            .release_data_page(page)
    }

    pub(super) fn enable_nv_tier(&self) -> Result<()> {
        let _transition = self.nv_tier_transition.lock();
        {
            let sb = self.super_block.read();
            if sb.feature_incompat().contains(FeatureIncompatSet::NV_TIER) {
                return Ok(());
            }
        }
        self.persist_nv_tier_guard(true)?;
        self.super_block
            .write()
            .set_incompat_feature(FeatureIncompatSet::NV_TIER, true);
        if let Err(error) = self.sync_metadata() {
            self.super_block
                .write()
                .set_incompat_feature(FeatureIncompatSet::NV_TIER, false);
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn drain_nv_data(&self) -> Result<()> {
        let first_ino = self.super_block.read().first_ino();
        for (group_idx, group) in self.block_groups.iter().enumerate() {
            let base = u32::try_from(group_idx)
                .map_err(|_| Error::new(Errno::EOVERFLOW))?
                .checked_mul(self.nr_inodes_per_group)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            for offset in group.allocated_inode_offsets() {
                let ino = base
                    .checked_add(offset)
                    .and_then(|value| value.checked_add(1))
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                if ino != ROOT_INO && ino < first_ino {
                    continue;
                }
                self.read_inode(ino)?.drain_nv_data()?;
            }
        }
        Ok(())
    }

    pub(super) fn finish_nv_drain(&self) -> Result<()> {
        let _transition = self.nv_tier_transition.lock();
        self.persist_nv_tier_guard(false)?;
        self.super_block
            .write()
            .set_incompat_feature(FeatureIncompatSet::NV_TIER, false);
        Ok(())
    }

    fn persist_nv_tier_guard(&self, enabled: bool) -> Result<()> {
        let mut raw = self
            .block_device
            .read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)?;
        let feature = FeatureIncompatSet::NV_TIER.bits();
        if enabled {
            raw.feature_incompat |= feature;
        } else {
            raw.feature_incompat &= !feature;
        }
        self.block_device.write_val(SUPER_BLOCK_OFFSET, &raw)?;
        if self.block_device.sync()? != BioStatus::Complete {
            return_errno_with_message!(Errno::EIO, "failed to persist NV-tier mount guard");
        }
        Ok(())
    }

    pub(super) fn begin_reclaim_epoch(&self) -> Result<ReclaimEpoch> {
        let _reclaim_mutation = self.reclaim_tracker.lock_mutation();
        let mut allocated: BTreeSet<_> = self
            .allocated_blocks_snapshot()
            .into_iter()
            .map(ReclaimObjectId::Block)
            .collect();
        for (group_idx, group) in self.block_groups.iter().enumerate() {
            let inode_base = u32::try_from(group_idx)
                .map_err(|_| Error::new(Errno::EOVERFLOW))?
                .checked_mul(self.nr_inodes_per_group)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            allocated.extend(group.allocated_inode_offsets().into_iter().map(|offset| {
                ReclaimObjectId::Inode(
                    inode_base
                        .checked_add(offset)
                        .and_then(|ino| ino.checked_add(1))
                        .expect("validated ext4 inode geometry cannot overflow"),
                )
            }));
        }
        self.reclaim_tracker.begin(allocated)
    }

    fn allocated_blocks_snapshot(&self) -> BTreeSet<Ext4Bid> {
        self.block_groups
            .iter()
            .flat_map(BlockGroup::allocated_blocks)
            .collect()
    }

    pub(super) fn is_block_allocated(&self, block: Ext4Bid) -> bool {
        self.block_groups
            .iter()
            .any(|group| group.is_block_allocated(block))
    }

    pub(super) fn reclaim_unreachable(&self) -> Result<ReclaimStats> {
        let mut stats = ReclaimStats::default();
        let mut epoch = self.begin_reclaim_epoch()?;
        if let Some(nv_state) = &self.nv_state {
            let _reclaim_mutation = self.reclaim_tracker.lock_mutation();
            let allocated = self.allocated_blocks_snapshot();
            stats.reclaimed_resident_frames =
                nv_state.reclaim_unallocated_residents(|block| allocated.contains(&block))? as u64;
        }
        let referenced_nv_pages = Ext4ReferenceWalker::mark_mounted_filesystem(self, &mut epoch)?;
        if let Some(nv_state) = &self.nv_state {
            stats.reclaimed_resident_frames +=
                nv_state.reclaim_orphan_data_pages(&referenced_nv_pages)? as u64;
        }

        let mut pinned = BTreeSet::new();
        let super_block = self.super_block.read();
        pinned.extend((1..super_block.first_ino()).map(ReclaimObjectId::Inode));
        pinned.insert(ReclaimObjectId::Block(
            Ext4Bid::try_from(SUPER_BLOCK_OFFSET / super_block.block_size())
                .map_err(|_| Error::new(Errno::EOVERFLOW))?,
        ));
        let backup_gdt_blocks = Ext4Bid::from(super_block.group_descriptor_blocks_count());
        for (group_idx, group) in self.block_groups.iter().enumerate() {
            if super_block.has_super_block(group_idx) {
                let backup_super = group.first_block();
                pinned.insert(ReclaimObjectId::Block(backup_super));
                pinned.extend(
                    (backup_super + 1..backup_super + 1 + backup_gdt_blocks)
                        .map(ReclaimObjectId::Block),
                );
            }
        }
        drop(super_block);
        for group in &self.block_groups {
            pinned.extend(
                group
                    .system_metadata_blocks()
                    .into_iter()
                    .map(ReclaimObjectId::Block),
            );
        }

        let candidates = epoch.sweep_candidates(&pinned);
        let mut inode_candidates = Vec::new();
        let mut block_candidates = Vec::new();
        for candidate in candidates {
            match candidate {
                ReclaimObjectId::Block(block) => block_candidates.push(block),
                ReclaimObjectId::Inode(ino) => inode_candidates.push(ino),
            }
        }

        for ino in inode_candidates {
            let reclaimed = epoch.reclaim_if_untouched(ReclaimObjectId::Inode(ino), || {
                let inode = self.read_inode(ino)?;
                let type_ = inode.reclaim_inode_type();
                self.free_inode_inner(ino, type_, false)?;
                let _ = self.remove_inode(ino);
                Ok(true)
            })?;
            if reclaimed {
                stats.reclaimed_inodes += 1;
            } else {
                stats.skipped_objects += 1;
            }
        }
        if stats.reclaimed_inodes != 0 {
            self.sync_metadata()?;
        }

        for block in block_candidates {
            let reclaimed = epoch.reclaim_if_untouched(ReclaimObjectId::Block(block), || {
                if !self.is_block_allocated(block) {
                    return Ok(false);
                }
                self.free_blocks_inner(block, 1, false)?;
                Ok(true)
            })?;
            if reclaimed {
                stats.reclaimed_blocks += 1;
            } else {
                stats.skipped_objects += 1;
            }
        }
        if stats.reclaimed_blocks != 0 {
            self.sync_metadata()?;
        }
        if let Some(nv_state) = &self.nv_state {
            let _reclaim_mutation = self.reclaim_tracker.lock_mutation();
            let allocated = self.allocated_blocks_snapshot();
            stats.reclaimed_resident_frames +=
                nv_state.reclaim_unallocated_residents(|block| allocated.contains(&block))? as u64;
        }
        Ok(stats)
    }

    pub(super) fn commit_rename_anchor(&self, anchor: &RenameAnchor) -> Result<()> {
        match &self.nv_state {
            Some(nv_state) => anchor.persist(nv_state),
            None => Ok(()),
        }
    }

    pub(super) fn clear_rename_anchor(&self) -> Result<()> {
        match &self.nv_state {
            Some(nv_state) => RenameAnchor::clear(nv_state),
            None => Ok(()),
        }
    }

    #[cfg(ktest)]
    pub(super) fn io_stats_snapshot(&self) -> Ext4IoStatsSnapshot {
        self.io_stats.snapshot()
    }

    /// Returns whether `statfs` reports totals in the `minixdf` style
    /// (including metadata overhead).
    pub(super) fn uses_minix_df(&self) -> bool {
        self.mount_options.stat_block_accounting == StatBlockAccounting::IncludeOverhead
    }

    pub(super) fn uses_delalloc(&self) -> bool {
        self.mount_options.delalloc
    }

    #[cfg(ktest)]
    pub(super) fn reserved_data_blocks(&self) -> u64 {
        self.reserved_data_blocks.load(Ordering::Acquire)
    }

    #[cfg(ktest)]
    pub(super) fn reserved_metadata_blocks(&self) -> u64 {
        self.reserved_metadata_blocks.load(Ordering::Acquire)
    }

    pub(super) fn reserve_data_blocks(&self, count: u32) -> Result<()> {
        if count == 0 {
            return Ok(());
        }

        let sb = self.super_block.write();
        let already_reserved = self.reserved_data_blocks.load(Ordering::Relaxed);
        let metadata_reserved = self.reserved_metadata_blocks.load(Ordering::Relaxed);
        let available = sb
            .free_blocks_count()
            .saturating_sub(already_reserved)
            .saturating_sub(metadata_reserved);
        let count = u64::from(count);
        if count > available
            || !Self::can_alloc(
                available - count + 1,
                sb.reserved_blocks_count(),
                sb.default_reserved_uid(),
                sb.default_reserved_gid(),
            )
        {
            return_errno_with_message!(Errno::ENOSPC, "insufficient space for delayed allocation");
        }
        self.reserved_data_blocks
            .store(already_reserved + count, Ordering::Release);
        Ok(())
    }

    pub(super) fn release_reserved_data_blocks(&self, count: u32) {
        if count == 0 {
            return;
        }
        let _sb = self.super_block.write();
        let previous = self
            .reserved_data_blocks
            .fetch_sub(u64::from(count), Ordering::AcqRel);
        debug_assert!(previous >= u64::from(count));
    }

    pub(super) fn reserve_extent_metadata(
        &self,
        count: u32,
    ) -> Result<ExtentMetadataReservation<'_>> {
        if count == 0 {
            return Ok(ExtentMetadataReservation {
                fs: self,
                remaining: 0,
            });
        }

        let sb = self.super_block.write();
        let data_reserved = self.reserved_data_blocks.load(Ordering::Relaxed);
        let metadata_reserved = self.reserved_metadata_blocks.load(Ordering::Relaxed);
        let available = sb
            .free_blocks_count()
            .saturating_sub(data_reserved)
            .saturating_sub(metadata_reserved);
        let count_u64 = u64::from(count);
        if count_u64 > available
            || !Self::can_alloc(
                available - count_u64 + 1,
                sb.reserved_blocks_count(),
                sb.default_reserved_uid(),
                sb.default_reserved_gid(),
            )
        {
            return_errno_with_message!(Errno::ENOSPC, "insufficient space for extent metadata");
        }
        self.reserved_metadata_blocks
            .store(metadata_reserved + count_u64, Ordering::Release);
        Ok(ExtentMetadataReservation {
            fs: self,
            remaining: count,
        })
    }

    fn release_reserved_metadata_blocks(&self, count: u32) {
        if count == 0 {
            return;
        }
        let _sb = self.super_block.write();
        let previous = self
            .reserved_metadata_blocks
            .fetch_sub(u64::from(count), Ordering::AcqRel);
        debug_assert!(previous >= u64::from(count));
    }

    /// Returns the filesystem-type name this instance was mounted as.
    pub(super) fn flavor(&self) -> MountFlavor {
        self.flavor
    }

    /// Returns the per-filesystem VFS flags.
    pub(super) fn fs_flags(&self) -> FsFlags {
        self.flags.load(Ordering::Relaxed)
    }

    /// Replaces the per-filesystem VFS flags.
    pub(super) fn set_fs_flags(&self, flags: FsFlags) {
        self.flags.store(flags, Ordering::Relaxed);
    }

    /// Returns a reference to the block group at `group_idx`.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn block_group(&self, group_idx: usize) -> &BlockGroup {
        &self.block_groups[group_idx]
    }

    pub(super) fn record_block_group_range(
        &self,
        groups: &mut BTreeSet<usize>,
        range: Range<Ext4Bid>,
    ) {
        if range.is_empty() {
            return;
        }
        let sb = self.super_block.read();
        let first_data_block = Ext4Bid::from(sb.first_data_block());
        let blocks_per_group = Ext4Bid::from(sb.nr_blocks_per_group());
        if blocks_per_group == 0 || range.start < first_data_block {
            return;
        }
        let first = (range.start - first_data_block) / blocks_per_group;
        let last = (range.end - 1 - first_data_block) / blocks_per_group;
        for group_idx in first..=last {
            if let Ok(group_idx) = usize::try_from(group_idx)
                && group_idx < self.block_groups.len()
            {
                groups.insert(group_idx);
            }
        }
    }

    pub(super) fn sync_metadata_groups(
        &self,
        groups: impl IntoIterator<Item = usize>,
    ) -> Result<()> {
        let groups = groups.into_iter().collect::<BTreeSet<_>>();
        if groups.is_empty() {
            return Ok(());
        }
        // Allocators update a group and the global free counters while holding
        // this lock. Keep the same serialization across targeted writeback so
        // the superblock snapshot cannot include a change from an unsynced
        // group.
        let mut sb = self.super_block.write();
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            self.io_stats.record_scanned_groups(groups.len());
        }
        for group_idx in groups {
            let group = self.block_groups.get(group_idx).ok_or_else(|| {
                Error::with_message(Errno::EIO, "metadata dependency group is out of range")
            })?;
            group.sync_metadata()?;
        }
        self.sync_superblock_snapshot(&sb)?;
        sb.clear_dirty();
        Ok(())
    }

    fn sync_superblock_snapshot(&self, sb: &SuperBlock) -> Result<()> {
        let block_size = sb.block_size();
        let free_blocks = u32::try_from(sb.free_blocks_count()).map_err(|_| {
            Error::with_message(Errno::EOVERFLOW, "free block count exceeds disk field")
        })?;
        let free_inodes = sb.free_inodes_count();
        let feature_incompat = sb.feature_incompat().bits();
        let home = Ext4Bid::try_from(SUPER_BLOCK_OFFSET / block_size)
            .map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let slot = SUPER_BLOCK_OFFSET % block_size;
        let handle = self.metadata_cache.modify(home, |bytes| {
            let raw_bytes = bytes
                .get_mut(slot..slot + size_of::<RawSuperBlock>())
                .ok_or_else(|| Error::with_message(Errno::EIO, "superblock crosses block"))?;
            let mut raw = RawSuperBlock::from_bytes(raw_bytes);
            raw.free_blocks_count = free_blocks;
            raw.free_inodes_count = free_inodes;
            raw.feature_incompat = feature_incompat;
            raw.wtime = utils::now().into();
            raw_bytes.copy_from_slice(raw.as_bytes());
            Ok(())
        })?;
        self.metadata_cache.sync(&handle)
    }

    /// Returns a read guard of the superblock.
    pub(super) fn super_block(&self) -> RwMutexReadGuard<'_, Dirty<SuperBlock>> {
        self.super_block.read()
    }

    /// Returns the device ID of the backing block device.
    pub(super) fn container_device_id(&self) -> DeviceId {
        self.block_device.id()
    }

    pub(super) fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats {
        &self.fs_event_subscriber_stats
    }

    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn this(&self) -> Weak<Ext4> {
        self.self_ref.clone()
    }

    /// Reads the root directory inode.
    pub(super) fn root_inode(&self) -> Result<Arc<Inode>> {
        self.read_inode(ROOT_INO)
    }

    /// Reads an inode, returning the cached `Arc<Inode>` if one already exists.
    ///
    /// Routing through the owning group's inode cache gives every reader of one
    /// inode number the same in-memory inode (identity), which the
    /// filesystem-level sync relies on to enumerate and flush all dirty inodes.
    pub(super) fn read_inode(&self, ino: Ext4Ino) -> Result<Arc<Inode>> {
        self.find_group(ino)?
            .lookup_inode(ino, self.self_ref.clone())
    }

    /// Inserts a newly created inode into its block group's live cache.
    pub(super) fn insert_inode(&self, inode: Arc<Inode>) {
        if let Ok(group) = self.find_group(inode.ino()) {
            group.insert_inode(inode);
        }
    }

    /// Removes an inode from its block group's live cache.
    pub(super) fn remove_inode(&self, ino: Ext4Ino) -> Option<Arc<Inode>> {
        self.find_group(ino)
            .ok()
            .and_then(|group| group.remove_inode(ino))
    }

    /// Returns whether `ino` is marked allocated in its owning group's inode
    /// bitmap. Used by the reclaim path to skip an already-freed inode. Mirrors
    /// ext2 routing through the owning block group.
    pub(super) fn is_inode_allocated(&self, ino: Ext4Ino) -> bool {
        self.find_group(ino)
            .map(|group| group.is_inode_allocated(ino))
            .unwrap_or(false)
    }

    /// Updates the cached `RawInode` for `ino`, patching only the
    /// fields that buffered writes can mutate (size, `i_blocks`, the extent
    /// root, timestamps, flags, and link count) and preserving everything else
    /// (`extra_isize`, checksums, generation, xattr tail, osd fields) losslessly.
    pub(super) fn write_back_inode_desc(
        &self,
        ino: Ext4Ino,
        desc: &InodeDesc,
        root: &[u32; super::inode::RAW_BLOCK_PTRS_LEN],
    ) -> Result<()> {
        let group = self.find_group(ino)?;
        let dependency = group.update_inode_desc(ino, |raw| raw.update_from_desc(desc, root))?;
        self.sync_metadata_block(&dependency)
    }

    /// Writes the complete on-disk `RawInode` for a freshly created inode.
    ///
    /// Unlike [`write_back_inode_desc`](Self::write_back_inode_desc), which is a
    /// read-modify-write tuned for the buffered-write path (and therefore
    /// preserves the on-disk type bits and generation), this writes every field
    /// of a brand-new inode from scratch: the type/permission mode, owners,
    /// timestamps, generation, the inline extent root, flags, link count, and
    /// `extra_isize`. The previous slot contents (a deleted inode or zeros) are
    /// fully overwritten.
    fn write_new_inode_desc(&self, ino: Ext4Ino, desc: &InodeDesc) -> Result<()> {
        let raw = RawInode::from_desc(desc)?;
        let group = self.find_group(ino)?;
        let (dependency, old_raw) = group.write_new_inode_desc(ino, &raw)?;
        if let Err(error) = self.sync_metadata_block(&dependency) {
            if let Err(restore_error) = group.update_inode_desc(ino, |cached| {
                *cached = old_raw;
                Ok(())
            }) {
                error!(
                    "failed to restore cached inode {} after writeback error: {:?}",
                    ino, restore_error
                );
            }
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn sync_metadata_block(&self, handle: &MetadataBlockHandle) -> Result<()> {
        self.metadata_cache.sync(handle)
    }

    /// Allocates up to `count` contiguous blocks, preferring the group that owns
    /// `goal`.
    ///
    /// Searches groups in a ring starting from the goal group. Returns
    /// `Err(ENOSPC)` if no group can satisfy the request, `Err(EINVAL)` if
    /// `count` is zero.
    pub(super) fn alloc_blocks(&self, count: u32, goal: Ext4Bid) -> Result<Range<Ext4Bid>> {
        self.alloc_blocks_inner(count, goal, BlockReservation::None)
    }

    /// Allocates blocks already promised by delayed-allocation reservation.
    pub(super) fn alloc_reserved_blocks(
        &self,
        count: u32,
        goal: Ext4Bid,
    ) -> Result<Range<Ext4Bid>> {
        self.alloc_blocks_inner(count, goal, BlockReservation::Data)
    }

    fn alloc_blocks_inner(
        &self,
        count: u32,
        goal: Ext4Bid,
        reservation: BlockReservation,
    ) -> Result<Range<Ext4Bid>> {
        if count == 0 {
            return_errno_with_message!(Errno::EINVAL, "zero block allocation requested");
        }

        let _reclaim_mutation = self.reclaim_tracker.lock_mutation();
        let mut sb = self.super_block.write();
        let nr_block_groups = usize::try_from(sb.nr_block_groups())
            .map_err(|_| Error::with_message(Errno::EOVERFLOW, "block group count overflow"))?;
        let sb_free_blocks = sb.free_blocks_count();
        let reserved_data = self.reserved_data_blocks.load(Ordering::Acquire);
        let reserved_metadata = self.reserved_metadata_blocks.load(Ordering::Acquire);
        let requested = u64::from(count);
        match reservation {
            BlockReservation::Data if requested > reserved_data => {
                return_errno_with_message!(Errno::EIO, "delayed allocation exceeds reservation");
            }
            BlockReservation::Metadata if requested > reserved_metadata => {
                return_errno_with_message!(
                    Errno::EIO,
                    "extent metadata allocation exceeds reservation"
                );
            }
            _ => {}
        }
        let allocatable_blocks = match reservation {
            BlockReservation::None => sb_free_blocks
                .saturating_sub(reserved_data)
                .saturating_sub(reserved_metadata),
            BlockReservation::Data => sb_free_blocks.saturating_sub(reserved_metadata),
            BlockReservation::Metadata => sb_free_blocks.saturating_sub(reserved_data),
        };
        let first_data_block = sb.first_data_block();
        let nr_blocks_per_group = Ext4Bid::from(sb.nr_blocks_per_group());
        if allocatable_blocks == 0 {
            return_errno_with_message!(Errno::ENOSPC, "no free blocks on device");
        }

        if reservation == BlockReservation::None
            && !Self::can_alloc(
                allocatable_blocks,
                sb.reserved_blocks_count(),
                sb.default_reserved_uid(),
                sb.default_reserved_gid(),
            )
        {
            return_errno_with_message!(
                Errno::ENOSPC,
                "no free blocks available for unprivileged user"
            );
        }

        let goal_group = if goal > first_data_block {
            usize::try_from((goal - first_data_block) / nr_blocks_per_group).unwrap_or(usize::MAX)
        } else {
            0
        }
        .min(nr_block_groups - 1);

        for group_search_offset in 0..nr_block_groups {
            let group_idx = (goal_group + group_search_offset) % nr_block_groups;
            let group = &self.block_groups[group_idx];

            let range = group.alloc_blocks(count, allocatable_blocks, goal)?;
            if !range.is_empty() {
                let allocated_count = range.end - range.start;
                sb.dec_free_blocks(allocated_count)?;
                match reservation {
                    BlockReservation::Data => {
                        let previous = self
                            .reserved_data_blocks
                            .fetch_sub(allocated_count, Ordering::AcqRel);
                        debug_assert!(previous >= allocated_count);
                    }
                    BlockReservation::Metadata => {
                        let previous = self
                            .reserved_metadata_blocks
                            .fetch_sub(allocated_count, Ordering::AcqRel);
                        debug_assert!(previous >= allocated_count);
                    }
                    BlockReservation::None => {}
                }
                self.reclaim_tracker.touch_blocks(range.start..range.end);
                return Ok(range);
            }
        }

        return_errno_with_message!(Errno::ENOSPC, "no free blocks available in any group");
    }

    /// Frees `count` blocks starting at `start`, splitting across groups.
    pub(super) fn free_blocks(&self, start: Ext4Bid, count: u32) -> Result<()> {
        if count == 0 {
            return Ok(());
        }

        if let Some(journal) = self.journal() {
            let end = start
                .checked_add(Ext4Bid::from(count))
                .ok_or_else(|| Error::with_message(Errno::EOVERFLOW, "block range overflow"))?;
            journal.prepare_block_reuse(start..end)?;
        }

        let _reclaim_mutation = self.reclaim_tracker.lock_mutation();
        self.free_blocks_inner(start, count, true)
    }

    fn free_blocks_inner(&self, start: Ext4Bid, count: u32, track_reclaim: bool) -> Result<()> {
        let mut sb = self.super_block.write();
        let nr_blocks_per_group = Ext4Bid::from(sb.nr_blocks_per_group());
        let first_data_block = sb.first_data_block();
        if start < first_data_block {
            return_errno_with_message!(Errno::EINVAL, "block range starts before data blocks");
        }
        let end = start
            .checked_add(Ext4Bid::from(count))
            .ok_or_else(|| Error::with_message(Errno::EOVERFLOW, "block range overflow"))?;
        if end > sb.total_blocks() {
            return_errno_with_message!(Errno::EINVAL, "block range outside filesystem");
        }

        let mut current_block = start;
        let mut remaining_blocks = count;

        while remaining_blocks > 0 {
            let group_idx = usize::try_from(
                (current_block - first_data_block) / nr_blocks_per_group,
            )
            .map_err(|_| Error::with_message(Errno::EOVERFLOW, "block group index overflow"))?;
            let group = self.block_groups.get(group_idx).ok_or_else(|| {
                Error::with_message(Errno::EINVAL, "block range outside filesystem")
            })?;

            let group_first_block = group.first_block();
            let group_last_block = group.last_block();
            if current_block < group_first_block || current_block > group_last_block {
                return_errno_with_message!(Errno::EINVAL, "block is outside its group");
            }
            let group_size = u32::try_from(group_last_block - group_first_block + 1)
                .map_err(|_| Error::with_message(Errno::EOVERFLOW, "block group is too large"))?;
            let group_start_bit = u32::try_from(current_block - group_first_block)
                .map_err(|_| Error::with_message(Errno::EINVAL, "block is outside its group"))?;
            let blocks_in_group = remaining_blocks.min(group_size - group_start_bit);
            let freed_count =
                group.free_blocks(group_start_bit..(group_start_bit + blocks_in_group))?;
            if freed_count > 0 {
                sb.inc_free_blocks(u64::from(freed_count))?;
                if track_reclaim {
                    self.reclaim_tracker.touch_blocks(
                        current_block..current_block + Ext4Bid::from(blocks_in_group),
                    );
                }
            }
            current_block = current_block
                .checked_add(Ext4Bid::from(blocks_in_group))
                .ok_or_else(|| Error::with_message(Errno::EOVERFLOW, "block range overflow"))?;
            remaining_blocks -= blocks_in_group;
        }

        Ok(())
    }

    /// Allocates and initializes a new inode, returning the live `Arc<Inode>`.
    ///
    /// Allocates an inode number, builds a fresh [`InodeDesc`] (an empty extent
    /// root with the `EXTENTS` flag set, size/`i_blocks` 0, owners from the
    /// caller's fsuid/fsgid, `now` timestamps, and a monotonic generation), and
    /// writes the full on-disk inode. On failure, the allocation is rolled back.
    pub(super) fn create_inode(
        &self,
        parent_ino: Ext4Ino,
        type_: InodeType,
        perm: FilePerm,
    ) -> Result<Arc<Inode>> {
        let ino = self.alloc_ino(parent_ino, type_)?;
        if self.nv_state.is_some()
            && let Err(err) = self.sync_metadata()
        {
            let _ = self.free_inode(ino, type_);
            return Err(err);
        }

        let link_count = if type_.is_directory() { 2 } else { 1 };
        let (uid, gid) = Thread::current()
            .and_then(|thread| {
                thread
                    .as_posix_thread()
                    .map(|posix_thread| posix_thread.credentials())
            })
            .map(|credentials| {
                (
                    u32::from(credentials.fsuid()),
                    u32::from(credentials.fsgid()),
                )
            })
            .unwrap_or((0, 0));
        let now = utils::now();
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        // New inodes follow the volume's mapping format: extent volumes get
        // extent roots, ext2-format volumes get indirect pointer arrays, so a
        // volume written by this driver keeps its original format. Special
        // files are excepted: their `i_block` holds the device encoding (or
        // nothing), never a mapping root, so they must not carry the `EXTENTS`
        // flag even on an extent volume (Linux does the same).
        let extent_based = matches!(type_, InodeType::File | InodeType::Dir | InodeType::SymLink)
            && self
                .super_block
                .read()
                .feature_incompat()
                .contains(FeatureIncompatSet::EXTENTS);
        let inode_desc = InodeDesc::new(
            type_,
            perm,
            uid,
            gid,
            link_count,
            generation,
            now,
            extent_based,
        );

        if let Err(err) = self.write_new_inode_desc(ino, &inode_desc) {
            // Roll back the inode allocation: clear the bitmap bit and restore
            // the superblock free-inode counter.
            if let Err(free_err) = self.free_inode(ino, type_) {
                error!("create_inode: rollback free_inode failed: {:?}", free_err);
            } else if self.nv_state.is_some() {
                let _ = self.sync_metadata();
            }
            return Err(err);
        }

        let block_group_idx = usize::try_from((ino - 1) / self.nr_inodes_per_group)
            .expect("inode group index fits usize");
        let inode = Inode::new(
            ino,
            inode_desc.type_(),
            Dirty::new(inode_desc),
            block_group_idx,
            self.self_ref.clone(),
        )?;
        inode.touch_metadata_group(block_group_idx);
        Ok(inode)
    }

    /// Frees an inode by number, mirroring [`Self::free_blocks`] on the block side.
    pub(super) fn free_inode(&self, ino: Ext4Ino, type_: InodeType) -> Result<()> {
        let _reclaim_mutation = self.reclaim_tracker.lock_mutation();
        self.free_inode_inner(ino, type_, true)
    }

    fn free_inode_inner(&self, ino: Ext4Ino, type_: InodeType, track_reclaim: bool) -> Result<()> {
        let mut sb = self.super_block.write();
        let group = self.find_group(ino)?;
        let local_idx = (ino - 1) % self.nr_inodes_per_group;

        let was_allocated = group.free_inode(local_idx, type_)?;
        if was_allocated {
            sb.inc_free_inodes()?;
            if track_reclaim {
                self.reclaim_tracker.touch(ReclaimObjectId::Inode(ino));
            }
        }

        Ok(())
    }

    /// Submits an asynchronous read of one or more blocks starting at `bid`.
    pub(super) fn read_blocks_async(
        &self,
        bid: Ext4Bid,
        bio_segment: BioSegment,
        complete_fn: Option<BioCompleteFn>,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        self.submit_read_blocks_async(
            bid,
            bio_segment,
            complete_fn,
            io_batch,
            Ext4IoPath::AsyncRead,
        )
    }

    fn submit_read_blocks_async(
        &self,
        bid: Ext4Bid,
        bio_segment: BioSegment,
        complete_fn: Option<BioCompleteFn>,
        io_batch: &mut IoBatch,
        path: Ext4IoPath,
    ) -> Result<()> {
        #[cfg(not(any(feature = "ext4-research-stats", ktest)))]
        let _ = path;
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        let complete_fn = {
            self.io_stats.record_submission(bio_segment.nbytes(), path);
            let callback_stats = self.io_stats.clone();
            Some(Box::new(move |status| {
                callback_stats.record_completion();
                if let Some(complete_fn) = complete_fn {
                    complete_fn(status);
                }
            }) as BioCompleteFn)
        };
        if let Err(err) =
            self.block_device
                .read_blocks_async(Bid::new(bid), bio_segment, complete_fn, io_batch)
        {
            #[cfg(any(feature = "ext4-research-stats", ktest))]
            self.io_stats.record_completion();
            return Err(err.into());
        }
        Ok(())
    }

    /// Reads one or more blocks synchronously starting at `bid`.
    pub(super) fn read_blocks(&self, bid: Ext4Bid, bio_segment: BioSegment) -> Result<()> {
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        self.io_stats
            .record_submission(bio_segment.nbytes(), Ext4IoPath::SyncRead);
        let result = self.block_device.read_blocks(Bid::new(bid), bio_segment);
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        self.io_stats.record_completion();
        let bio_status = result?;
        match bio_status {
            BioStatus::Complete => Ok(()),
            _ => {
                return_errno_with_message!(Errno::EIO, "failed to read blocks from block device")
            }
        }
    }

    /// Submits an asynchronous write of one or more blocks starting at `bid`.
    pub(super) fn write_blocks_async(
        &self,
        bid: Ext4Bid,
        bio_segment: BioSegment,
        complete_fn: Option<BioCompleteFn>,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        self.write_blocks_async_with_path(
            bid,
            bio_segment,
            complete_fn,
            io_batch,
            Ext4IoPath::AsyncWrite,
        )
    }

    pub(super) fn write_blocks_async_with_path(
        &self,
        bid: Ext4Bid,
        bio_segment: BioSegment,
        complete_fn: Option<BioCompleteFn>,
        io_batch: &mut IoBatch,
        path: Ext4IoPath,
    ) -> Result<()> {
        self.write_blocks_async_segments_with_path(
            bid,
            vec![bio_segment],
            complete_fn,
            io_batch,
            path,
        )
    }

    pub(super) fn write_blocks_async_segments_with_path(
        &self,
        bid: Ext4Bid,
        bio_segments: Vec<BioSegment>,
        complete_fn: Option<BioCompleteFn>,
        io_batch: &mut IoBatch,
        path: Ext4IoPath,
    ) -> Result<()> {
        #[cfg(not(any(feature = "ext4-research-stats", ktest)))]
        let _ = path;
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        let complete_fn = {
            let nbytes = bio_segments.iter().map(BioSegment::nbytes).sum();
            self.io_stats.record_submission(nbytes, path);
            let callback_stats = self.io_stats.clone();
            Some(Box::new(move |status| {
                callback_stats.record_completion();
                if let Some(complete_fn) = complete_fn {
                    complete_fn(status);
                }
            }) as BioCompleteFn)
        };
        if let Err(err) = self.block_device.write_blocks_async_segments(
            Bid::new(bid),
            bio_segments,
            complete_fn,
            io_batch,
        ) {
            #[cfg(any(feature = "ext4-research-stats", ktest))]
            self.io_stats.record_completion();
            return Err(err.into());
        }
        Ok(())
    }

    /// Writes one or more blocks synchronously starting at `bid`.
    pub(super) fn write_blocks(&self, bid: Ext4Bid, bio_segment: BioSegment) -> Result<()> {
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        self.io_stats
            .record_submission(bio_segment.nbytes(), Ext4IoPath::SyncWrite);
        let result = self.block_device.write_blocks(Bid::new(bid), bio_segment);
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        self.io_stats.record_completion();
        let bio_status = result?;
        match bio_status {
            BioStatus::Complete => Ok(()),
            _ => {
                return_errno_with_message!(Errno::EIO, "failed to write blocks to block device")
            }
        }
    }

    /// Flushes every cached inode together with the block-side metadata.
    ///
    /// Order matters for on-disk consistency: each group's cached inodes (their
    /// data pages + inode-table descriptors) are flushed first, then the dirty
    /// bitmaps/GDT/superblock. Flushing inodes before the bitmap keeps the
    /// on-disk extents and the block bitmap mutually consistent — otherwise a
    /// truncate that freed blocks in the bitmap could be persisted while the
    /// inode still on disk references those (now free) blocks, which `e2fsck`
    /// reports as corruption.
    ///
    /// Inode flushing never holds a group's `inode_cache` lock across the sync
    /// (see [`BlockGroup::sync_inodes`]), so the only locks held in sequence are
    /// `inode.inner.write()` then, later, `super_block.write()` + per-group
    /// `metadata.write()` — no inversion.
    pub(super) fn sync_all(&self) -> Result<()> {
        for group in &self.block_groups {
            group.sync_inodes()?;
        }
        self.sync_metadata()?;
        self.commit_journal()?;
        #[cfg(feature = "ext4-research-stats")]
        {
            let stats = self.io_stats.snapshot_and_reset();
            warn!(
                "[ext4-research] io_stats total_bios={} read_bios={} write_bios={} four_kib_bios={} total_bytes={} current_in_flight={} peak_in_flight={} sync_bios={} async_bios={} extent_writeback_bios={} writeback_inode_visits={} max_bio_bytes={}",
                stats.total_bios,
                stats.read_bios,
                stats.write_bios,
                stats.four_kib_bios,
                stats.total_bytes,
                stats.current_in_flight,
                stats.peak_in_flight,
                stats.sync_bios,
                stats.async_bios,
                stats.extent_writeback_bios,
                stats.writeback_inode_visits,
                stats.max_bio_bytes,
            );
            warn!(
                "[ext4-research] sync_stats sync_all_calls={} sync_data_calls={} sync_all_ns={} sync_data_ns={} xattr_ns={} global_metadata_ns={} inode_lock_wait_ns={} data_pages_ns={} inode_desc_ns={} device_flush_ns={} scanned_groups={}",
                stats.sync_all_calls,
                stats.sync_data_calls,
                stats.sync_all_ns,
                stats.sync_data_ns,
                stats.xattr_ns,
                stats.global_metadata_ns,
                stats.inode_lock_wait_ns,
                stats.data_pages_ns,
                stats.inode_desc_ns,
                stats.device_flush_ns,
                stats.scanned_groups,
            );
        }
        Ok(())
    }

    pub(super) fn report_writeback_pipeline_stats(&self) {
        #[cfg(feature = "ext4-research-stats")]
        {
            let writeback = self.writeback_controller.stats_snapshot_and_reset();
            let completion = writeback.completion;
            let average_inflight_bios_milli = completion
                .inflight_bio_sum
                .saturating_mul(1000)
                .checked_div(completion.inflight_bio_samples)
                .unwrap_or(0);
            let average_inflight_pages_milli = completion
                .inflight_page_sum
                .saturating_mul(1000)
                .checked_div(completion.inflight_bio_samples)
                .unwrap_or(0);
            warn!(
                "[ext4-research] writeback_pipeline submitted_bios={} submitted_pages={} completed_bios={} completed_pages={} avg_inflight_bios_milli={} avg_inflight_pages_milli={} peak_inflight_bios={} peak_inflight_pages={} completion_queue_hwm={} credit_stalls={} foreground_throttle_stalls={} post_io_jobs={} coalesced_jobs={} device_failures={} post_io_failures={}",
                completion.submitted_bios,
                completion.submitted_pages,
                completion.completed_bios,
                completion.completed_pages,
                average_inflight_bios_milli,
                average_inflight_pages_milli,
                completion.peak_inflight_bios,
                completion.peak_inflight_pages,
                completion.completion_queue_hwm,
                completion.credit_stalls,
                writeback.foreground_throttle_stalls,
                completion.post_io_jobs,
                completion.coalesced_jobs,
                completion.device_failures,
                completion.post_io_failures,
            );
            if let Some(journal) = self.journal() {
                let stats = journal.stats_snapshot();
                warn!(
                    "[ext4-research] jbd2_stats captured_blocks={} committed_transactions={} committed_blocks={} checkpoint_blocks={} barriers={} aborted={}",
                    stats.captured_blocks,
                    stats.committed_transactions,
                    stats.committed_blocks,
                    stats.checkpoint_blocks,
                    stats.barriers,
                    stats.aborted,
                );
            }
        }
    }

    /// Allocates one inode, preferring the group that owns `parent_ino`.
    ///
    /// Searches groups in a ring starting from the parent's group. Returns the
    /// global inode number on success, or `ENOSPC` if no group has a free inode.
    pub(super) fn alloc_ino(&self, parent_ino: Ext4Ino, type_: InodeType) -> Result<Ext4Ino> {
        if type_ == InodeType::Unknown {
            return_errno_with_message!(Errno::EINVAL, "cannot allocate inode with unknown type");
        }

        let _reclaim_mutation = self.reclaim_tracker.lock_mutation();
        let mut sb = self.super_block.write();
        let nr_block_groups = usize::try_from(sb.nr_block_groups())
            .map_err(|_| Error::with_message(Errno::EOVERFLOW, "block group count overflow"))?;
        let nr_inodes_per_group = sb.nr_inodes_per_group();
        let total_inodes = sb.total_inodes();
        if parent_ino < ROOT_INO || parent_ino > total_inodes {
            return_errno_with_message!(Errno::EIO, "parent inode number out of range");
        }
        if sb.free_inodes_count() == 0 {
            return_errno_with_message!(Errno::ENOSPC, "no free inodes on device");
        }

        let parent_group = usize::try_from((parent_ino - 1) / nr_inodes_per_group)
            .expect("inode group index fits usize");
        for group_search_offset in 0..nr_block_groups {
            let group_idx = (parent_group + group_search_offset) % nr_block_groups;
            let group = &self.block_groups[group_idx];

            let Some(local_idx) = group.alloc_ino(type_)? else {
                continue;
            };

            let group_idx = u32::try_from(group_idx)
                .map_err(|_| Error::with_message(Errno::EOVERFLOW, "inode group index overflow"))?;
            let ino = group_idx
                .checked_mul(nr_inodes_per_group)
                .and_then(|base| base.checked_add(local_idx))
                .and_then(|base| base.checked_add(1))
                .ok_or_else(|| Error::with_message(Errno::EOVERFLOW, "inode number overflow"))?;
            if ino < sb.first_ino() || ino > total_inodes {
                // Roll back the group-level allocation before erroring out.
                let _ = group.free_inode(local_idx, type_);
                return_errno_with_message!(Errno::EIO, "allocated inode number out of valid range");
            }
            sb.dec_free_inodes()?;

            self.reclaim_tracker.touch(ReclaimObjectId::Inode(ino));

            return Ok(ino);
        }

        return_errno_with_message!(Errno::ENOSPC, "no free inodes available in any group");
    }

    /// Returns the block group that owns `ino`.
    fn find_group(&self, ino: Ext4Ino) -> Result<&BlockGroup> {
        if ino == 0 {
            return_errno_with_message!(Errno::ENOENT, "invalid inode number 0");
        }
        let group_idx = usize::try_from((ino - 1) / self.nr_inodes_per_group)
            .expect("inode group index fits usize");
        self.block_groups
            .get(group_idx)
            .ok_or_else(|| Error::with_message(Errno::ENOENT, "inode block group out of range"))
    }

    /// Checks whether the current caller may allocate blocks.
    ///
    /// Non-privileged users are denied when free blocks fall below the reserved
    /// threshold, unless they have `CAP_SYS_RESOURCE` or match `s_resuid`/`s_resgid`.
    fn can_alloc(
        free_blocks: u64,
        reserved_blocks: u64,
        reserved_uid: u32,
        reserved_gid: u32,
    ) -> bool {
        if free_blocks > reserved_blocks {
            return true;
        }

        let Some(thread) = Thread::current() else {
            return true;
        };
        let Some(posix_thread) = thread.as_posix_thread() else {
            return true;
        };

        if lsm_hooks::on_capable(lsm_hooks::CapableContext::new(
            UserNamespace::get_init_singleton().as_ref(),
            posix_thread,
            CapSet::SYS_RESOURCE,
        ))
        .is_ok()
        {
            return true;
        }

        let credentials = posix_thread.credentials();
        if u32::from(credentials.fsuid()) == reserved_uid {
            return true;
        }

        let reserved_gid_value = Gid::from(reserved_gid);
        if !reserved_gid_value.is_root() {
            if credentials.fsgid() == reserved_gid_value {
                return true;
            }
            if credentials.groups().contains(&reserved_gid_value) {
                return true;
            }
        }

        false
    }

    /// Writes back the superblock and every dirty group descriptor/bitmap.
    pub(super) fn sync_metadata(&self) -> Result<()> {
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        let start = sync_clock_now();
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            self.io_stats.record_scanned_groups(self.block_groups.len());
        }
        let result = (|| {
            self.metadata_cache.sync_all()?;
            for group in &self.block_groups {
                group.sync_metadata()?;
            }

            let mut sb = self.super_block.write();
            if sb.is_dirty() {
                // RMW the on-disk superblock: patch only the free-block and
                // free-inode counters so every other on-disk field is preserved
                // losslessly.
                let free_blocks = u32::try_from(sb.free_blocks_count()).map_err(|_| {
                    Error::with_message(Errno::EOVERFLOW, "free block count exceeds disk field")
                })?;
                let free_inodes = sb.free_inodes_count();
                let feature_incompat = sb.feature_incompat().bits();
                let wtime = utils::now().into();
                if let Some(nv_state) = &self.nv_state {
                    nv_state.update_meta_at::<RawSuperBlock>(SUPER_BLOCK_OFFSET, |raw| {
                        raw.free_blocks_count = free_blocks;
                        raw.free_inodes_count = free_inodes;
                        raw.feature_incompat = feature_incompat;
                        raw.wtime = wtime;
                        Ok(())
                    })?;
                } else {
                    // Conventional nojournal mounts write this cache block to
                    // its home location. Journal mounts capture the same
                    // after-image and publish it with the enclosing commit.
                    self.sync_superblock_snapshot(&sb)?;
                }
                sb.clear_dirty();
            }

            Ok(())
        })();
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            let elapsed = sync_clock_now().saturating_sub(start).as_nanos();
            self.io_stats.record_sync_stage(
                Ext4SyncStage::GlobalMetadata,
                usize::try_from(elapsed).unwrap_or(usize::MAX),
            );
        }
        result
    }
}

/// Test-only rollback guard for blocks allocated through [`Ext4::alloc_blocks`].
#[cfg(ktest)]
pub(super) struct FsBlockAllocGuard<'a> {
    fs: &'a Ext4,
    ranges: Vec<Range<Ext4Bid>>,
}

#[cfg(ktest)]
impl<'a> FsBlockAllocGuard<'a> {
    /// Creates a guard tracking no ranges yet.
    pub(super) fn new(fs: &'a Ext4) -> Self {
        Self {
            fs,
            ranges: Vec::new(),
        }
    }

    /// Records an allocated range to be rolled back on drop.
    pub(super) fn extend(&mut self, range: Range<Ext4Bid>) {
        if !range.is_empty() {
            self.ranges.push(range);
        }
    }
}

#[cfg(ktest)]
impl Drop for FsBlockAllocGuard<'_> {
    fn drop(&mut self) {
        for range in self.ranges.iter() {
            let count = (range.end - range.start) as u32;
            if let Err(err) = self.fs.free_blocks(range.start, count) {
                error!(
                    "FsBlockAllocGuard: failed to free range {:?} in rollback: {:?}",
                    range, err
                );
            }
        }
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{
        super::test_utils::{Ext4FixtureBuilder, make_empty_file_inode},
        *,
    };
    use crate::{
        fs::vfs::file_system::FileSystem as FileSystemTrait, thread::kernel_thread::ThreadOptions,
    };

    const SECTORS_PER_BLOCK: u64 = (BLOCK_SIZE / SECTOR_SIZE) as u64;

    fn write_all(inode: &Inode, offset: usize, data: &[u8]) {
        let mut reader = VmReader::from(data).to_fallible();
        let n = inode.write_at(offset, &mut reader).unwrap();
        assert_eq!(n, data.len());
    }

    /// Returns whether physical block `pblock` is marked allocated in group 0.
    fn block_is_allocated(f: &super::super::test_utils::Ext4Fixture, pblock: Ext4Bid) -> bool {
        let group = f.ext4.block_group(0);
        group
            .metadata()
            .block_bitmap
            .is_allocated((pblock - group.first_block()) as u16)
    }

    /// Returns the physical block mapped by a depth-0 extent root.
    fn ondisk_pblock_of(raw: &RawInode, lblock: u32) -> Option<Ext4Bid> {
        let entries = (raw.block[0] >> 16) & 0xFFFF;
        let depth = (raw.block[1] >> 16) & 0xFFFF;
        if depth != 0 {
            return None;
        }
        for i in 0..entries as usize {
            let ee_block = raw.block[3 + i * 3];
            let raw_len = raw.block[4 + i * 3] & 0xFFFF;
            let len = if raw_len > 32768 {
                raw_len - 32768
            } else {
                raw_len
            };
            let ee_start = raw.block[5 + i * 3] as Ext4Bid;
            if lblock >= ee_block && lblock < ee_block + len {
                return Some(ee_start + (lblock - ee_block) as Ext4Bid);
            }
        }
        None
    }

    #[ktest]
    fn block_alloc_guard_rolls_back_on_drop() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let before_sb_free = f.ext4.super_block().free_blocks_count();
        let before_group_free = f.ext4.block_group(0).free_blocks_count();

        let range = f
            .ext4
            .alloc_blocks(4, f.ext4.block_group(0).first_block())
            .unwrap();
        let alloc_len = (range.end - range.start) as u32;
        assert!(alloc_len > 0);

        {
            let mut guard = FsBlockAllocGuard::new(&f.ext4);
            guard.extend(range.clone());
            // Drop without commit -> rollback.
        }

        // Counts restored and bitmap bits cleared.
        assert_eq!(f.ext4.super_block().free_blocks_count(), before_sb_free);
        assert_eq!(f.ext4.block_group(0).free_blocks_count(), before_group_free);
        let group = f.ext4.block_group(0);
        let metadata = group.metadata();
        for bid in range.clone() {
            let bit = (bid - group.first_block()) as u16;
            assert!(!metadata.block_bitmap.is_allocated(bit));
        }
    }

    #[ktest]
    fn block_allocator_honors_goal_and_refreshes_after_free() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let goal = f.ext4.block_group(0).first_block() + 100;

        let first = f.ext4.alloc_blocks(4, goal).unwrap();
        assert_eq!(first, goal..goal + 4);
        f.ext4.free_blocks(first.start, 4).unwrap();

        let second = f.ext4.alloc_blocks(4, goal).unwrap();
        assert_eq!(second, first);
    }

    #[ktest]
    fn foreground_free_marks_epoch_object_as_touched() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_free_blocks(8)
            .build()
            .unwrap();
        let allocated = f.ext4.alloc_blocks(1, 0).unwrap();
        let block = allocated.start;
        let epoch = f.ext4.begin_reclaim_epoch().unwrap();

        f.ext4.free_blocks(block, 1).unwrap();

        assert!(
            !epoch
                .sweep_candidates(&BTreeSet::new())
                .contains(&ReclaimObjectId::Block(block))
        );
    }

    #[ktest]
    fn io_stats_observe_one_page_and_sixteen_page_cross_inode_writeback() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        f.write_raw_inode(11, &make_empty_file_inode());
        f.write_raw_inode(12, &make_empty_file_inode());

        let one_page = f.ext4.read_inode(11).unwrap();
        let sixteen_pages = f.ext4.read_inode(12).unwrap();
        write_all(&one_page, 0, &[0x11; BLOCK_SIZE]);
        write_all(
            &sixteen_pages,
            0,
            &vec![0x22; 16usize.checked_mul(BLOCK_SIZE).unwrap()],
        );

        let before = f.ext4.io_stats_snapshot();
        one_page.sync_data().unwrap();
        sixteen_pages.sync_data().unwrap();
        let after = f.ext4.io_stats_snapshot();

        assert_eq!(
            after.writeback_inode_visits - before.writeback_inode_visits,
            2
        );
        assert_eq!(
            after.extent_writeback_bios - before.extent_writeback_bios,
            2
        );
        assert!(after.write_bios > before.write_bios);
        assert!(after.total_bytes - before.total_bytes >= 17 * BLOCK_SIZE);
        assert!(after.max_bio_bytes >= 16 * BLOCK_SIZE);
        assert_eq!(f.disk.max_write_segments(), 1);
        assert!(f.disk.max_write_segment_bytes() >= 16 * BLOCK_SIZE);

        let start_pblock = ondisk_pblock_of(&f.read_raw_inode(12), 0).unwrap();
        let mut persisted = vec![0; 16 * BLOCK_SIZE];
        f.disk
            .segment()
            .read_bytes(start_pblock as usize * BLOCK_SIZE, &mut persisted)
            .unwrap();
        assert_eq!(persisted, vec![0x22; 16 * BLOCK_SIZE]);

        assert!(after.peak_in_flight >= 1);
        assert_eq!(after.current_in_flight, 0);
    }

    /// Ensures filesystem sync keeps truncated inodes and block bitmaps consistent.
    #[ktest]
    fn cross_inode_truncate_consistent_after_sync_all() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        f.write_raw_inode(11, &make_empty_file_inode());
        f.write_raw_inode(12, &make_empty_file_inode());

        let a = f.ext4.read_inode(11).unwrap();
        write_all(&a, 0, &[0xAA; BLOCK_SIZE]);
        a.sync_data().unwrap();

        let b = f.ext4.read_inode(12).unwrap();
        write_all(&b, 0, &[0xBB; 3 * BLOCK_SIZE]);
        b.sync_data().unwrap();

        let b2_pblock = ondisk_pblock_of(&f.read_raw_inode(12), 2).unwrap();
        assert!(block_is_allocated(&f, b2_pblock));

        b.resize(BLOCK_SIZE).unwrap();
        assert_eq!(b.file_size(), BLOCK_SIZE);
        f.ext4.sync_all().unwrap();

        let raw_b = f.read_raw_inode(12);
        assert_eq!(raw_b.size_lo, BLOCK_SIZE as u32);
        assert_eq!(raw_b.sector_count as u64, SECTORS_PER_BLOCK);

        let b0_pblock = ondisk_pblock_of(&raw_b, 0).unwrap();
        assert!(block_is_allocated(&f, b0_pblock));
        assert!(ondisk_pblock_of(&raw_b, 2).is_none());
        assert!(!block_is_allocated(&f, b2_pblock));

        let a0_pblock = ondisk_pblock_of(&f.read_raw_inode(11), 0).unwrap();
        assert!(block_is_allocated(&f, a0_pblock));
    }

    /// create_inode rolls back the inode allocation when the on-disk writeback
    /// fails: the bitmap bit is cleared and the superblock counter restored.
    #[ktest]
    fn create_inode_rolls_back_on_write_failure() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_inode_bitmap_metadata_marked()
            .build()
            .unwrap();

        let before_sb = f.ext4.super_block().free_inodes_count();
        let before_group = f.ext4.block_group(0).free_inodes_count();

        // Force the inode writeback to fail, so create_inode must roll back.
        f.disk.set_fail_writes(true);
        let perm = FilePerm::from_bits_truncate(0o644);
        // `Arc<Inode>` is not `Debug`, so match instead of `unwrap_err`.
        let err = match f.ext4.create_inode(ROOT_INO, InodeType::File, perm) {
            Ok(_) => panic!("create_inode unexpectedly succeeded despite write failure"),
            Err(err) => err,
        };
        assert_eq!(err.error(), Errno::EIO);
        f.disk.set_fail_writes(false);

        // Allocation fully rolled back: counters restored and the freshly taken
        // bit (group-local index 10, i.e. ino 11) is clear again.
        assert_eq!(f.ext4.super_block().free_inodes_count(), before_sb);
        assert_eq!(f.ext4.block_group(0).free_inodes_count(), before_group);
        let group = f.ext4.block_group(0);
        assert!(!group.metadata().inode_bitmap.is_allocated(10));

        let inode = f
            .ext4
            .create_inode(ROOT_INO, InodeType::File, perm)
            .unwrap();
        assert_eq!(inode.ino(), 11);
    }

    /// Verifies that a 128-byte inode read does not consume the next slot.
    fn verify_128_byte_inode_read() {
        use super::super::test_utils::make_indirect_file_inode;

        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .inode_size(128)
            .build()
            .unwrap();

        let mut raw = make_indirect_file_inode(100, 42);
        raw.atime = 1234;
        f.write_raw_inode(20, &raw);
        // Fill the next slot (bytes 128..256 after inode 20's start) with a
        // pattern a fixed 256-byte read would misdecode as the extra area.
        let neighbor_offset = 4 * BLOCK_SIZE + 20 * 128;
        f.disk
            .segment()
            .write_bytes(neighbor_offset, &[0xFF; 128])
            .unwrap();

        let desc = f.ext4.block_group(0).read_inode_desc(20).unwrap();
        assert_eq!(desc.atime().as_secs(), 1234);
        assert_eq!(desc.atime().subsec_nanos(), 0);
        assert_eq!(desc.crtime(), Duration::ZERO);
    }

    /// Verifies that a 256-byte inode write preserves unmodeled tail bytes.
    fn verify_256_byte_inode_write() {
        use super::super::test_utils::make_indirect_file_inode;

        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();

        let mut raw = make_indirect_file_inode(100, 42);
        raw.tail.0[0] = 0xAB;
        raw.tail.0[7] = 0xCD;
        f.write_raw_inode(20, &raw);

        let inode = f.ext4.read_inode(20).unwrap();
        inode.set_mtime(Duration::from_secs(7777));
        inode.sync_data().unwrap();

        let after = f.read_raw_inode(20);
        assert_eq!(after.tail.0[0], 0xAB);
        assert_eq!(after.tail.0[7], 0xCD);
        assert_eq!(after.mtime, 7777);
    }

    #[ktest]
    fn inode_slot_io_honors_inode_size() {
        verify_128_byte_inode_read();
        verify_256_byte_inode_write();
    }

    #[ktest]
    fn inode_table_block_read_is_reused() {
        use super::super::test_utils::make_indirect_file_inode;

        for inode_size in [128, 256] {
            let f = Ext4FixtureBuilder::new(2048, 256, 2048)
                .inode_size(inode_size)
                .build()
                .unwrap();
            f.write_raw_inode(20, &make_indirect_file_inode(100, 42));
            f.write_raw_inode(21, &make_indirect_file_inode(101, 43));

            let reads_before = f.disk.read_count();
            f.ext4.block_group(0).read_inode_desc(20).unwrap();
            let reads_after_first = f.disk.read_count();
            f.ext4.block_group(0).read_inode_desc(21).unwrap();
            let reads_after_second = f.disk.read_count();

            assert_eq!(reads_after_first - reads_before, 1);
            assert_eq!(reads_after_second, reads_after_first);
        }
    }

    #[ktest]
    fn metadata_cache_evicts_an_unpinned_clean_block_at_capacity() {
        use super::super::test_utils::make_indirect_file_inode;

        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let first_ino = 17;
        f.write_raw_inode(first_ino, &make_indirect_file_inode(100, 42));
        for table_block_idx in 2..=9 {
            let ino = table_block_idx * 16 + 1;
            f.write_raw_inode(ino, &make_indirect_file_inode(100 + table_block_idx, 42));
        }

        let reads_before = f.disk.read_count();
        f.ext4.block_group(0).read_inode_desc(first_ino).unwrap();
        for table_block_idx in 2..=9 {
            f.ext4
                .block_group(0)
                .read_inode_desc(table_block_idx * 16 + 1)
                .unwrap();
        }
        let reads_after_fill = f.disk.read_count();
        f.ext4.block_group(0).read_inode_desc(first_ino).unwrap();

        assert_eq!(reads_after_fill - reads_before, 9);
        assert_eq!(f.disk.read_count() - reads_after_fill, 1);
    }

    #[ktest]
    fn inode_table_updates_coalesce_until_explicit_writeback() {
        use super::super::test_utils::make_indirect_file_inode;

        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        f.write_raw_inode(20, &make_indirect_file_inode(100, 42));

        let mut desc = f.ext4.block_group(0).read_inode_desc(20).unwrap();
        let reads_after_load = f.disk.read_count();
        let writes_before = f.disk.write_count();

        desc.set_mtime(Duration::from_secs(1000));
        let root = *desc.raw_block();
        let first = f
            .ext4
            .block_group(0)
            .update_inode_desc(20, |raw| raw.update_from_desc(&desc, &root))
            .unwrap();
        desc.set_mtime(Duration::from_secs(2000));
        let second = f
            .ext4
            .block_group(0)
            .update_inode_desc(20, |raw| raw.update_from_desc(&desc, &root))
            .unwrap();

        assert_eq!(f.disk.read_count(), reads_after_load);
        assert_eq!(f.disk.write_count(), writes_before);
        assert_eq!(f.read_raw_inode(20).mtime, 0);

        f.ext4.sync_metadata_block(&first).unwrap();
        f.ext4.sync_metadata_block(&second).unwrap();

        assert_eq!(f.disk.write_count() - writes_before, 1);
        assert_eq!(f.read_raw_inode(20).mtime, 2000);
    }

    #[ktest]
    fn inode_table_cached_update_survives_remount() {
        use super::super::test_utils::make_indirect_file_inode;

        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        f.write_raw_inode(20, &make_indirect_file_inode(100, 42));

        let mut desc = f.ext4.block_group(0).read_inode_desc(20).unwrap();
        desc.set_mtime(Duration::from_secs(2000));
        let root = *desc.raw_block();
        f.ext4.write_back_inode_desc(20, &desc, &root).unwrap();

        let remounted = Ext4::open(
            f.disk.clone() as Arc<dyn BlockDevice>,
            FsFlags::empty(),
            MountFlavor::Ext4,
            None,
        )
        .unwrap();
        assert_eq!(
            remounted
                .block_group(0)
                .read_inode_desc(20)
                .unwrap()
                .mtime(),
            Duration::from_secs(2000)
        );
    }

    #[ktest]
    fn failed_inode_table_writeback_keeps_dirty_cached_slot() {
        use super::super::test_utils::make_indirect_file_inode;

        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        f.write_raw_inode(20, &make_indirect_file_inode(100, 42));

        let mut desc = f.ext4.block_group(0).read_inode_desc(20).unwrap();
        desc.set_mtime(Duration::from_secs(1000));
        let root = *desc.raw_block();

        f.disk.set_fail_writes(true);
        let dependency = f
            .ext4
            .block_group(0)
            .update_inode_desc(20, |raw| raw.update_from_desc(&desc, &root))
            .unwrap();
        assert!(f.ext4.sync_metadata_block(&dependency).is_err());
        f.disk.set_fail_writes(false);

        let cached = f.ext4.block_group(0).read_inode_desc(20).unwrap();
        assert_eq!(cached.mtime(), Duration::from_secs(1000));
        assert_eq!(f.read_raw_inode(20).mtime, 0);

        f.ext4.sync_metadata_block(&dependency).unwrap();
        assert_eq!(f.read_raw_inode(20).mtime, 1000);
    }

    #[ktest]
    fn global_metadata_sync_retries_dirty_cached_blocks() {
        use super::super::test_utils::make_indirect_file_inode;

        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        f.write_raw_inode(20, &make_indirect_file_inode(100, 42));

        let mut desc = f.ext4.block_group(0).read_inode_desc(20).unwrap();
        desc.set_mtime(Duration::from_secs(1000));
        let root = *desc.raw_block();
        let dependency = f
            .ext4
            .block_group(0)
            .update_inode_desc(20, |raw| raw.update_from_desc(&desc, &root))
            .unwrap();

        f.disk.set_fail_writes(true);
        assert!(f.ext4.sync_metadata_block(&dependency).is_err());
        f.disk.set_fail_writes(false);

        f.ext4.sync_metadata().unwrap();
        assert_eq!(f.read_raw_inode(20).mtime, 1000);
    }

    #[ktest]
    fn concurrent_inode_table_writes_preserve_neighboring_slots() {
        use super::super::test_utils::make_indirect_file_inode;

        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        f.write_raw_inode(20, &make_indirect_file_inode(100, 42));
        f.write_raw_inode(21, &make_indirect_file_inode(101, 43));

        let mut first = f.ext4.block_group(0).read_inode_desc(20).unwrap();
        let mut second = f.ext4.block_group(0).read_inode_desc(21).unwrap();
        first.set_mtime(Duration::from_secs(1000));
        second.set_mtime(Duration::from_secs(2000));
        let first_root = *first.raw_block();
        let second_root = *second.raw_block();
        let first_result = Arc::new(Mutex::new(None));
        let second_result = Arc::new(Mutex::new(None));

        let first_thread = {
            let ext4 = f.ext4.clone();
            let result = first_result.clone();
            ThreadOptions::new(move || {
                *result.lock() = Some(
                    ext4.block_group(0)
                        .update_inode_desc(20, |raw| raw.update_from_desc(&first, &first_root)),
                );
            })
            .spawn()
        };
        let second_thread = {
            let ext4 = f.ext4.clone();
            let result = second_result.clone();
            ThreadOptions::new(move || {
                *result.lock() = Some(
                    ext4.block_group(0)
                        .update_inode_desc(21, |raw| raw.update_from_desc(&second, &second_root)),
                );
            })
            .spawn()
        };

        first_thread.join();
        second_thread.join();
        let first_dependency = first_result.lock().take().unwrap().unwrap();
        let second_dependency = second_result.lock().take().unwrap().unwrap();
        f.ext4.sync_metadata_block(&first_dependency).unwrap();
        f.ext4.sync_metadata_block(&second_dependency).unwrap();
        assert_eq!(f.read_raw_inode(20).mtime, 1000);
        assert_eq!(f.read_raw_inode(21).mtime, 2000);
    }

    /// Verifies the ext2/ext4 mount-flavor feature matrix.
    #[ktest]
    fn mount_flavor_decision_matrix() {
        Ext4FixtureBuilder::new(2048, 256, 2048)
            .without_extents_feature()
            .build()
            .unwrap();
        Ext4FixtureBuilder::new(2048, 256, 2048)
            .without_extents_feature()
            .with_flavor(MountFlavor::Ext2)
            .build()
            .unwrap();

        let err = match Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_flavor(MountFlavor::Ext2)
            .build()
        {
            Err(err) => err,
            Ok(_) => panic!("an extent volume must not mount as ext2"),
        };
        assert_eq!(err.error(), Errno::EINVAL);

        Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_journal(200, 8, 0)
            .build()
            .unwrap();
        let err = match Ext4FixtureBuilder::new(2048, 256, 2048)
            .without_extents_feature()
            .with_extra_compat(0x4)
            .with_flavor(MountFlavor::Ext2)
            .build()
        {
            Err(err) => err,
            Ok(_) => panic!("a journaled volume must not mount as ext2"),
        };
        assert_eq!(err.error(), Errno::EINVAL);

        Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_extra_ro_compat(0x4)
            .build()
            .unwrap();
    }

    /// An ext2-format volume (no EXTENTS feature) mounts, creates ext2-format
    /// inodes (DR-9), and serves buffered write/read/shrink through the
    /// indirect engine end to end.
    #[ktest]
    fn ext2_format_volume_end_to_end() {
        use super::super::inode::EXTENTS_FL;

        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .without_extents_feature()
            .with_block_bitmap_metadata_marked()
            .with_inode_bitmap_metadata_marked()
            .build()
            .unwrap();

        let inode = f
            .ext4
            .create_inode(
                ROOT_INO,
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        let ino = inode.ino();

        // The new inode is pure ext2 format on disk (`create_inode` writes the
        // fresh slot immediately): no `EXTENTS` flag and an all-zero pointer
        // array, so an ext2 driver could still mount the volume.
        let raw = f.read_raw_inode(ino);
        assert_eq!(raw.flags & EXTENTS_FL, 0);
        assert_eq!(raw.block, [0u32; super::super::inode::RAW_BLOCK_PTRS_LEN]);

        // Buffered write and read back through the indirect engine.
        let data: Vec<u8> = (0..BLOCK_SIZE * 3 + 123).map(|i| i as u8).collect();
        let mut reader = VmReader::from(&data[..]).to_fallible();
        assert_eq!(inode.write_at(0, &mut reader).unwrap(), data.len());
        let mut buf = vec![0u8; data.len()];
        let mut writer = VmWriter::from(buf.as_mut_slice()).to_fallible();
        assert_eq!(inode.read_at(0, &mut writer).unwrap(), data.len());
        assert_eq!(buf, data);

        // Shrinking frees the tail blocks and persists direct pointers.
        inode.resize(BLOCK_SIZE).unwrap();
        inode.sync_data().unwrap();
        let raw = f.read_raw_inode(ino);
        assert_ne!(raw.block[0], 0);
        assert_eq!(raw.block[1], 0);
    }

    fn expected_overhead_blocks(fs: &Ext4) -> u64 {
        let sb = fs.super_block();
        let nr_block_groups = sb.nr_block_groups() as usize;
        let gdb_count =
            ((nr_block_groups * size_of::<RawBlockGroup>()).div_ceil(BLOCK_SIZE)) as u64;
        let mut overhead = sb.first_data_block();

        for group_idx in 0..nr_block_groups {
            if group_idx == 0 || sb.is_backup_group(group_idx) {
                overhead += 1 + gdb_count;
            }
        }

        overhead
            + u64::from(sb.nr_block_groups())
                * (2 + u64::from(sb.nr_inode_table_blocks_per_group()))
    }

    /// The default (`bsddf`) accounting subtracts metadata overhead from the
    /// reported total block count.
    #[ktest]
    fn stat_bsddf_subtracts_overhead() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();

        let stat = FileSystemTrait::sb(f.ext4.as_ref());
        let expected_overhead = expected_overhead_blocks(&f.ext4);

        assert_eq!(
            stat.blocks,
            (f.ext4.super_block().total_blocks() - expected_overhead) as usize
        );
        assert!(stat.blocks < f.ext4.super_block().total_blocks() as usize);
    }

    /// The `minixdf` mount option reports the raw total block count.
    #[ktest]
    fn stat_minixdf_reports_total() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();

        let ext4 = Ext4::open(
            f.disk.clone() as Arc<dyn BlockDevice>,
            FsFlags::empty(),
            MountFlavor::Ext4,
            Some("minixdf"),
        )
        .unwrap();

        let stat = FileSystemTrait::sb(ext4.as_ref());
        assert_eq!(stat.blocks, f.ext4.super_block().total_blocks() as usize);
    }

    #[ktest]
    fn delalloc_mount_option_defaults_on_and_last_value_wins() {
        assert!(ExtMountOptions::parse(None).delalloc);
        assert!(!ExtMountOptions::parse(Some("nodelalloc")).delalloc);
        assert!(ExtMountOptions::parse(Some("nodelalloc,delalloc")).delalloc);
        assert!(!ExtMountOptions::parse(Some("delalloc,nodelalloc")).delalloc);
    }

    #[ktest]
    fn writeback_capacity_mount_option_sets_an_explicit_high_watermark() {
        assert_eq!(
            ExtMountOptions::parse(Some("writeback_capacity_mb=128")).writeback_capacity_bytes,
            Some(128 * 1024 * 1024)
        );
        assert_eq!(
            ExtMountOptions::parse(Some("writeback_capacity_mb=invalid")).writeback_capacity_bytes,
            None
        );
    }

    #[ktest]
    fn data_and_extent_metadata_reservations_protect_each_other() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_free_blocks(2)
            .build()
            .unwrap();

        f.ext4.reserve_data_blocks(1).unwrap();
        let mut metadata = f.ext4.reserve_extent_metadata(1).unwrap();
        assert_eq!(f.ext4.reserved_data_blocks(), 1);
        assert_eq!(f.ext4.reserved_metadata_blocks(), 1);
        assert_eq!(
            f.ext4.alloc_blocks(1, 0).unwrap_err().error(),
            Errno::ENOSPC
        );

        f.ext4.alloc_reserved_blocks(1, 0).unwrap();
        metadata.alloc_block(0).unwrap();
        assert_eq!(f.ext4.reserved_data_blocks(), 0);
        assert_eq!(f.ext4.reserved_metadata_blocks(), 0);
    }

    /// `sync_metadata` stamps the primary superblock's write time and leaves
    /// backup superblock copies untouched: the running filesystem maintains
    /// only the primary; backups belong to mkfs/fsck/resize (matching Linux).
    #[ktest]
    fn sync_stamps_wtime_and_leaves_backups_alone() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(1024, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();

        // Plant a sentinel write time on disk; the sync below must overwrite
        // it with the current time (the ktest clock reads as the epoch, so
        // "was patched" is asserted as "no longer the sentinel").
        let sentinel = Duration::from_secs(12345);
        let mut raw = f
            .disk
            .segment()
            .read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)
            .unwrap();
        raw.wtime = sentinel.into();
        f.disk
            .segment()
            .write_val(SUPER_BLOCK_OFFSET, &raw)
            .unwrap();

        // Snapshot the group-1 backup-superblock area before the sync.
        let backup_offset = (f.ext4.super_block().group_first_block_no(1) as usize) * BLOCK_SIZE;
        let mut before = vec![0u8; 1024];
        f.disk
            .segment()
            .read_bytes(backup_offset, &mut before)
            .unwrap();

        // Dirty the superblock (allocation moves the free-block counter).
        let range = f.ext4.alloc_blocks(1, 0).unwrap();
        assert!(!range.is_empty());
        f.ext4.sync_metadata().unwrap();

        let raw = f
            .disk
            .segment()
            .read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)
            .unwrap();
        assert_ne!(Duration::from(raw.wtime), sentinel);

        let mut after = vec![0u8; 1024];
        f.disk
            .segment()
            .read_bytes(backup_offset, &mut after)
            .unwrap();
        assert_eq!(before, after);
    }
}
