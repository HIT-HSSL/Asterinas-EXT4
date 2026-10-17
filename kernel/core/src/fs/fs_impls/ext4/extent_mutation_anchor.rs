// SPDX-License-Identifier: MPL-2.0

//! Fixed-size recovery anchor for one in-flight extent-tree mutation.

use super::{nv::Ext4NvState, prelude::*};

const EXTENT_MUTATION_MAGIC: u64 = 0x4558_544d_5554_3031;
const EXTENT_MUTATION_HOME_TAG: u64 = 1 << 63;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u64)]
pub(super) enum ExtentMutationKind {
    Split = 1,
    Remove = 2,
    Merge = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u64)]
pub(super) enum ExtentMutationPhase {
    Prepared = 1,
    ParentPublished = 2,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct PendingExtentMutation {
    magic: u64,
    ino: u64,
    kind: u64,
    phase: u64,
    parent: u64,
    old_child: u64,
    new_child: u64,
    split_iblock: u64,
    old_len_before: u64,
    committed: u64,
}

impl super::nv::persistent::Sealed for PendingExtentMutation {}

impl PendingExtentMutation {
    pub(super) fn split(
        ino: Ext4Ino,
        parent: Ext4Bid,
        old_child: Ext4Bid,
        new_child: Ext4Bid,
        split_iblock: Iblock,
        old_len_before: u16,
    ) -> Self {
        Self {
            magic: EXTENT_MUTATION_MAGIC,
            ino: u64::from(ino),
            kind: ExtentMutationKind::Split as u64,
            phase: ExtentMutationPhase::Prepared as u64,
            parent,
            old_child,
            new_child,
            split_iblock: u64::from(split_iblock),
            old_len_before: u64::from(old_len_before),
            committed: 1,
        }
    }

    pub(super) fn ino(&self) -> Result<Ext4Ino> {
        Ext4Ino::try_from(self.ino).map_err(|_| Error::new(Errno::EOVERFLOW))
    }

    pub(super) fn kind(&self) -> Result<ExtentMutationKind> {
        match self.kind {
            value if value == ExtentMutationKind::Split as u64 => Ok(ExtentMutationKind::Split),
            value if value == ExtentMutationKind::Remove as u64 => Ok(ExtentMutationKind::Remove),
            value if value == ExtentMutationKind::Merge as u64 => Ok(ExtentMutationKind::Merge),
            _ => return_errno_with_message!(Errno::EIO, "invalid extent mutation kind"),
        }
    }

    pub(super) fn phase(&self) -> Result<ExtentMutationPhase> {
        match self.phase {
            value if value == ExtentMutationPhase::Prepared as u64 => {
                Ok(ExtentMutationPhase::Prepared)
            }
            value if value == ExtentMutationPhase::ParentPublished as u64 => {
                Ok(ExtentMutationPhase::ParentPublished)
            }
            _ => return_errno_with_message!(Errno::EIO, "invalid extent mutation phase"),
        }
    }

    pub(super) fn set_phase(&mut self, phase: ExtentMutationPhase) {
        self.phase = phase as u64;
    }

    pub(super) const fn parent(&self) -> Ext4Bid {
        self.parent
    }

    pub(super) const fn old_child(&self) -> Ext4Bid {
        self.old_child
    }

    pub(super) const fn new_child(&self) -> Ext4Bid {
        self.new_child
    }

    pub(super) fn split_iblock(&self) -> Result<Iblock> {
        Iblock::try_from(self.split_iblock).map_err(|_| Error::new(Errno::EOVERFLOW))
    }

    pub(super) fn persist(&self, nv_state: &Ext4NvState) -> Result<()> {
        let mut block = [0; BLOCK_SIZE];
        block[..size_of::<Self>()].copy_from_slice(self.as_bytes());
        nv_state.commit_transient_block(Self::home(self.ino()?), &block)
    }

    pub(super) fn load(nv_state: &Ext4NvState, ino: Ext4Ino) -> Result<Option<Self>> {
        let home = Self::home(ino);
        if nv_state.resident_frame(home).is_none() {
            return Ok(None);
        }
        let mut block = [0; BLOCK_SIZE];
        nv_state.read_meta_block(home)?.read(&mut block)?;
        let mutation = Self::from_bytes(&block[..size_of::<Self>()]);
        if mutation.magic != EXTENT_MUTATION_MAGIC
            || mutation.committed != 1
            || mutation.ino()? != ino
        {
            return_errno_with_message!(Errno::EIO, "invalid extent mutation anchor");
        }
        mutation.kind()?;
        mutation.phase()?;
        Ok(Some(mutation))
    }

    pub(super) fn clear(nv_state: &Ext4NvState, ino: Ext4Ino) -> Result<()> {
        nv_state.clear_transient_block(Self::home(ino))
    }

    pub(super) const fn home(ino: Ext4Ino) -> Ext4Bid {
        EXTENT_MUTATION_HOME_TAG | ino as u64
    }
}

pub(super) const fn is_extent_mutation_home(home: Ext4Bid) -> bool {
    home & EXTENT_MUTATION_HOME_TAG != 0 && home != super::rename_anchor::RENAME_ANCHOR_HOME
}

pub(super) fn ino_from_extent_mutation_home(home: Ext4Bid) -> Result<Ext4Ino> {
    if !is_extent_mutation_home(home) {
        return_errno_with_message!(Errno::EINVAL, "not an extent mutation anchor home");
    }
    Ext4Ino::try_from(home & u64::from(u32::MAX)).map_err(|_| Error::new(Errno::EOVERFLOW))
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

    #[ktest]
    fn extent_mutation_anchor_is_fixed_size_persistent_state() {
        let fixture = Ext4FixtureBuilder::new(128, 128, 128)
            .with_nv_pool(pool())
            .build()
            .unwrap();
        let nv_state = fixture.ext4.nv_state().unwrap();
        let mutation = PendingExtentMutation::split(17, 200, 201, 202, 64, 8);

        mutation.persist(nv_state).unwrap();
        let restored = PendingExtentMutation::load(nv_state, 17).unwrap().unwrap();

        assert_eq!(restored.ino().unwrap(), 17);
        assert_eq!(restored.kind().unwrap(), ExtentMutationKind::Split);
        assert_eq!(restored.phase().unwrap(), ExtentMutationPhase::Prepared);
        assert!(is_extent_mutation_home(PendingExtentMutation::home(17)));
        PendingExtentMutation::clear(nv_state, 17).unwrap();
        assert!(PendingExtentMutation::load(nv_state, 17).unwrap().is_none());
    }
}
