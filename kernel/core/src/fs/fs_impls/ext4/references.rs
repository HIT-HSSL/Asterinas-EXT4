// SPDX-License-Identifier: MPL-2.0

//! Authority-aware decoding of numeric references stored in ext4 metadata.

use super::{
    fs::{Ext4, ROOT_INO},
    inode::block_mapping::extent::node::{
        EXTENT_MAGIC, MAX_WRITTEN_LEN, RawExtent, RawExtentHeader, RawExtentIdx, TierBlock,
    },
    nv::Ext4NvState,
    prelude::*,
};
use crate::fs::nv_pool::reclaim::{ObjectId, ReclaimEpoch};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Reference {
    MetadataBlock(Ext4Bid),
    DataRange {
        start: Ext4Bid,
        len: u16,
    },
    NvData {
        page: crate::fs::nv_pool::id::NvPageId,
        target: Ext4Bid,
    },
}

pub(super) struct AuthorityResolver<'state, 'allocation> {
    state: &'state Ext4NvState,
    is_ssd_allocated: &'allocation dyn Fn(Ext4Bid) -> bool,
}

impl<'state, 'allocation> AuthorityResolver<'state, 'allocation> {
    pub(super) fn new(
        state: &'state Ext4NvState,
        is_ssd_allocated: &'allocation dyn Fn(Ext4Bid) -> bool,
    ) -> Self {
        Self {
            state,
            is_ssd_allocated,
        }
    }

    pub(super) fn read(&self, home: Ext4Bid) -> Result<[u8; BLOCK_SIZE]> {
        self.state
            .read_authoritative_block(home, (self.is_ssd_allocated)(home))
    }

    fn data_target(&self, page: crate::fs::nv_pool::id::NvPageId) -> Result<Ext4Bid> {
        self.state.data_page_target(page)
    }
}

pub(super) struct Ext4ReferenceWalker<'resolver, 'state, 'allocation> {
    resolver: &'resolver AuthorityResolver<'state, 'allocation>,
}

impl<'resolver, 'state, 'allocation> Ext4ReferenceWalker<'resolver, 'state, 'allocation> {
    pub(super) fn new(resolver: &'resolver AuthorityResolver<'state, 'allocation>) -> Self {
        Self { resolver }
    }

    #[cfg(ktest)]
    pub(super) fn walk_extent_tree(&self, root: Ext4Bid) -> Result<Vec<Reference>> {
        let mut references = Vec::new();
        let mut visited = BTreeSet::new();
        self.walk_extent_node(root, &mut visited, &mut references)?;
        Ok(references)
    }

    fn walk_extent_node(
        &self,
        home: Ext4Bid,
        visited: &mut BTreeSet<Ext4Bid>,
        references: &mut Vec<Reference>,
    ) -> Result<()> {
        if !visited.insert(home) {
            return_errno_with_message!(Errno::EUCLEAN, "extent tree contains a cycle");
        }
        references.push(Reference::MetadataBlock(home));
        let block = self.resolver.read(home)?;
        self.walk_extent_bytes(&block, visited, references)
    }

    fn walk_inline_root(&self, root: &[u32]) -> Result<Vec<Reference>> {
        let mut references = Vec::new();
        self.walk_extent_bytes(root.as_bytes(), &mut BTreeSet::new(), &mut references)?;
        Ok(references)
    }

    fn walk_extent_bytes(
        &self,
        block: &[u8],
        visited: &mut BTreeSet<Ext4Bid>,
        references: &mut Vec<Reference>,
    ) -> Result<()> {
        let header = RawExtentHeader::from_bytes(&block[..size_of::<RawExtentHeader>()]);
        if header.magic != EXTENT_MAGIC || header.entries > header.max || header.depth > 5 {
            return_errno_with_message!(Errno::EUCLEAN, "invalid extent node header");
        }
        let entries = usize::from(header.entries);
        let end = size_of::<RawExtentHeader>()
            .checked_add(entries.checked_mul(size_of::<RawExtent>()).ok_or_else(|| {
                Error::with_message(Errno::EOVERFLOW, "extent entry count overflow")
            })?)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        if end > block.len() {
            return_errno_with_message!(Errno::EUCLEAN, "extent entries exceed metadata block");
        }
        for index in 0..entries {
            let offset = size_of::<RawExtentHeader>() + index * size_of::<RawExtent>();
            let bytes = &block[offset..offset + size_of::<RawExtent>()];
            if header.depth == 0 {
                let extent = RawExtent::from_bytes(bytes);
                let len = if extent.len > MAX_WRITTEN_LEN {
                    extent.len - MAX_WRITTEN_LEN
                } else {
                    extent.len
                };
                if len == 0 {
                    return_errno_with_message!(Errno::EUCLEAN, "zero-length extent");
                }
                let encoded =
                    (Ext4Bid::from(extent.start_hi) << 32) | Ext4Bid::from(extent.start_lo);
                match TierBlock::decode(encoded)? {
                    TierBlock::Ssd(start) => {
                        references.push(Reference::DataRange { start, len });
                    }
                    TierBlock::Nv(page) => {
                        for offset in 0..u32::from(len) {
                            let TierBlock::Nv(page) = TierBlock::Nv(page).offset(offset)? else {
                                unreachable!()
                            };
                            references.push(Reference::NvData {
                                page,
                                target: self.resolver.data_target(page)?,
                            });
                        }
                    }
                }
            } else {
                let child = RawExtentIdx::from_bytes(bytes);
                let child_home =
                    (Ext4Bid::from(child.leaf_hi) << 32) | Ext4Bid::from(child.leaf_lo);
                self.walk_extent_node(child_home, visited, references)?;
            }
        }
        Ok(())
    }

    pub(super) fn mark_mounted_filesystem(
        fs: &Ext4,
        epoch: &mut ReclaimEpoch,
    ) -> Result<BTreeSet<crate::fs::nv_pool::id::NvPageId>> {
        let nv_state = fs
            .nv_state()
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "NV recovery requires NVPool"))?;
        let allocated = |home| fs.is_block_allocated(home);
        let resolver = AuthorityResolver::new(nv_state, &allocated);
        let walker = Ext4ReferenceWalker::new(&resolver);
        let mut nv_pages = BTreeSet::new();
        let mut pending = VecDeque::from([ROOT_INO]);
        while let Some(ino) = pending.pop_front() {
            if !epoch.visit(ObjectId::Inode(ino)) {
                continue;
            }
            let inode = fs.read_inode(ino)?;
            for reference in walker.walk_inline_root(&inode.reclaim_extent_root()?)? {
                match reference {
                    Reference::MetadataBlock(block) => {
                        epoch.visit(ObjectId::Block(block));
                    }
                    Reference::DataRange { start, len } => {
                        for block in start..start + Ext4Bid::from(len) {
                            epoch.visit(ObjectId::Block(block));
                        }
                    }
                    Reference::NvData { page, target } => {
                        nv_pages.insert(page);
                        epoch.visit(ObjectId::Block(target));
                    }
                }
            }
            if let Some(block) = inode.reclaim_xattr_block() {
                epoch.visit(ObjectId::Block(block));
            }
            pending.extend(inode.reclaim_directory_children()?);
        }
        Ok(nv_pages)
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;
    use crate::fs::{
        fs_impls::ext4::test_utils::Ext4MemoryDisk,
        nv_pool::{
            config::{NvPoolConfig, NvSimMode},
            dram::DramNvBackend,
            frame::NvPool,
        },
    };

    fn pool() -> Arc<NvPool> {
        let config = NvPoolConfig::from_bytes(
            Some(16 * PAGE_SIZE),
            Some(16 * PAGE_SIZE),
            Some(0),
            Some(NvSimMode::Fast),
        )
        .unwrap()
        .unwrap();
        let backend = Arc::new(DramNvBackend::allocate(config).unwrap());
        Arc::new(NvPool::format(backend, config, None).unwrap())
    }

    fn extent_node(depth: u16, target: Ext4Bid) -> [u8; BLOCK_SIZE] {
        let mut block = [0; BLOCK_SIZE];
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 1,
            max: 1,
            depth,
            generation: 0,
        };
        block[..size_of::<RawExtentHeader>()].copy_from_slice(header.as_bytes());
        if depth == 0 {
            let extent = RawExtent {
                block: 0,
                len: 1,
                start_hi: u16::try_from(target >> 32).unwrap(),
                start_lo: target as u32,
            };
            block[12..24].copy_from_slice(extent.as_bytes());
        } else {
            let index = RawExtentIdx {
                block: 0,
                leaf_lo: target as u32,
                leaf_hi: u16::try_from(target >> 32).unwrap(),
                unused: 0,
            };
            block[12..24].copy_from_slice(index.as_bytes());
        }
        block
    }

    fn install(
        state: &Ext4NvState,
        disk: &Ext4MemoryDisk,
        home: Ext4Bid,
        bytes: &[u8; BLOCK_SIZE],
        in_nv: bool,
    ) {
        disk.segment()
            .write_bytes(Bid::new(home).to_offset(), bytes)
            .unwrap();
        if in_nv {
            state
                .update_meta_block(home, |content| {
                    content.copy_from_slice(bytes);
                    Ok(())
                })
                .unwrap();
        }
    }

    #[ktest]
    fn extent_walk_handles_all_parent_child_authority_combinations() {
        for (parent_nv, child_nv) in [(true, false), (false, true), (true, true), (false, false)] {
            let disk = Arc::new(Ext4MemoryDisk::new(64));
            let state = Ext4NvState::new(
                pool(),
                disk.clone() as Arc<dyn BlockDevice>,
                BLOCK_SIZE,
                [0x34; 16],
            )
            .unwrap();
            install(&state, &disk, 8, &extent_node(1, 9), parent_nv);
            install(&state, &disk, 9, &extent_node(0, 24), child_nv);
            let resolver = AuthorityResolver::new(&state, &|block| matches!(block, 8 | 9));
            let walker = Ext4ReferenceWalker::new(&resolver);

            assert_eq!(
                walker.walk_extent_tree(8).unwrap(),
                vec![
                    Reference::MetadataBlock(8),
                    Reference::MetadataBlock(9),
                    Reference::DataRange { start: 24, len: 1 },
                ]
            );
        }
    }
}
