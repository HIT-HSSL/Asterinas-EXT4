// SPDX-License-Identifier: MPL-2.0

//! Ext4 filesystem implementation.
//!
//! The implementation supports extent-based and ext2-style indirect file I/O,
//! linear directories, special files, and extended attributes. The ext4
//! flavor also supports the runtime JBD2 subset implemented in [`journal`].
//! Metadata checksums and indexed directories are not supported yet.

// Set this module's log prefix for `ostd::log`.
macro_rules! __log_prefix {
    () => {
        "ext4: "
    };
}

pub(crate) use fs::Ext4;
pub(crate) use inode::{FilePerm, Inode};

pub(in crate::fs) use self::fs_type::{EXT2_TYPE, EXT4_TYPE};
use crate::fs::vfs::registry;

mod block_group;
mod buddy_cache;
mod extent_mutation_anchor;
mod feature;
mod fs;
mod fs_type;
mod impl_for_vfs;
mod inode;
mod journal;
mod metadata_cache;
mod nv;
mod prelude;
mod references;
mod rename_anchor;
mod stats;
mod super_block;
mod utils;
mod writeback;
mod xattr;

#[cfg(ktest)]
mod test_utils;

/// Registers the ext4 and ext2 filesystem types with the VFS registry; the
/// one driver serves both names.
pub(super) fn init() {
    registry::register(&EXT4_TYPE).unwrap();
    registry::register(&EXT2_TYPE).unwrap();
}
