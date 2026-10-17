// SPDX-License-Identifier: MPL-2.0

//! Persistent, rename-only recovery anchor.

use super::{nv::Ext4NvState, prelude::*};

pub(super) const RENAME_ANCHOR_HOME: Ext4Bid = Ext4Bid::MAX;
pub(super) const UNKNOWN_RENAME_SLOT: u64 = u64::MAX;

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct RenameAnchor {
    pub(super) src_dir: Ext4Ino,
    pub(super) dst_dir: Ext4Ino,
    pub(super) ino: Ext4Ino,
    pub(super) replaced_ino: Ext4Ino,
    pub(super) src_slot: u64,
    pub(super) dst_slot: u64,
    pub(super) src_links_before: u16,
    pub(super) src_links_after: u16,
    pub(super) dst_links_before: u16,
    pub(super) dst_links_after: u16,
    pub(super) ino_links_before: u16,
    pub(super) ino_links_after: u16,
    pub(super) replaced_links_before: u16,
    pub(super) replaced_links_after: u16,
    pub(super) committed: u64,
}

impl crate::fs::fs_impls::ext4::nv::persistent::Sealed for RenameAnchor {}

impl RenameAnchor {
    pub(super) fn new(
        src_dir: Ext4Ino,
        src_slot: usize,
        dst_dir: Ext4Ino,
        dst_slot: Option<usize>,
        ino: Ext4Ino,
        replaced_ino: Option<Ext4Ino>,
        src_links: (u16, u16),
        dst_links: (u16, u16),
        ino_links: (u16, u16),
        replaced_links: (u16, u16),
    ) -> Result<Self> {
        Ok(Self {
            src_dir,
            dst_dir,
            ino,
            replaced_ino: replaced_ino.unwrap_or(0),
            src_slot: u64::try_from(src_slot).map_err(|_| Error::new(Errno::EOVERFLOW))?,
            dst_slot: dst_slot
                .map(u64::try_from)
                .transpose()
                .map_err(|_| Error::new(Errno::EOVERFLOW))?
                .unwrap_or(UNKNOWN_RENAME_SLOT),
            src_links_before: src_links.0,
            src_links_after: src_links.1,
            dst_links_before: dst_links.0,
            dst_links_after: dst_links.1,
            ino_links_before: ino_links.0,
            ino_links_after: ino_links.1,
            replaced_links_before: replaced_links.0,
            replaced_links_after: replaced_links.1,
            committed: 1,
        })
    }

    pub(super) fn load(nv_state: &Ext4NvState) -> Result<Option<Self>> {
        if nv_state.resident_frame(RENAME_ANCHOR_HOME).is_none() {
            return Ok(None);
        }
        let mut block = [0; BLOCK_SIZE];
        nv_state
            .read_meta_block(RENAME_ANCHOR_HOME)?
            .read(&mut block)?;
        let anchor = Self::from_bytes(&block[..size_of::<Self>()]);
        if anchor.committed != 1 {
            return_errno_with_message!(Errno::EIO, "invalid rename recovery anchor");
        }
        Ok(Some(anchor))
    }

    pub(super) fn persist(&self, nv_state: &Ext4NvState) -> Result<()> {
        let mut block = [0; BLOCK_SIZE];
        block[..size_of::<Self>()].copy_from_slice(self.as_bytes());
        nv_state.commit_transient_block(RENAME_ANCHOR_HOME, &block)
    }

    pub(super) fn clear(nv_state: &Ext4NvState) -> Result<()> {
        nv_state.clear_transient_block(RENAME_ANCHOR_HOME)
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;
    use crate::fs::{
        fs_impls::ext4::test_utils::Ext4FixtureBuilder,
        nv_pool::{
            config::{NvPoolConfig, NvSimMode},
            dram::DramNvBackend,
            frame::NvPool,
        },
    };

    fn pool() -> Arc<NvPool> {
        let config = NvPoolConfig::from_bytes(
            Some(8 * PAGE_SIZE),
            Some(8 * PAGE_SIZE),
            Some(0),
            Some(NvSimMode::Fast),
        )
        .unwrap()
        .unwrap();
        let backend = Arc::new(DramNvBackend::allocate(config).unwrap());
        Arc::new(NvPool::format(backend, config, None).unwrap())
    }

    #[ktest]
    fn rename_anchor_commit_and_clear_are_persistent_pool_operations() {
        let fixture = Ext4FixtureBuilder::new(128, 128, 128)
            .with_nv_pool(pool())
            .build()
            .unwrap();
        let state = fixture.ext4.nv_state().unwrap();
        let anchor = RenameAnchor::new(
            12,
            64,
            13,
            Some(128),
            20,
            Some(21),
            (2, 1),
            (3, 4),
            (1, 1),
            (1, 0),
        )
        .unwrap();

        anchor.persist(state).unwrap();
        let mut block = [0; BLOCK_SIZE];
        state
            .read_meta_block(RENAME_ANCHOR_HOME)
            .unwrap()
            .read(&mut block)
            .unwrap();
        let restored = RenameAnchor::from_bytes(&block[..size_of::<RenameAnchor>()]);
        assert_eq!(restored.src_dir, 12);
        assert_eq!(restored.dst_dir, 13);
        assert_eq!(restored.src_slot, 64);
        assert_eq!(restored.dst_slot, 128);
        assert_eq!(restored.ino, 20);
        assert_eq!(restored.replaced_ino, 21);
        assert_eq!(restored.committed, 1);

        RenameAnchor::clear(state).unwrap();
        assert!(state.resident_frame(RENAME_ANCHOR_HOME).is_none());
    }
}
