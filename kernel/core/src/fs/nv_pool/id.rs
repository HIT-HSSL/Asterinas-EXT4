// SPDX-License-Identifier: MPL-2.0

//! Stable identifiers within an NVPool image.

macro_rules! define_pool_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
        pub(crate) struct $name(u32);

        impl $name {
            pub(crate) fn index(self) -> usize {
                self.0 as usize
            }
        }

        impl TryFrom<usize> for $name {
            type Error = core::num::TryFromIntError;

            fn try_from(value: usize) -> Result<Self, Self::Error> {
                Ok(Self(u32::try_from(value)?))
            }
        }
    };
}

define_pool_id!(NvFrameId);
define_pool_id!(NvPageId);
