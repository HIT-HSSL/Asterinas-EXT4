// SPDX-License-Identifier: MPL-2.0

//! Runtime state machine for NV-resident data pages.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DataPageState {
    Free,
    Staged,
    NvDirty,
    InFlight,
    Synced,
}

impl DataPageState {
    pub(super) fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Staged, Self::NvDirty)
                | (Self::NvDirty, Self::InFlight)
                | (Self::InFlight, Self::NvDirty)
                | (Self::InFlight, Self::Synced)
                | (Self::Synced, Self::NvDirty)
        )
    }

    pub(super) fn can_free(self) -> bool {
        matches!(self, Self::Staged | Self::Synced)
    }
}
