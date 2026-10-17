// SPDX-License-Identifier: MPL-2.0

//! Regular-file data I/O for ext4 inodes: buffered reads/writes, direct I/O,
//! and fallocate.
//!
//! This module contains the regular-file behavior exposed through the VFS
//! layer: page-cache I/O, direct I/O, size changes, and block preallocation.
//! File size, page-cache state, and ext4 block mappings must stay coherent
//! across those entry points.
//!
//! The direct paths are engine-agnostic: they walk the file as
//! [`BlockMapping::map_blocks`](super::block_mapping::BlockMapping::map_blocks)
//! runs, so extent-mapped and indirect-mapped inodes share one implementation.
//! Coherence with the buffered path is kept per request: a direct read first
//! flushes overlapping dirty pages, and a direct write invalidates overlapping
//! cached pages after allocating its range.

#![short_vis_path::add(ext4)]

use ostd::mm::io::util::HasVmReaderWriter;

use super::{
    super::{fs::Ext4, prelude::*, utils},
    Inode, InodeInner,
    block_mapping::{BufferedWriteReservation, MapState, Mapping, extent::node::TierBlock},
};
use crate::fs::vfs::inode::FallocMode;

impl Inode {
    /// Reads file data at `offset` through the inode's page cache.
    pub(in ext4) fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }
        let read_len = self.inner.read().read_at(offset, writer)?;
        if read_len > 0 {
            self.set_atime(utils::now());
        }
        Ok(read_len)
    }

    /// Writes file data at `offset` through the inode's page cache.
    ///
    /// Allocates blocks for any holes the write covers, fills the page cache,
    /// and updates size and timestamps. Data and the inode become durable on a
    /// later `sync` / writeback.
    pub(in ext4) fn write_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }
        if reader.remain() == 0 {
            return Ok(0);
        }
        offset
            .checked_add(reader.remain())
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "write range overflow"))?;
        let fs = self.fs()?;
        let mut written = 0usize;
        let chunk_bytes = fs.writeback_controller().write_chunk_bytes();
        while reader.has_remain() {
            fs.writeback_controller().balance_dirty_pages()?;
            let chunk_len = reader.remain().min(chunk_bytes);
            let mut chunk_reader = reader.clone();
            chunk_reader.limit(chunk_len);
            let result = self
                .inner
                .write()
                .write_at(offset + written, &mut chunk_reader);
            match result {
                Ok(chunk_written) => {
                    debug_assert_eq!(chunk_written, chunk_len);
                    if chunk_written == 0 {
                        return Ok(written);
                    }
                    reader.skip(chunk_written);
                    written += chunk_written;
                }
                Err(err) if written == 0 => return Err(err),
                Err(_) => return Ok(written),
            }
            if fs.writeback_controller().balance_dirty_pages().is_err() {
                return Ok(written);
            }
        }
        Ok(written)
    }

    /// Direct-I/O read path.
    pub(in ext4) fn read_direct_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        let read_len = writer.avail();
        if !is_block_aligned(offset) || !is_block_aligned(read_len) {
            // TODO: Implement a fallback mechanism.
            return_errno_with_message!(Errno::EINVAL, "not block-aligned");
        }
        if read_len == 0 {
            return Ok(0);
        }

        let fs = self.fs()?;
        let read_len = fs
            .writeback_controller()
            .with_submission_allowed(|| self.inner.read().read_direct_at(&fs, offset, writer))?;
        if read_len > 0 {
            self.set_atime(utils::now());
        }
        Ok(read_len)
    }

    /// Direct-I/O write path with pre-allocation and rollback.
    pub(in ext4) fn write_direct_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        let write_len = reader.remain();
        if write_len == 0 {
            return Ok(0);
        }

        if !is_block_aligned(offset) || !is_block_aligned(write_len) {
            // TODO: Implement a fallback mechanism.
            return_errno_with_message!(Errno::EINVAL, "not block-aligned");
        }

        let fs = self.fs()?;
        let mut inner = self.inner.write();
        inner.write_direct_at(&fs, offset, reader)
    }

    /// Truncates or extends a regular file to `new_size` bytes.
    ///
    /// Shrinking frees the trailing data/metadata blocks and zeroes the kept
    /// partial last block; expanding is sparse (the gap is a hole that reads as
    /// zeros). Directories are rejected with `EISDIR`.
    pub(in ext4) fn resize(&self, new_size: usize) -> Result<()> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }
        if self.type_ != InodeType::File && self.type_ != InodeType::SymLink {
            return_errno!(Errno::EINVAL);
        }
        let mut inner = self.inner.write();
        inner.resize(new_size)?;
        Ok(())
    }

    /// Preallocates blocks for `[offset, offset + len)`.
    ///
    /// `Allocate` also extends the file size if the range ends past it;
    /// `AllocateKeepSize` never touches the size. Other modes are unsupported.
    pub(in ext4) fn fallocate(&self, mode: FallocMode, offset: usize, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "fallocate range overflow"))?;
        let mut inner = self.inner.write();
        let old_size = inner.file_size();
        if end > old_size {
            inner.ensure_size_within_limit(end)?;
        }

        match mode {
            FallocMode::Allocate => {
                if let Err(err) = inner.preallocate_range(offset, end) {
                    inner.rollback_fallocate(old_size);
                    return Err(err);
                }
                if end > old_size
                    && let Err(err) = inner.expand(end)
                {
                    inner.rollback_fallocate(old_size);
                    return Err(err);
                }
            }
            FallocMode::AllocateKeepSize => {
                if let Err(err) = inner.preallocate_range(offset, end) {
                    inner.rollback_fallocate(old_size);
                    return Err(err);
                }
            }
            _ => {
                return_errno_with_message!(
                    Errno::EOPNOTSUPP,
                    "fallocate with the specified flags is not supported"
                );
            }
        }

        inner.set_mtime_ctime(utils::now());
        Ok(())
    }
}

impl InodeInner {
    /// Prepares the inode for a write spanning `[offset, end)`: grows the page
    /// cache if extending, then allocates data blocks for any holes covered.
    ///
    /// On failure the caller must invoke `rollback_write` to restore page-cache
    /// capacity and free the partially allocated blocks.
    pub(super) fn prepare_write(&mut self, offset: usize, end: usize) -> Result<()> {
        let old_size = self.file_size();
        if end > old_size {
            self.ensure_size_within_limit(end)?;
            self.resize_page_cache(end, old_size)?;
        }
        let start_block = (offset / BLOCK_SIZE) as Iblock;
        let end_block = end.div_ceil(BLOCK_SIZE) as Iblock;
        self.block_manager()?
            .ensure_allocated(start_block, end_block)
    }

    fn prepare_buffered_write(
        &mut self,
        offset: usize,
        end: usize,
    ) -> Result<BufferedWriteReservation> {
        let old_size = self.file_size();
        if end > old_size {
            self.ensure_size_within_limit(end)?;
            self.resize_page_cache(end, old_size)?;
        }
        let start_block = (offset / BLOCK_SIZE) as Iblock;
        let end_block = end.div_ceil(BLOCK_SIZE) as Iblock;
        self.block_manager()?
            .prepare_buffered_write(start_block, end_block)
    }

    /// Restores page-cache capacity and frees blocks allocated past `old_size`
    /// after a failed write.
    pub(super) fn rollback_write(&mut self, old_size: usize, end: usize) {
        if end <= old_size {
            return;
        }
        if let Err(err) = self.resize_page_cache(old_size, end) {
            error!(
                "write_at: cleanup page cache resize failed: old_size={}, err={:?}",
                old_size, err
            );
        }
        if let Ok(block_manager) = self.block_manager()
            && let Err(err) = block_manager.truncate_to_byte_len(old_size)
        {
            error!("write_at: cleanup block truncate failed: {:?}", err);
        }
    }

    /// Frees blocks mapped past `old_size` after a failed fallocate. Blocks
    /// preallocated inside `[0, old_size)` stay; they are harmless.
    fn rollback_fallocate(&mut self, old_size: usize) {
        if let Ok(block_manager) = self.block_manager()
            && let Err(err) = block_manager.truncate_to_byte_len(old_size)
        {
            error!("fallocate: cleanup block truncate failed: {:?}", err);
        }
    }

    fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        if writer.avail() == 0 {
            return Ok(0);
        }
        let file_size = self.file_size();
        if offset >= file_size {
            return Ok(0);
        }
        let read_len = writer.avail().min(file_size - offset);
        writer.limit(read_len);
        self.page_cache()?.read(offset, writer)?;
        Ok(read_len)
    }

    /// Writes file data at `offset` through the page cache.
    fn write_at(&mut self, offset: usize, reader: &mut VmReader) -> Result<usize> {
        let write_len = reader.remain();
        if write_len == 0 {
            return Ok(0);
        }
        let end = offset
            .checked_add(write_len)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "write range overflow"))?;
        let old_size = self.file_size();

        let reservation = match self.prepare_buffered_write(offset, end) {
            Ok(reservation) => reservation,
            Err(err) => {
                self.rollback_write(old_size, end);
                return Err(err);
            }
        };
        if let Err(err) = self.page_cache()?.write(offset, reader) {
            if let Ok(block_manager) = self.block_manager() {
                block_manager.cancel_buffered_write(reservation);
            }
            self.rollback_write(old_size, end);
            return Err(err.into());
        }

        self.set_mtime_ctime(utils::now());
        if end > old_size {
            self.set_file_size(end);
        }
        Ok(write_len)
    }

    /// Reads file data directly after flushing overlapping cached pages.
    fn read_direct_at(&self, fs: &Ext4, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        let file_size = self.file_size();
        if offset >= file_size || writer.avail() == 0 {
            return Ok(0);
        }

        let read_len = writer.avail().min(file_size - offset);
        writer.limit(read_len);
        let end = offset + read_len;

        self.page_cache()?.flush_range(offset..end)?;
        self.read_direct_blocks(fs, offset, end, writer)?;
        Ok(read_len)
    }

    /// Writes file data directly after invalidating overlapping cached pages.
    fn write_direct_at(
        &mut self,
        fs: &Ext4,
        offset: usize,
        reader: &mut VmReader,
    ) -> Result<usize> {
        let write_len = reader.remain();
        if write_len == 0 {
            return Ok(0);
        }

        let end = offset
            .checked_add(write_len)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "write range overflow"))?;
        let old_size = self.file_size();

        let discard_start_bytes = offset.min(old_size);
        let discard_end_bytes = end.min(old_size);
        if discard_start_bytes < discard_end_bytes {
            self.page_cache()?
                .invalidate_range(discard_start_bytes..discard_end_bytes)?;
        }

        if let Err(err) = self.prepare_write(offset, end) {
            self.rollback_write(old_size, end);
            return Err(err);
        }

        if let Err(err) = self.write_direct_blocks(fs, offset, reader) {
            self.rollback_write(old_size, end);
            return Err(err);
        }

        self.set_mtime_ctime(utils::now());
        if end > self.file_size() {
            self.set_file_size(end);
        }
        Ok(write_len)
    }

    /// Reads file data directly from data blocks into `writer`.
    ///
    /// Holes and unwritten extents read as zeros, exactly like the buffered
    /// path.
    fn read_direct_blocks(
        &self,
        fs: &Ext4,
        offset: usize,
        end: usize,
        writer: &mut VmWriter,
    ) -> Result<()> {
        let block_manager = self.block_manager()?;
        let mut iblock = (offset / BLOCK_SIZE) as Iblock;
        let iblock_end = end.div_ceil(BLOCK_SIZE) as Iblock;

        while iblock < iblock_end {
            let mapping = block_manager.map_blocks(iblock)?;
            let run_len = mapping.len().min(iblock_end - iblock);
            if mapping.reads_as_zeros() {
                writer.fill_zeros(run_len as usize * BLOCK_SIZE)?;
            } else {
                let Some(TierBlock::Ssd(pblock)) = mapping.tier_block() else {
                    return_errno_with_message!(
                        Errno::EOPNOTSUPP,
                        "direct I/O on an NV-tier extent is not supported"
                    );
                };
                let bio_segment = BioSegment::alloc(run_len as usize, BioDirection::FromDevice);
                fs.read_blocks(pblock, bio_segment.clone())?;
                bio_segment.reader()?.read_fallible(writer)?;
            }
            iblock += run_len;
        }
        Ok(())
    }

    /// Writes file data directly to already-allocated data blocks.
    fn write_direct_blocks(
        &mut self,
        fs: &Ext4,
        offset: usize,
        reader: &mut VmReader,
    ) -> Result<()> {
        let write_len = reader.remain();
        debug_assert_eq!(write_len % BLOCK_SIZE, 0);
        // `end` is already checked in `InodeInner::write_direct_at`.
        let end = offset + write_len;
        let block_manager = self.block_manager()?;
        let mut iblock = (offset / BLOCK_SIZE) as Iblock;
        let iblock_end = end.div_ceil(BLOCK_SIZE) as Iblock;

        let mut io_batch = IoBatch::new();
        while iblock < iblock_end {
            let mapping = block_manager.map_blocks(iblock)?;
            let Mapping::Mapped { pblock, len, state } = mapping else {
                // The upper layer should have performed allocation for the write
                // range. Linux does not allocate blocks in the direct write path;
                // when it encounters a hole it falls back to buffered write to
                // prevent stale data exposure. We pre-allocate in `prepare_write`
                // so holes here indicate a bug. Stale-read is not a concern
                // because our inode-level lock serializes reads after this write.
                return_errno_with_message!(Errno::EIO, "unexpected hole in direct write path");
            };
            let TierBlock::Ssd(pblock) = TierBlock::decode(pblock)? else {
                return_errno_with_message!(
                    Errno::EOPNOTSUPP,
                    "direct I/O on an NV-tier extent is not supported"
                );
            };
            if state == MapState::Unwritten {
                // `prepare_write` allocates in written state (and converts any
                // unwritten extent it covers), so this also indicates a bug.
                return_errno_with_message!(
                    Errno::EIO,
                    "unexpected unwritten extent in direct write path"
                );
            }
            let run_len = len.min(iblock_end - iblock);
            let bio_segment = BioSegment::alloc(run_len as usize, BioDirection::ToDevice);
            bio_segment.writer()?.write_fallible(reader)?;
            fs.write_blocks_async(pblock, bio_segment, None, &mut io_batch)?;
            iblock += run_len;
        }

        io_batch.wait_all()?;
        Ok(())
    }

    /// Truncates or extends the file to `new_size` bytes.
    ///
    /// Shrinking zeroes the partial tail (in the page cache, via
    /// `resize_page_cache`) before freeing the trailing data/metadata blocks;
    /// expanding is sparse (no allocation — the gap stays a hole that reads as
    /// zeros). The caller updates timestamps and holds the `inner` write lock.
    fn resize(&mut self, new_size: usize) -> Result<()> {
        let old_size = self.file_size();
        if new_size == old_size {
            return Ok(());
        }
        if new_size < old_size {
            self.shrink(new_size)?;
        } else {
            self.expand(new_size)?;
        }
        self.set_mtime_ctime(utils::now());
        Ok(())
    }

    /// Shrinks the file: zeroes the kept partial last block in the page cache,
    /// frees every data/metadata block past `new_size`, then publishes the size.
    pub(super) fn shrink(&mut self, new_size: usize) -> Result<()> {
        let old_size = self.file_size();
        // Shrink the VMO before publishing the smaller size.
        // `PageCache::resize` zeroes `[new_size, block_end)` of the
        // kept partial block (BLOCK_SIZE == PAGE_SIZE), so stale tail bytes do
        // not reappear if the file is later extended.
        self.resize_page_cache(new_size, old_size)?;
        self.block_manager()?.truncate_to_byte_len(new_size)?;
        self.set_file_size(new_size);
        Ok(())
    }

    /// Expands the file sparsely: grows the page cache and publishes the new
    /// size without allocating any data block — the gap stays a hole.
    fn expand(&mut self, new_size: usize) -> Result<()> {
        let old_size = self.file_size();
        if new_size <= old_size {
            return Ok(());
        }
        self.ensure_size_within_limit(new_size)?;
        // Publish the size before growing the VMO.
        self.set_file_size(new_size);
        self.resize_page_cache(new_size, old_size)?;
        Ok(())
    }

    /// Preallocates blocks covering the byte range `[offset, end)`.
    fn preallocate_range(&mut self, offset: usize, end: usize) -> Result<()> {
        let start_block = (offset / BLOCK_SIZE) as Iblock;
        let end_block = end.div_ceil(BLOCK_SIZE) as Iblock;
        self.block_manager()?.preallocate(start_block, end_block)
    }

    /// Rejects growth beyond the maximum representable file size.
    fn ensure_size_within_limit(&self, new_size: usize) -> Result<()> {
        let max = match self.desc.type_() {
            InodeType::File => self.block_manager()?.max_file_size(),
            _ => usize::try_from(u32::MAX).expect("Asterinas supports 64-bit architectures"),
        };
        if new_size > max {
            return_errno_with_message!(Errno::EFBIG, "inode size exceeds ext4 maximum");
        }
        Ok(())
    }
}

fn is_block_aligned(offset: usize) -> bool {
    offset.is_multiple_of(BLOCK_SIZE)
}

#[cfg(ktest)]
mod tests {
    use aster_block::BLOCK_SIZE;
    use ostd::{mm::VmIo, prelude::ktest};

    use super::{
        super::super::test_utils::{Ext4Fixture, Ext4FixtureBuilder, make_empty_file_inode},
        MapState,
    };
    use crate::{
        fs::{
            fs_impls::ext4::{
                fs::{Ext4, MountFlavor},
                inode::block_mapping::extent::node::TierBlock,
            },
            nv_pool::{
                config::{NvPoolConfig, NvSimMode},
                dram::DramNvBackend,
                frame::NvPool,
            },
            vfs::{file_system::FsFlags, inode::FallocMode},
        },
        prelude::*,
        time::clocks,
    };

    const FILE_INO: u32 = 11;

    /// A single-group fixture capped to exactly `free` free data blocks, holding
    /// an empty extent-based regular file at `FILE_INO`.
    fn fixture_with_free_blocks(free: u32) -> Ext4Fixture {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_free_blocks(free)
            .build()
            .unwrap();
        f.write_raw_inode(FILE_INO, &make_empty_file_inode());
        f
    }

    fn nv_pool() -> Arc<NvPool> {
        let config = NvPoolConfig::from_bytes(
            Some(128 * PAGE_SIZE),
            Some(64 * PAGE_SIZE),
            Some(64 * PAGE_SIZE),
            Some(NvSimMode::Fast),
        )
        .unwrap()
        .unwrap();
        let backend = Arc::new(DramNvBackend::allocate(config).unwrap());
        Arc::new(NvPool::format(backend, config, None).unwrap())
    }

    #[ktest]
    fn fsync_publishes_nv_extent_and_clean_drain_restores_ssd_extent() {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_free_blocks(8)
            .with_reserved_inode(FILE_INO)
            .with_nv_pool(nv_pool())
            .build()
            .unwrap();
        f.write_raw_inode(FILE_INO, &make_empty_file_inode());
        let inode = f.ext4.read_inode(FILE_INO).unwrap();
        let data = vec![0xa5; BLOCK_SIZE];
        inode
            .write_at(0, &mut VmReader::from(data.as_slice()).to_fallible())
            .unwrap();

        inode.sync_data().unwrap();

        let mapping = inode
            .inner
            .read()
            .block_manager()
            .unwrap()
            .map_blocks(0)
            .unwrap();
        let TierBlock::Nv(page) = mapping.tier_block().unwrap() else {
            panic!("fsync did not publish an NV extent")
        };
        let target = f.ext4.nv_state().unwrap().data_page_target(page).unwrap();
        let mut direct_read = vec![0; BLOCK_SIZE];
        assert_eq!(
            inode
                .read_direct_at(
                    0,
                    &mut VmWriter::from(direct_read.as_mut_slice()).to_fallible(),
                )
                .unwrap_err()
                .error(),
            Errno::EOPNOTSUPP
        );
        assert_eq!(
            inode
                .write_direct_at(
                    0,
                    &mut VmReader::from(vec![0x5c; BLOCK_SIZE].as_slice()).to_fallible(),
                )
                .unwrap_err()
                .error(),
            Errno::EOPNOTSUPP
        );
        let err = match Ext4::open(f.disk.clone(), FsFlags::empty(), MountFlavor::Ext4, None) {
            Ok(_) => panic!("NV-tier volume mounted without the nvpool option"),
            Err(err) => err,
        };
        assert_eq!(err.error(), Errno::EINVAL);

        f.ext4.drain_nv_data().unwrap();
        f.ext4.nv_state().unwrap().drain_all_metadata().unwrap();
        f.ext4.finish_nv_drain().unwrap();

        let mapping = inode
            .inner
            .read()
            .block_manager()
            .unwrap()
            .map_blocks(0)
            .unwrap();
        assert_eq!(mapping.tier_block(), Some(TierBlock::Ssd(target)));
        let mut ssd = vec![0; BLOCK_SIZE];
        f.disk
            .segment()
            .read_bytes(target as usize * BLOCK_SIZE, &mut ssd)
            .unwrap();
        assert_eq!(ssd, data);
        assert!(Ext4::open(f.disk.clone(), FsFlags::empty(), MountFlavor::Ext4, None).is_ok());
        assert_eq!(
            f.ext4
                .nv_state()
                .unwrap()
                .data_page_target(page)
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );
    }

    #[ktest]
    fn fsync_overwrite_replaces_and_releases_old_nv_page() {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_free_blocks(8)
            .with_reserved_inode(FILE_INO)
            .with_nv_pool(nv_pool())
            .build()
            .unwrap();
        f.write_raw_inode(FILE_INO, &make_empty_file_inode());
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        let first_data = vec![0x31; BLOCK_SIZE];
        inode
            .write_at(0, &mut VmReader::from(first_data.as_slice()).to_fallible())
            .unwrap();
        inode.sync_data().unwrap();
        let first_mapping = inode
            .inner
            .read()
            .block_manager()
            .unwrap()
            .map_blocks(0)
            .unwrap();
        let TierBlock::Nv(first_page) = first_mapping.tier_block().unwrap() else {
            panic!("first fsync did not publish an NV extent")
        };
        let target = f
            .ext4
            .nv_state()
            .unwrap()
            .data_page_target(first_page)
            .unwrap();

        let overwrite_offset = 137;
        let overwrite = vec![0x72; 500];
        let mut expected = first_data.clone();
        expected[overwrite_offset..overwrite_offset + overwrite.len()].copy_from_slice(&overwrite);
        inode
            .write_at(
                overwrite_offset,
                &mut VmReader::from(overwrite.as_slice()).to_fallible(),
            )
            .unwrap();
        inode.sync_data().unwrap();
        let second_mapping = inode
            .inner
            .read()
            .block_manager()
            .unwrap()
            .map_blocks(0)
            .unwrap();
        let TierBlock::Nv(second_page) = second_mapping.tier_block().unwrap() else {
            panic!("overwrite fsync did not publish an NV extent")
        };

        assert_ne!(first_page, second_page);
        assert_eq!(
            f.ext4
                .nv_state()
                .unwrap()
                .data_page_target(second_page)
                .unwrap(),
            target
        );
        assert_eq!(
            f.ext4
                .nv_state()
                .unwrap()
                .data_page_target(first_page)
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );
        let mut readback = vec![0; BLOCK_SIZE];
        inode
            .read_at(
                0,
                &mut VmWriter::from(readback.as_mut_slice()).to_fallible(),
            )
            .unwrap();
        assert_eq!(readback, expected);
    }

    #[ktest]
    fn nvpool_fsync_does_not_flush_ssd_after_tier_guard_is_durable() {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_free_blocks(8)
            .with_reserved_inode(FILE_INO)
            .with_nv_pool(nv_pool())
            .build()
            .unwrap();
        f.write_raw_inode(FILE_INO, &make_empty_file_inode());
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        inode
            .write_at(
                0,
                &mut VmReader::from(vec![0x41; BLOCK_SIZE].as_slice()).to_fallible(),
            )
            .unwrap();
        inode.sync_data().unwrap();
        let flushes_after_guard = f.disk.flush_count();
        assert!(flushes_after_guard > 0);

        inode
            .write_at(
                0,
                &mut VmReader::from(vec![0x52; BLOCK_SIZE].as_slice()).to_fallible(),
            )
            .unwrap();
        inode.sync_data().unwrap();

        assert_eq!(f.disk.flush_count(), flushes_after_guard);
    }

    #[ktest]
    fn nvpool_filesystem_sync_does_not_flush_ssd() {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_free_blocks(8)
            .with_reserved_inode(FILE_INO)
            .with_nv_pool(nv_pool())
            .build()
            .unwrap();
        f.write_raw_inode(FILE_INO, &make_empty_file_inode());
        let inode = f.ext4.read_inode(FILE_INO).unwrap();
        inode
            .write_at(
                0,
                &mut VmReader::from(vec![0x63; BLOCK_SIZE].as_slice()).to_fallible(),
            )
            .unwrap();
        inode.sync_data().unwrap();
        let flushes_after_guard = f.disk.flush_count();

        crate::fs::vfs::file_system::FileSystem::sync(f.ext4.as_ref()).unwrap();

        assert_eq!(f.disk.flush_count(), flushes_after_guard);
    }

    #[ktest]
    fn truncate_defers_block_reuse_until_new_inode_root_commits() {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_free_blocks(8)
            .with_reserved_inode(FILE_INO)
            .with_nv_pool(nv_pool())
            .build()
            .unwrap();
        f.write_raw_inode(FILE_INO, &make_empty_file_inode());
        let inode = f.ext4.read_inode(FILE_INO).unwrap();
        let data = vec![0x71; 2 * BLOCK_SIZE];
        inode
            .write_at(0, &mut VmReader::from(data.as_slice()).to_fallible())
            .unwrap();
        inode.sync_data().unwrap();
        let allocated_free_count = f.ext4.super_block().free_blocks_count();

        inode.resize(0).unwrap();
        assert_eq!(
            f.ext4.super_block().free_blocks_count(),
            allocated_free_count,
            "truncate exposed blocks for reuse before inode commit"
        );

        inode.sync_data().unwrap();
        assert_eq!(
            f.ext4.super_block().free_blocks_count(),
            allocated_free_count + 2
        );
    }

    /// A buffered write that runs out of space midway rolls back cleanly: the
    /// file size, the free-block count, and the already-written data are all
    /// left as they were before the failing write.
    #[ktest]
    fn file_write_enospc_rollback() {
        // Exactly two free data blocks in group 0.
        let f = fixture_with_free_blocks(2);
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // First write lands one block; one free block remains.
        let base_data = vec![0x44u8; BLOCK_SIZE];
        let mut reader = VmReader::from(base_data.as_slice()).to_fallible();
        assert_eq!(inode.write_at(0, &mut reader).unwrap(), BLOCK_SIZE);
        inode.sync_data().unwrap();
        let free_before_fail = f.ext4.super_block().free_blocks_count();
        assert_eq!(free_before_fail, 1);

        // A two-block write past EOF needs two blocks but only one is free, so
        // it fails with ENOSPC and then rolls back its partial allocation.
        let fail_payload = vec![0x66u8; BLOCK_SIZE * 2];
        let mut fail_reader = VmReader::from(fail_payload.as_slice()).to_fallible();
        assert_eq!(
            inode
                .write_at(BLOCK_SIZE, &mut fail_reader)
                .unwrap_err()
                .error(),
            Errno::ENOSPC
        );

        // Size, free count, and the first block's data survive unchanged.
        assert_eq!(inode.file_size(), BLOCK_SIZE);
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before_fail);

        let mut readback = vec![0u8; BLOCK_SIZE];
        let mut writer = VmWriter::from(readback.as_mut_slice()).to_fallible();
        inode.read_at(0, &mut writer).unwrap();
        assert_eq!(readback, base_data);
    }

    #[ktest]
    fn buffered_write_reserves_without_allocating_and_does_not_double_reserve() {
        let f = fixture_with_free_blocks(2);
        let inode = f.ext4.read_inode(FILE_INO).unwrap();
        let free_before = f.ext4.super_block().free_blocks_count();

        let data = vec![0x31u8; BLOCK_SIZE];
        let mut first = VmReader::from(data.as_slice()).to_fallible();
        assert_eq!(inode.write_at(0, &mut first).unwrap(), BLOCK_SIZE);
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before);
        assert_eq!(f.ext4.reserved_data_blocks(), 1);

        let mut overwrite = VmReader::from(data.as_slice()).to_fallible();
        assert_eq!(inode.write_at(0, &mut overwrite).unwrap(), BLOCK_SIZE);
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before);
        assert_eq!(f.ext4.reserved_data_blocks(), 1);

        let two_blocks = vec![0x42u8; BLOCK_SIZE * 2];
        let mut no_space = VmReader::from(two_blocks.as_slice()).to_fallible();
        assert_eq!(
            inode
                .write_at(BLOCK_SIZE, &mut no_space)
                .unwrap_err()
                .error(),
            Errno::ENOSPC
        );
        assert_eq!(inode.file_size(), BLOCK_SIZE);
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before);
        assert_eq!(f.ext4.reserved_data_blocks(), 1);
    }

    #[ktest]
    fn buffered_write_and_sync_update_mount_dirty_accounting() {
        let f = fixture_with_free_blocks(2);
        let inode = f.ext4.read_inode(FILE_INO).unwrap();
        let data = vec![0x6bu8; BLOCK_SIZE];
        let mut reader = VmReader::from(data.as_slice()).to_fallible();

        inode.write_at(0, &mut reader).unwrap();
        assert_eq!(f.ext4.writeback_controller().outstanding_pages(), 1);

        inode.sync_data().unwrap();
        assert_eq!(f.ext4.writeback_controller().outstanding_pages(), 0);
    }

    #[ktest]
    fn writeback_queue_keeps_the_exact_dirty_inode_object() {
        let f = fixture_with_free_blocks(2);
        let inode = f.ext4.read_inode(FILE_INO).unwrap();
        let data = vec![0x6bu8; BLOCK_SIZE];
        let mut reader = VmReader::from(data.as_slice()).to_fallible();

        inode.write_at(0, &mut reader).unwrap();
        f.ext4.remove_inode(FILE_INO);

        let queued = f.ext4.writeback_controller().pop_dirty_inode().unwrap();
        assert!(Arc::ptr_eq(&queued, &inode));
    }

    #[ktest]
    fn nodelalloc_buffered_write_allocates_before_returning() {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_free_blocks(2)
            .with_mount_options("nodelalloc")
            .build()
            .unwrap();
        f.write_raw_inode(FILE_INO, &make_empty_file_inode());
        let inode = f.ext4.read_inode(FILE_INO).unwrap();
        let free_before = f.ext4.super_block().free_blocks_count();
        let data = vec![0x37u8; BLOCK_SIZE];
        let mut reader = VmReader::from(data.as_slice()).to_fallible();

        assert_eq!(inode.write_at(0, &mut reader).unwrap(), BLOCK_SIZE);
        assert_eq!(f.ext4.reserved_data_blocks(), 0);
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before - 1);
        let inner = inode.inner.read();
        assert_eq!(
            inner
                .block_manager()
                .unwrap()
                .map_blocks(0)
                .unwrap()
                .state(),
            Some(MapState::Written)
        );
    }

    #[ktest]
    fn fsync_materializes_delayed_extent_and_commits_written_state() {
        let f = fixture_with_free_blocks(4);
        let inode = f.ext4.read_inode(FILE_INO).unwrap();
        let free_before = f.ext4.super_block().free_blocks_count();
        let data = vec![0x5au8; BLOCK_SIZE * 2];
        let mut reader = VmReader::from(data.as_slice()).to_fallible();

        inode.write_at(0, &mut reader).unwrap();
        assert_eq!(f.ext4.reserved_data_blocks(), 2);
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before);

        inode.sync_data().unwrap();
        assert_eq!(f.ext4.reserved_data_blocks(), 0);
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before - 2);
        let inner = inode.inner.read();
        let mapping = inner.block_manager().unwrap().map_blocks(0).unwrap();
        assert_eq!(mapping.state(), Some(MapState::Written));
        drop(inner);

        let mut readback = vec![0u8; data.len()];
        let mut writer = VmWriter::from(readback.as_mut_slice()).to_fallible();
        inode.read_at(0, &mut writer).unwrap();
        assert_eq!(readback, data);
    }

    #[ktest]
    fn fsync_persists_delalloc_block_bitmap_before_returning() {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        f.write_raw_inode(FILE_INO, &make_empty_file_inode());
        let inode = f.ext4.read_inode(FILE_INO).unwrap();
        let data = vec![0x5au8; BLOCK_SIZE];
        let mut reader = VmReader::from(data.as_slice()).to_fallible();
        let scanned_before = f.ext4.io_stats().snapshot().scanned_groups;

        inode.write_at(0, &mut reader).unwrap();
        inode.sync_data().unwrap();

        assert_eq!(
            f.ext4.io_stats().snapshot().scanned_groups - scanned_before,
            1
        );

        let pblock = inode
            .inner
            .read()
            .block_manager()
            .unwrap()
            .map_blocks(0)
            .unwrap()
            .pblock()
            .unwrap() as usize;
        let mut bitmap_byte = [0u8; 1];
        f.disk
            .segment()
            .read_bytes(2 * BLOCK_SIZE + pblock / 8, &mut bitmap_byte)
            .unwrap();
        assert_ne!(bitmap_byte[0] & (1 << (pblock % 8)), 0);
    }

    #[ktest]
    fn mapped_overwrite_fsync_does_not_scan_block_groups() {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        f.write_raw_inode(FILE_INO, &make_empty_file_inode());
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        let first = vec![0x31u8; BLOCK_SIZE];
        inode
            .write_at(0, &mut VmReader::from(first.as_slice()).to_fallible())
            .unwrap();
        inode.sync_data().unwrap();

        let scanned_before = f.ext4.io_stats().snapshot().scanned_groups;
        let overwrite = vec![0x72u8; BLOCK_SIZE];
        inode
            .write_at(0, &mut VmReader::from(overwrite.as_slice()).to_fallible())
            .unwrap();
        inode.sync_data().unwrap();

        assert_eq!(
            f.ext4.io_stats().snapshot().scanned_groups - scanned_before,
            0
        );
    }

    #[ktest]
    fn failed_delayed_writeback_remains_retryable() {
        let f = fixture_with_free_blocks(4);
        let inode = f.ext4.read_inode(FILE_INO).unwrap();
        let data = vec![0x6bu8; BLOCK_SIZE];
        let mut reader = VmReader::from(data.as_slice()).to_fallible();
        inode.write_at(0, &mut reader).unwrap();

        f.disk.set_fail_writes(true);
        assert_eq!(inode.sync_data().unwrap_err().error(), Errno::EIO);
        assert_eq!(f.ext4.reserved_data_blocks(), 0);
        let inner = inode.inner.read();
        assert_eq!(
            inner
                .block_manager()
                .unwrap()
                .map_blocks(0)
                .unwrap()
                .state(),
            Some(MapState::Unwritten)
        );
        drop(inner);

        f.disk.set_fail_writes(false);
        inode.sync_data().unwrap();
        f.ext4.writeback_controller().balance_dirty_pages().unwrap();
        let inner = inode.inner.read();
        assert_eq!(
            inner
                .block_manager()
                .unwrap()
                .map_blocks(0)
                .unwrap()
                .state(),
            Some(MapState::Written)
        );
    }

    #[ktest]
    fn truncate_releases_only_discarded_delayed_reservations() {
        let f = fixture_with_free_blocks(8);
        let inode = f.ext4.read_inode(FILE_INO).unwrap();
        let data = vec![0x29u8; BLOCK_SIZE * 4];
        let mut reader = VmReader::from(data.as_slice()).to_fallible();

        inode.write_at(0, &mut reader).unwrap();
        assert_eq!(f.ext4.reserved_data_blocks(), 4);

        inode.resize(BLOCK_SIZE * 2).unwrap();
        assert_eq!(inode.file_size(), BLOCK_SIZE * 2);
        assert_eq!(f.ext4.reserved_data_blocks(), 2);

        inode.sync_data().unwrap();
        assert_eq!(f.ext4.reserved_data_blocks(), 0);
        let mut readback = vec![0u8; BLOCK_SIZE * 2];
        let mut writer = VmWriter::from(readback.as_mut_slice()).to_fallible();
        inode.read_at(0, &mut writer).unwrap();
        assert_eq!(readback, data[..BLOCK_SIZE * 2]);
    }

    /// `fallocate(Allocate)` that exhausts the volume returns ENOSPC without
    /// disturbing the blocks and size an earlier successful fallocate produced.
    #[ktest]
    fn falloc_allocate_returns_enospc_after_consuming_blocks() {
        // Exactly two free data blocks in group 0.
        let f = fixture_with_free_blocks(2);
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // Consume both free blocks and extend the size to two blocks.
        inode
            .fallocate(FallocMode::Allocate, 0, BLOCK_SIZE * 2)
            .unwrap();
        assert_eq!(f.ext4.super_block().free_blocks_count(), 0);
        assert_eq!(inode.file_size(), BLOCK_SIZE * 2);

        // A further allocation has no free block, so it fails with ENOSPC and
        // leaves the free count and size from the successful call intact.
        assert_eq!(
            inode
                .fallocate(FallocMode::Allocate, BLOCK_SIZE * 2, BLOCK_SIZE)
                .unwrap_err()
                .error(),
            Errno::ENOSPC
        );
        assert_eq!(f.ext4.super_block().free_blocks_count(), 0);
        assert_eq!(inode.file_size(), BLOCK_SIZE * 2);
    }
}
