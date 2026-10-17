// SPDX-License-Identifier: MPL-2.0

//! On-disk ext4 extent structures and their decoded forms.
//!
//! An extent tree node is a 12-byte header followed by 12-byte entries: index
//! entries (`RawExtentIdx`) in interior nodes, leaf entries (`RawExtent`) in
//! depth-0 nodes. The tree root lives inline in the inode's 60-byte `i_block`.

use crate::fs::{fs_impls::ext4::prelude::*, nv_pool::id::NvPageId};

/// Extent header magic (`eh_magic`).
pub(in crate::fs::fs_impls::ext4) const EXTENT_MAGIC: u16 = 0xF30A;

/// Maximum logical length encodable in a single extent. A length above this
/// marks the extent as unwritten (preallocated but not yet written).
pub(in crate::fs::fs_impls::ext4) const MAX_WRITTEN_LEN: u16 = 32768;
const NV_TIER_BIT: u64 = 1 << 47;
const TIER_PAYLOAD_MASK: u64 = NV_TIER_BIT - 1;

/// A leaf extent address in the SSD or NVPool half of the unified namespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::fs::fs_impls::ext4) enum TierBlock {
    Ssd(Ext4Bid),
    Nv(NvPageId),
}

impl TierBlock {
    pub(in crate::fs::fs_impls::ext4) fn encode(self) -> Result<u64> {
        match self {
            Self::Ssd(block) if block < NV_TIER_BIT => Ok(block),
            Self::Ssd(_) => {
                return_errno_with_message!(Errno::EOVERFLOW, "SSD block exceeds 47 bits")
            }
            Self::Nv(page) => {
                let index =
                    u64::try_from(page.index()).map_err(|_| Error::new(Errno::EOVERFLOW))?;
                if index > TIER_PAYLOAD_MASK {
                    return_errno_with_message!(Errno::EOVERFLOW, "NV page exceeds 47 bits");
                }
                Ok(NV_TIER_BIT | index)
            }
        }
    }

    pub(in crate::fs::fs_impls::ext4) fn decode(encoded: u64) -> Result<Self> {
        if encoded >> 48 != 0 {
            return_errno_with_message!(Errno::EOVERFLOW, "tier address exceeds 48 bits");
        }
        if encoded & NV_TIER_BIT == 0 {
            Ok(Self::Ssd(encoded))
        } else {
            let index = usize::try_from(encoded & TIER_PAYLOAD_MASK)
                .map_err(|_| Error::new(Errno::EOVERFLOW))?;
            Ok(Self::Nv(
                NvPageId::try_from(index).map_err(|_| Error::new(Errno::EOVERFLOW))?,
            ))
        }
    }

    pub(in crate::fs::fs_impls::ext4) fn offset(self, blocks: u32) -> Result<Self> {
        match self {
            Self::Ssd(start) => start
                .checked_add(Ext4Bid::from(blocks))
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))
                .and_then(|block| {
                    if block >= NV_TIER_BIT {
                        return_errno_with_message!(Errno::EOVERFLOW, "SSD extent crosses tier bit");
                    }
                    Ok(Self::Ssd(block))
                }),
            Self::Nv(start) => {
                let index = start
                    .index()
                    .checked_add(usize::try_from(blocks).unwrap())
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                Ok(Self::Nv(
                    NvPageId::try_from(index).map_err(|_| Error::new(Errno::EOVERFLOW))?,
                ))
            }
        }
    }
}

const_assert!(size_of::<RawExtentHeader>() == 12);
const_assert!(size_of::<RawExtentIdx>() == 12);
const_assert!(size_of::<RawExtent>() == 12);

/// On-disk extent-tree node header (`ext4_extent_header`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(in crate::fs::fs_impls::ext4) struct RawExtentHeader {
    /// Extent-tree magic number (`eh_magic`).
    pub magic: u16,
    /// Number of valid entries following the header (`eh_entries`).
    pub entries: u16,
    /// Entry capacity of this node (`eh_max`).
    pub max: u16,
    /// Distance from this node to its leaf level (`eh_depth`).
    pub depth: u16,
    /// Tree generation, reserved by ext4 for future use (`eh_generation`).
    pub generation: u32,
}

/// On-disk interior index entry (`ext4_extent_idx`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(in crate::fs::fs_impls::ext4) struct RawExtentIdx {
    /// First logical block this child covers (`ei_block`).
    pub block: u32,
    /// Lower 32 bits of the child node's physical block (`ei_leaf_lo`).
    pub leaf_lo: u32,
    /// Upper 16 bits of the child node's physical block (`ei_leaf_hi`).
    pub leaf_hi: u16,
    /// Reserved padding (`ei_unused`).
    pub unused: u16,
}

impl RawExtentIdx {
    /// Encodes a child pointer after checking the 48-bit on-disk limit.
    pub(super) fn new(block: Iblock, leaf: Ext4Bid) -> Result<Self> {
        if leaf >> 48 != 0 {
            return_errno_with_message!(Errno::EOVERFLOW, "extent child block exceeds 48 bits");
        }
        Ok(Self {
            block,
            leaf_lo: u32::try_from(leaf & u64::from(u32::MAX))
                .expect("masked block low half fits u32"),
            leaf_hi: u16::try_from(leaf >> 32).expect("48-bit block has 16-bit high half"),
            unused: 0,
        })
    }
}

/// On-disk leaf entry (`ext4_extent`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(in crate::fs::fs_impls::ext4) struct RawExtent {
    /// First logical block this extent covers (`ee_block`).
    pub block: u32,
    /// Length; the top bit (`> 32768`) marks the extent unwritten (`ee_len`).
    pub len: u16,
    /// Upper 16 bits of the starting physical block (`ee_start_hi`).
    pub start_hi: u16,
    /// Lower 32 bits of the starting physical block (`ee_start_lo`).
    pub start_lo: u32,
}

/// A validated extent-tree node header.
#[derive(Clone, Copy, Debug)]
pub(super) struct ExtentHeader {
    entries: u16,
    max: u16,
    depth: u16,
}

impl ExtentHeader {
    pub(super) const fn entries(&self) -> u16 {
        self.entries
    }

    pub(super) const fn max(&self) -> u16 {
        self.max
    }

    /// Returns the node depth: 0 is a leaf (holds `RawExtent`), greater is an
    /// interior node (holds `RawExtentIdx`).
    pub(super) const fn depth(&self) -> u16 {
        self.depth
    }

    pub(super) const fn is_leaf(&self) -> bool {
        self.depth == 0
    }
}

impl TryFrom<&RawExtentHeader> for ExtentHeader {
    type Error = Error;

    fn try_from(raw: &RawExtentHeader) -> Result<Self> {
        if raw.magic != EXTENT_MAGIC {
            return_errno_with_message!(Errno::EUCLEAN, "bad extent header magic");
        }
        if raw.entries > raw.max {
            return_errno_with_message!(Errno::EUCLEAN, "extent header entries exceed max");
        }
        if raw.depth > 5 {
            return_errno_with_message!(Errno::EUCLEAN, "extent tree too deep");
        }
        Ok(Self {
            entries: raw.entries,
            max: raw.max,
            depth: raw.depth,
        })
    }
}

/// A decoded interior index entry.
#[derive(Clone, Copy, Debug)]
pub(super) struct ExtentIdx {
    block: Iblock,
    leaf: Ext4Bid,
}

impl ExtentIdx {
    /// Returns the first logical block this child covers.
    pub(super) const fn block(&self) -> Iblock {
        self.block
    }

    /// Returns the physical block of the child node (48-bit).
    pub(super) const fn leaf(&self) -> Ext4Bid {
        self.leaf
    }
}

impl From<&RawExtentIdx> for ExtentIdx {
    fn from(raw: &RawExtentIdx) -> Self {
        Self {
            block: raw.block,
            leaf: (raw.leaf_lo as Ext4Bid) | ((raw.leaf_hi as Ext4Bid) << 32),
        }
    }
}

/// Classifies an extent as initialized data or zero-reading preallocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ExtentKind {
    /// Data has been initialized and reads from the physical blocks.
    Written,
    /// Blocks are allocated but read as zeros until initialized.
    Unwritten,
}

impl ExtentKind {
    /// Returns whether this is an unwritten (preallocated) extent.
    pub(super) const fn is_unwritten(self) -> bool {
        matches!(self, ExtentKind::Unwritten)
    }
}

/// A decoded contiguous mapping from logical blocks to physical blocks.
#[derive(Clone, Copy, Debug)]
pub(super) struct Extent {
    block: Iblock,
    len: u16,
    start: Ext4Bid,
    kind: ExtentKind,
}

impl Extent {
    /// Builds a leaf extent mapping `len` logical blocks from logical `block` to
    /// physical `start`, written or unwritten per `kind`.
    pub(super) const fn new(block: Iblock, len: u16, start: Ext4Bid, kind: ExtentKind) -> Self {
        Self {
            block,
            len,
            start,
            kind,
        }
    }

    pub(super) fn new_tiered(block: Iblock, len: u16, start: TierBlock, kind: ExtentKind) -> Self {
        Self {
            block,
            len,
            start: start.encode().expect("validated tier address"),
            kind,
        }
    }

    /// Returns whether this extent is written or unwritten.
    pub(super) const fn kind(&self) -> ExtentKind {
        self.kind
    }

    /// Returns the first logical block this extent covers.
    pub(super) const fn block(&self) -> Iblock {
        self.block
    }

    /// Returns the number of logical blocks covered.
    pub(super) const fn len(&self) -> u16 {
        self.len
    }

    /// Returns the starting physical block (48-bit).
    pub(super) const fn start(&self) -> Ext4Bid {
        self.start
    }

    pub(in crate::fs::fs_impls::ext4) fn tier_start(&self) -> TierBlock {
        TierBlock::decode(self.start).expect("extent stores a validated 48-bit tier address")
    }

    /// Returns whether this extent is unwritten (allocated, reads as zeros).
    pub(super) const fn is_unwritten(&self) -> bool {
        self.kind.is_unwritten()
    }

    /// Returns whether `iblock` falls within this extent.
    pub(super) const fn covers(&self, iblock: Iblock) -> bool {
        iblock >= self.block && (iblock as u64) < self.block as u64 + self.len as u64
    }
}

impl TryFrom<&RawExtent> for Extent {
    type Error = Error;

    fn try_from(raw: &RawExtent) -> Result<Self> {
        let unwritten = raw.len > MAX_WRITTEN_LEN;
        let len = if unwritten {
            raw.len - MAX_WRITTEN_LEN
        } else {
            raw.len
        };
        if len == 0 {
            return_errno_with_message!(Errno::EUCLEAN, "extent has zero length");
        }
        Ok(Self {
            block: raw.block,
            len,
            start: (raw.start_lo as Ext4Bid) | ((raw.start_hi as Ext4Bid) << 32),
            kind: if unwritten {
                ExtentKind::Unwritten
            } else {
                ExtentKind::Written
            },
        })
    }
}

impl TryFrom<&Extent> for RawExtent {
    type Error = Error;

    fn try_from(ext: &Extent) -> Result<Self> {
        // Unwritten extents encode their length biased by `MAX_WRITTEN_LEN`; the
        // physical block splits into a 32-bit low half and a 16-bit high half.
        let unwritten = ext.kind.is_unwritten();
        debug_assert!(
            !unwritten || ext.len < MAX_WRITTEN_LEN,
            "unwritten extent length must be < MAX_WRITTEN_LEN to bias-encode"
        );
        let len = if unwritten {
            ext.len + MAX_WRITTEN_LEN
        } else {
            ext.len
        };
        if ext.start >> 48 != 0 {
            return_errno_with_message!(Errno::EOVERFLOW, "physical block exceeds 48 bits");
        }
        Ok(Self {
            block: ext.block,
            len,
            start_hi: u16::try_from(ext.start >> 32).expect("48-bit block has 16-bit high half"),
            start_lo: u32::try_from(ext.start & u64::from(u32::MAX))
                .expect("masked block low half fits u32"),
        })
    }
}

impl crate::fs::fs_impls::ext4::nv::persistent::Sealed for RawExtentHeader {}
impl crate::fs::fs_impls::ext4::nv::persistent::Sealed for RawExtentIdx {}
impl crate::fs::fs_impls::ext4::nv::persistent::Sealed for RawExtent {}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;
    use crate::fs::nv_pool::id::NvPageId;

    #[ktest]
    fn tier_block_round_trips_ssd_and_nv_addresses() {
        let ssd = TierBlock::Ssd((1u64 << 47) - 1);
        let nv = TierBlock::Nv(NvPageId::try_from(37usize).unwrap());

        assert_eq!(TierBlock::decode(ssd.encode().unwrap()).unwrap(), ssd);
        assert_eq!(TierBlock::decode(nv.encode().unwrap()).unwrap(), nv);
        assert!(TierBlock::Ssd(1u64 << 47).encode().is_err());
    }

    #[ktest]
    fn extent_tier_marker_does_not_change_interior_index_encoding() {
        let nv = TierBlock::Nv(NvPageId::try_from(9usize).unwrap());
        let extent = Extent::new_tiered(4, 2, nv, ExtentKind::Written);
        let raw = RawExtent::try_from(&extent).unwrap();
        let decoded = Extent::try_from(&raw).unwrap();
        assert_eq!(decoded.tier_start(), nv);
        assert_eq!(decoded.len(), 2);

        let index = RawExtentIdx::new(4, (1u64 << 47) + 9).unwrap();
        assert_eq!(ExtentIdx::from(&index).leaf(), (1u64 << 47) + 9);
    }
}
