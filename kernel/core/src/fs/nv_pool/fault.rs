// SPDX-License-Identifier: MPL-2.0

//! Deterministic fault points at NVPool persistence protocol boundaries.

use crate::prelude::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum FaultPoint {
    BeforeContentPersist,
    AfterContentPersist,
    AfterCommitStore,
    AfterCommitPersist,
    AfterCommitFence,
    BeforeClear,
    AfterClearStore,
    AfterClearPersist,
    AfterClearFence,
}

pub(super) fn inject(selected: Option<FaultPoint>, current: FaultPoint) -> Result<()> {
    if selected == Some(current) {
        return_errno_with_message!(Errno::EIO, "injected NVPool persistence fault");
    }
    Ok(())
}
