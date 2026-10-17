// SPDX-License-Identifier: MPL-2.0

//! Persistent NVPool control-image layout.

use core::mem::offset_of;

use crate::prelude::*;

pub(super) const NV_IMAGE_MAGIC: u64 = 0x4e56_504f_4f4c_3031;
pub(super) const NV_IMAGE_VERSION: u32 = 2;
pub(super) const FRAME_COMMIT_WORD: u64 = 0x434f_4d4d_4954_3031;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(crate) struct NvImageHeader {
    pub(super) magic: u64,
    pub(super) capacity_pages: u64,
    pub(super) version: u32,
    pub(super) header_size: u32,
    pub(super) entry_size: u32,
    pub(super) flags: u32,
    pub(super) reserved: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(crate) struct FrameTableEntry {
    pub(super) namespace: [u8; 16],
    pub(super) home_block: u64,
    pub(super) version: u64,
    pub(super) slot_bitmap: u64,
    pub(super) commit_word: u64,
    pub(super) class: u8,
    pub(super) reserved: [u8; 7],
}

const _: () = {
    assert!(size_of::<NvImageHeader>() == 64);
    assert!(align_of::<NvImageHeader>() == 8);
    assert!(size_of::<FrameTableEntry>() == 56);
    assert!(align_of::<FrameTableEntry>() == 8);
    assert!(offset_of!(FrameTableEntry, commit_word) == 40);
};

pub(super) fn control_len(capacity_pages: usize) -> Result<usize> {
    size_of::<NvImageHeader>()
        .checked_add(
            capacity_pages
                .checked_mul(size_of::<FrameTableEntry>())
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
        )
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))
}

pub(super) fn entry_offset(index: usize) -> Result<usize> {
    size_of::<NvImageHeader>()
        .checked_add(
            index
                .checked_mul(size_of::<FrameTableEntry>())
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
        )
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))
}

pub(super) fn commit_offset(index: usize) -> Result<usize> {
    entry_offset(index)?
        .checked_add(offset_of!(FrameTableEntry, commit_word))
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))
}
