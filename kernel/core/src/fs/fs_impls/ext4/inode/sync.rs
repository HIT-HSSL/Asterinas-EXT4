// SPDX-License-Identifier: MPL-2.0

//! Writeback and reclaim for ext4 inodes.

#![short_vis_path::add(ext4)]

#[cfg(any(feature = "ext4-research-stats", ktest))]
use super::super::stats::{Ext4SyncStage, sync_clock_now};
use super::{
    super::{fs::Ext4, prelude::*, utils},
    Inode, InodeInner, InodePayload,
};
use crate::vm::page_cache::{WritebackBatchResult, WritebackMode};

#[cfg(any(feature = "ext4-research-stats", ktest))]
fn elapsed_ns(start: Duration) -> usize {
    usize::try_from(sync_clock_now().saturating_sub(start).as_nanos()).unwrap_or(usize::MAX)
}

impl Inode {
    /// Flushes one bounded data-page window for the mount writeback worker.
    pub(in ext4) fn writeback_one_batch(&self, max_pages: usize) -> Result<WritebackBatchResult> {
        self.fs()?;
        let inner = self.inner.write();
        let file_size = inner.file_size();
        if file_size == 0 {
            return Ok(WritebackBatchResult::empty());
        }
        let result = match &inner.payload {
            InodePayload::DataBacked { page_cache, .. } => page_cache.flush_range_batch_with_mode(
                0..file_size,
                max_pages,
                WritebackMode::Background,
            )?,
            _ => WritebackBatchResult::empty(),
        };
        // Background writeback deliberately leaves a newly allocated mapping
        // dirty in memory. Explicit sync orders its allocation metadata before
        // publishing the inode reference on disk.
        Ok(result)
    }

    /// Flushes all dirty state to the active persistence tier.
    pub(in ext4) fn sync_all(&self) -> Result<()> {
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        let fs = self.fs()?;
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        let sync_start = sync_clock_now();
        let result = (|| {
            #[cfg(any(feature = "ext4-research-stats", ktest))]
            let xattr_start = sync_clock_now();
            let result = self.flush_xattr();
            #[cfg(any(feature = "ext4-research-stats", ktest))]
            {
                fs.io_stats()
                    .record_sync_stage(Ext4SyncStage::Xattr, elapsed_ns(xattr_start));
            }
            result?;

            self.sync_data()
        })();
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            fs.io_stats().record_sync_call(true, elapsed_ns(sync_start));
        }
        result
    }

    /// Flushes dirty file data and the metadata required to retrieve it.
    ///
    /// Conventional mounts finish with an SSD barrier. NVPool mounts return
    /// after their NV commits because later SSD eviction is outside fsync.
    pub(in ext4) fn sync_data(&self) -> Result<()> {
        let fs = self.fs()?;
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        let sync_start = sync_clock_now();
        let result = (|| {
            self.sync_data_no_barrier()?;
            fs.writeback_controller().clear_failure_if_resolved();

            // With NVPool, data, allocation metadata, and the inode root are
            // already durable when their NV commits return. SSD writeback is a
            // later eviction concern and must not extend the fsync boundary.
            if !fs.uses_nv_pool() && !fs.uses_journal() {
                #[cfg(any(feature = "ext4-research-stats", ktest))]
                let flush_start = sync_clock_now();
                let status = fs.block_device().sync();
                #[cfg(any(feature = "ext4-research-stats", ktest))]
                {
                    fs.io_stats()
                        .record_sync_stage(Ext4SyncStage::DeviceFlush, elapsed_ns(flush_start));
                }
                if status? != BioStatus::Complete {
                    return_errno_with_message!(Errno::EIO, "failed to flush block device");
                }
            }
            Ok(())
        })();
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            fs.io_stats()
                .record_sync_call(false, elapsed_ns(sync_start));
        }
        result
    }

    /// Flushes dirty data and inode metadata without a device barrier.
    fn sync_data_no_barrier(&self) -> Result<()> {
        let fs = self.fs()?;
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            fs.io_stats().record_writeback_inode();
        }

        #[cfg(any(feature = "ext4-research-stats", ktest))]
        let lock_start = sync_clock_now();
        let mut inner = self.inner.write();
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            fs.io_stats()
                .record_sync_stage(Ext4SyncStage::InodeLockWait, elapsed_ns(lock_start));
        }

        #[cfg(any(feature = "ext4-research-stats", ktest))]
        let data_start = sync_clock_now();
        let result = inner.sync_data_pages();
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            fs.io_stats()
                .record_sync_stage(Ext4SyncStage::DataPages, elapsed_ns(data_start));
        }
        result?;

        #[cfg(any(feature = "ext4-research-stats", ktest))]
        let inode_start = sync_clock_now();
        let result = self.write_back_with_metadata_dependencies(&fs, &mut inner);
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            fs.io_stats()
                .record_sync_stage(Ext4SyncStage::InodeDesc, elapsed_ns(inode_start));
        }
        result?;
        // All ordered data I/O has completed and every metadata after-image is
        // now captured, so one transaction can publish the whole fsync batch.
        fs.commit_journal()?;
        Ok(())
    }

    /// Flushes the inode's extended-attribute block, if present.
    fn flush_xattr(&self) -> Result<()> {
        match &self.xattr {
            Some(xattr) => xattr.flush(),
            None => Ok(()),
        }
    }

    /// Flushes all inode-owned state without issuing a device barrier.
    pub(in ext4) fn sync_all_no_barrier(&self) -> Result<()> {
        self.flush_xattr()?;
        self.sync_data_no_barrier()
    }

    /// Reclaims the storage owned by a fully unlinked inode.
    pub(super) fn try_reclaim_deleted_inode(&self) -> Result<bool> {
        if self.link_count() != 0 {
            return Ok(false);
        }

        let fs = self.fs()?;
        if !fs.is_inode_allocated(self.ino) {
            return Ok(false);
        }

        if let Some(xattr) = self.xattr.as_ref() {
            let old_bid = xattr.bid();
            xattr.delete_xattr_block()?;
            if old_bid != 0 {
                self.touch_released_metadata_range(&fs, old_bid..old_bid + 1);
            }
        }

        let mut inner = self.inner.write();
        let old_size = inner.file_size();
        // A fast symlink stores its target inline and owns no mapped data blocks.
        let block_manager = inner.block_manager().ok().cloned();
        if block_manager.is_some() {
            inner.resize_page_cache(0, old_size)?;
        }
        inner.set_dtime(utils::now());
        inner.set_file_size(0);
        inner.set_file_acl(0);
        // The mapping engine is authoritative until its accounting is written back.
        if let Some(block_manager) = block_manager
            && block_manager.sector_count() > 0
        {
            block_manager.truncate_to_byte_len(0)?;
        }
        self.write_back_with_metadata_dependencies(&fs, &mut inner)?;

        fs.free_inode(self.ino, self.type_)?;
        self.touch_released_metadata_group(self.block_group_idx);
        Self::sync_metadata_group_set(&self.release_metadata_groups, &fs)?;
        Ok(true)
    }
}

impl InodeInner {
    /// Persists a dirty inode descriptor and clears its dirty state.
    pub(super) fn write_back_inode_desc(&mut self, fs: &Ext4, ino: Ext4Ino) -> Result<()> {
        if !self.is_dirty() {
            return Ok(());
        }
        // Engine-internal metadata blocks must be durable before the inode
        // that references them is written back.
        if let Ok(bm) = self.block_manager() {
            bm.sync_metadata()?;
        }
        let (root, sector_count) = match self.block_manager() {
            Ok(bm) => (bm.root_snapshot(), bm.sector_count()),
            Err(_) => (*self.desc.raw_block(), self.desc.sector_count()),
        };
        // Mirror the mapping engine's authoritative accounting before writeback.
        self.desc.set_sector_count(sector_count);
        fs.write_back_inode_desc(ino, &self.desc, &root)?;
        if let Ok(block_manager) = self.block_manager() {
            block_manager.commit_pending_frees()?;
        }
        self.clear_dirty();
        Ok(())
    }

    /// Flushes dirty data pages in `[0, file_size)`.
    pub(in ext4) fn sync_data_pages(&self) -> Result<()> {
        let file_size = self.file_size();
        if file_size == 0 {
            return Ok(());
        }
        match &self.payload {
            InodePayload::DataBacked { page_cache, .. } => page_cache.flush_range(0..file_size),
            _ => Ok(()),
        }
    }
}
