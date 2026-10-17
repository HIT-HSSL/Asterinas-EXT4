// SPDX-License-Identifier: MPL-2.0

//! `FileSystem` trait implementation for `Ext4`.

use aster_block::{BLOCK_SIZE, bio::BioStatus};

use crate::{
    fs::{
        fs_impls::ext4::{Ext4, super_block::MAGIC_NUM},
        utils::NAME_MAX,
        vfs::{
            file_system::{FileSystem, FsEventSubscriberStats, FsFlags, SuperBlock},
            inode::Inode,
        },
    },
    prelude::*,
};

impl FileSystem for Ext4 {
    fn name(&self) -> &'static str {
        self.flavor().name()
    }

    fn sync(&self) -> Result<()> {
        // Flush every cached inode together with the block-side metadata. A
        // conventional mount then issues one device barrier; an NVPool mount
        // has already crossed its persistence boundary through NV commits.
        self.writeback_controller().with_submissions_quiesced(|| {
            self.sync_all()?;
            self.writeback_controller().drain()?;
            // NVPool commits are the persistence boundary for sync/syncfs.
            // Only the conventional SSD-authoritative path needs a device
            // barrier here; clean unmount still drains NV state below.
            if !self.uses_nv_pool()
                && !self.uses_journal()
                && self.block_device().sync()? != BioStatus::Complete
            {
                return_errno_with_message!(Errno::EIO, "failed to flush block device");
            }
            self.report_writeback_pipeline_stats();
            Ok(())
        })
    }

    fn drain(&self) -> Result<()> {
        self.writeback_controller().with_submissions_quiesced(|| {
            self.sync_all()?;
            self.writeback_controller().drain()?;
            if let Some(journal) = self.journal() {
                journal.checkpoint_all()?;
            }
            if self.block_device().sync()? != BioStatus::Complete {
                return_errno_with_message!(Errno::EIO, "failed to flush block device");
            }
            if self.uses_nv_pool() {
                self.drain_nv_data()?;
                let nv_state = self
                    .nv_state()
                    .expect("NVPool mount must retain its persistent state");
                nv_state.drain_all_metadata()?;
                self.finish_nv_drain()?;
            }
            self.report_writeback_pipeline_stats();
            Ok(())
        })
    }

    fn root_inode(&self) -> Arc<dyn Inode> {
        self.root_inode().unwrap()
    }

    fn sb(&self) -> SuperBlock {
        let sb = self.super_block();
        let blocks = if self.uses_minix_df() {
            sb.total_blocks()
        } else {
            sb.total_blocks().saturating_sub(sb.total_metadata_blocks())
        };
        SuperBlock {
            magic: MAGIC_NUM as u64,
            bsize: BLOCK_SIZE,
            blocks: blocks as usize,
            bfree: sb.free_blocks_count() as usize,
            bavail: sb
                .free_blocks_count()
                .saturating_sub(sb.reserved_blocks_count()) as usize,
            files: sb.total_inodes() as usize,
            ffree: sb.free_inodes_count() as usize,
            fsid: 0,
            namelen: NAME_MAX,
            frsize: sb.fragment_size(),
            flags: 0,
            container_dev_id: self.container_device_id(),
        }
    }

    fn flags(&self) -> FsFlags {
        self.fs_flags()
    }

    fn set_fs_flags(&self, flags: FsFlags, _data: Option<&str>, _ctx: &Context) -> Result<()> {
        self.set_fs_flags(flags);
        warn!("ext2-specific handling for filesystem flags is not implemented");
        Ok(())
    }

    fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats {
        self.fs_event_subscriber_stats()
    }
}
