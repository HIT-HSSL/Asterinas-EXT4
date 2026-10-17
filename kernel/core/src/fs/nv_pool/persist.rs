// SPDX-License-Identifier: MPL-2.0

//! NVPool frame-table formatting and persistence protocol.

use super::{
    backend::NvMemoryBackend,
    capacity::PoolClass,
    fault::{FaultPoint, inject},
    layout::{
        FRAME_COMMIT_WORD, FrameTableEntry, NV_IMAGE_MAGIC, NV_IMAGE_VERSION, NvImageHeader,
        commit_offset, control_len, entry_offset,
    },
};
use crate::prelude::*;

const CLASS_METADATA: u8 = 1;
const CLASS_DATA: u8 = 2;

pub(crate) struct PersistEngine {
    backend: Arc<dyn NvMemoryBackend>,
    capacity_pages: usize,
}

impl PersistEngine {
    pub(super) fn new(backend: Arc<dyn NvMemoryBackend>, capacity_pages: usize) -> Result<Self> {
        if backend.capacity_pages() != capacity_pages
            || backend.control_len() < control_len(capacity_pages)?
        {
            return_errno_with_message!(Errno::EINVAL, "NVPool image capacity mismatch");
        }
        Ok(Self {
            backend,
            capacity_pages,
        })
    }

    pub(super) fn format(&self) -> Result<()> {
        let image_len = control_len(self.capacity_pages)?;
        let mut image = vec![0; image_len];
        let header = NvImageHeader {
            magic: NV_IMAGE_MAGIC,
            capacity_pages: self.capacity_pages as u64,
            version: NV_IMAGE_VERSION,
            header_size: size_of::<NvImageHeader>() as u32,
            entry_size: size_of::<FrameTableEntry>() as u32,
            flags: 0,
            reserved: [0; 32],
        };
        image[..size_of::<NvImageHeader>()].copy_from_slice(header.as_bytes());
        self.backend.write_control(0, &image)?;
        self.backend.persist_control(0, image_len)?;
        self.backend.fence()
    }

    pub(super) fn read_header(&self) -> Result<NvImageHeader> {
        let mut header = NvImageHeader::default();
        self.backend.read_control(0, header.as_mut_bytes())?;
        if header.magic != NV_IMAGE_MAGIC
            || header.version != NV_IMAGE_VERSION
            || header.header_size as usize != size_of::<NvImageHeader>()
            || header.entry_size as usize != size_of::<FrameTableEntry>()
            || header.capacity_pages != self.capacity_pages as u64
        {
            return_errno_with_message!(Errno::EINVAL, "invalid NVPool image header");
        }
        Ok(header)
    }

    pub(super) fn read_entry(
        &self,
        index: usize,
    ) -> Result<Option<(PoolClass, [u8; 16], u64, u64, u64)>> {
        self.check_index(index)?;
        if self.backend.load_control_u64(commit_offset(index)?)? != FRAME_COMMIT_WORD {
            return Ok(None);
        }
        let mut entry = FrameTableEntry::default();
        self.backend
            .read_control(entry_offset(index)?, entry.as_mut_bytes())?;
        let class = match entry.class {
            CLASS_METADATA => PoolClass::Metadata,
            CLASS_DATA => PoolClass::Data,
            _ => return_errno_with_message!(Errno::EINVAL, "invalid NVPool frame class"),
        };
        Ok(Some((
            class,
            entry.namespace,
            entry.home_block,
            entry.version,
            entry.slot_bitmap,
        )))
    }

    pub(super) fn commit_frame(
        &self,
        index: usize,
        class: PoolClass,
        namespace: [u8; 16],
        home_block: u64,
        version: u64,
        slot_bitmap: u64,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        self.check_index(index)?;
        inject(fault, FaultPoint::BeforeContentPersist)?;
        let entry = FrameTableEntry {
            namespace,
            home_block,
            version,
            slot_bitmap,
            commit_word: 0,
            class: match class {
                PoolClass::Metadata => CLASS_METADATA,
                PoolClass::Data => CLASS_DATA,
            },
            reserved: [0; 7],
        };
        self.backend
            .write_control(entry_offset(index)?, entry.as_bytes())?;
        self.backend.persist(index * PAGE_SIZE, PAGE_SIZE)?;
        self.backend
            .persist_control(entry_offset(index)?, size_of::<FrameTableEntry>())?;
        self.backend.fence()?;
        inject(fault, FaultPoint::AfterContentPersist)?;

        let commit_offset = commit_offset(index)?;
        self.backend
            .store_control_u64(commit_offset, FRAME_COMMIT_WORD)?;
        inject(fault, FaultPoint::AfterCommitStore)?;
        self.backend
            .persist_control(commit_offset, size_of::<u64>())?;
        inject(fault, FaultPoint::AfterCommitPersist)?;
        self.backend.fence()?;
        inject(fault, FaultPoint::AfterCommitFence)
    }

    pub(super) fn clear_frame(&self, index: usize, fault: Option<FaultPoint>) -> Result<()> {
        self.check_index(index)?;
        inject(fault, FaultPoint::BeforeClear)?;
        let commit_offset = commit_offset(index)?;
        self.backend.store_control_u64(commit_offset, 0)?;
        inject(fault, FaultPoint::AfterClearStore)?;
        self.backend
            .persist_control(commit_offset, size_of::<u64>())?;
        inject(fault, FaultPoint::AfterClearPersist)?;
        self.backend.fence()?;
        inject(fault, FaultPoint::AfterClearFence)
    }

    fn check_index(&self, index: usize) -> Result<()> {
        if index >= self.capacity_pages {
            return_errno_with_message!(Errno::EINVAL, "NVPool frame index is out of bounds");
        }
        Ok(())
    }
}
