// SPDX-License-Identifier: MPL-2.0

//! The JBD2 on-disk journal format.
//!
//! This module defines the byte-for-byte layout of the log that jbd2 (the Linux
//! journaling block device) writes into the journal inode. Our images share this
//! format with Linux exactly, so `e2fsck` and a stock Linux kernel can recover
//! and interpret a journal we wrote, and vice versa.
//!
//! # Big-endian
//!
//! Unlike the ext4 filesystem proper (which is little-endian), **every
//! multi-byte field of the jbd2 on-disk format is big-endian.** Each such field
//! is therefore stored in a [`Be16`]/[`Be32`]/[`Be64`] newtype that keeps the
//! bytes in big-endian order on disk and converts on access; the raw structs are
//! never read as native-endian integers.
//!
//! # Supported features (the Phase 4 subset)
//!
//! Phase 4 parses only the journal layout it can honor. Of the jbd2 INCOMPAT
//! features, it tolerates **only** [`INCOMPAT_REVOKE`] — and even then it merely
//! *ignores* revoke blocks during recovery (the documented "revoke gap"; full
//! revoke support is Phase 7). Every other INCOMPAT feature changes the on-disk
//! layout we cannot yet parse and is rejected:
//!
//! - [`INCOMPAT_64BIT`] widens block tags with a `t_blocknr_high` word (so
//!   [`RawBlockTag`] would no longer be 8 bytes),
//! - [`INCOMPAT_CSUM_V2`]/[`INCOMPAT_CSUM_V3`] add per-block/per-tag checksums,
//! - [`INCOMPAT_ASYNC_COMMIT`] removes the trailing commit-block barrier,
//! - [`INCOMPAT_FAST_COMMIT`] adds a wholly different fast-commit area.
//!
//! See [`INCOMPAT_SUPP`] for the resulting support mask.

use core::fmt;

use super::{super::prelude::*, Tid};

/// A big-endian `u16` as stored on disk (jbd2 is big-endian, unlike ext4 proper).
///
/// Backed by raw bytes (alignment 1) so it never forces padding into the packed
/// on-disk structs it appears in.
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, PartialEq, Eq)]
pub(super) struct Be16([u8; 2]);

impl Be16 {
    /// Wraps a host-order value for on-disk storage (converts to big-endian).
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) const fn new(v: u16) -> Self {
        Self(v.to_be_bytes())
    }

    /// Returns the logical (host-order) value.
    pub(super) const fn get(self) -> u16 {
        u16::from_be_bytes(self.0)
    }
}

impl Debug for Be16 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Print the logical value; the raw bytes are big-endian.
        Debug::fmt(&self.get(), f)
    }
}

/// A big-endian `u32` as stored on disk (jbd2 is big-endian, unlike ext4 proper).
///
/// Backed by raw bytes (alignment 1) so it never forces padding into the packed
/// on-disk structs it appears in.
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, PartialEq, Eq)]
pub(super) struct Be32([u8; 4]);

impl Be32 {
    /// Wraps a host-order value for on-disk storage (converts to big-endian).
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) const fn new(v: u32) -> Self {
        Self(v.to_be_bytes())
    }

    /// Returns the logical (host-order) value.
    pub(super) const fn get(self) -> u32 {
        u32::from_be_bytes(self.0)
    }
}

impl Debug for Be32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Print the logical value; the raw bytes are big-endian.
        Debug::fmt(&self.get(), f)
    }
}

/// A big-endian `u64` as stored on disk (jbd2 is big-endian, unlike ext4 proper).
///
/// Backed by raw bytes (alignment 1) so it never forces padding into the packed
/// on-disk structs it appears in (e.g. the 60-byte commit block's unaligned
/// `h_commit_sec`).
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, PartialEq, Eq)]
pub(super) struct Be64([u8; 8]);

impl Be64 {
    /// Wraps a host-order value for on-disk storage (converts to big-endian).
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) const fn new(v: u64) -> Self {
        Self(v.to_be_bytes())
    }

    /// Returns the logical (host-order) value.
    pub(super) const fn get(self) -> u64 {
        u64::from_be_bytes(self.0)
    }
}

impl Debug for Be64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Print the logical value; the raw bytes are big-endian.
        Debug::fmt(&self.get(), f)
    }
}

/// Magic signature at the start of every jbd2 block header (`JBD2_MAGIC_NUMBER`).
pub(super) const JBD2_MAGIC: u32 = 0xC03B_3998;

/// Descriptor block: lists the filesystem blocks that follow in this
/// transaction (`JBD2_DESCRIPTOR_BLOCK`).
#[cfg_attr(not(ktest), expect(dead_code))]
pub(super) const BLOCKTYPE_DESCRIPTOR: u32 = 1;
/// Commit block: seals a transaction (`JBD2_COMMIT_BLOCK`).
#[cfg_attr(not(ktest), expect(dead_code))]
pub(super) const BLOCKTYPE_COMMIT: u32 = 2;
/// Version-1 journal superblock (`JBD2_SUPERBLOCK_V1`).
pub(super) const BLOCKTYPE_SUPERBLOCK_V1: u32 = 3;
/// Version-2 journal superblock (`JBD2_SUPERBLOCK_V2`).
pub(super) const BLOCKTYPE_SUPERBLOCK_V2: u32 = 4;
/// Revoke block: lists blocks that must not be replayed (`JBD2_REVOKE_BLOCK`).
#[cfg_attr(not(ktest), expect(dead_code))]
pub(super) const BLOCKTYPE_REVOKE: u32 = 5;

/// The journaled block was escaped because it began with [`JBD2_MAGIC`]
/// (`JBD2_FLAG_ESCAPE`).
#[cfg_attr(not(ktest), expect(dead_code))]
pub(super) const TAG_FLAG_ESCAPE: u16 = 1;
/// This tag reuses the UUID of the previous tag (`JBD2_FLAG_SAME_UUID`).
#[cfg_attr(not(ktest), expect(dead_code))]
pub(super) const TAG_FLAG_SAME_UUID: u16 = 2;
/// The tagged block was deleted (`JBD2_FLAG_DELETED`).
#[cfg_attr(not(ktest), expect(dead_code))]
pub(super) const TAG_FLAG_DELETED: u16 = 4;
/// This is the last tag in the descriptor block (`JBD2_FLAG_LAST_TAG`).
#[cfg_attr(not(ktest), expect(dead_code))]
pub(super) const TAG_FLAG_LAST_TAG: u16 = 8;

/// Revoke records are present (`JBD2_FEATURE_INCOMPAT_REVOKE`). Tolerated by
/// Phase 4, which ignores revoke blocks during recovery (see the module docs).
pub(super) const INCOMPAT_REVOKE: u32 = 0x1;
/// 64-bit block numbers in block tags (`JBD2_FEATURE_INCOMPAT_64BIT`).
#[cfg_attr(not(ktest), expect(dead_code))]
pub(super) const INCOMPAT_64BIT: u32 = 0x2;
/// Commit blocks may be written before their data is durable
/// (`JBD2_FEATURE_INCOMPAT_ASYNC_COMMIT`).
#[cfg_attr(not(ktest), expect(dead_code))]
pub(super) const INCOMPAT_ASYNC_COMMIT: u32 = 0x4;
/// Version-2 checksums (`JBD2_FEATURE_INCOMPAT_CSUM_V2`).
#[cfg_attr(not(ktest), expect(dead_code))]
pub(super) const INCOMPAT_CSUM_V2: u32 = 0x8;
/// Version-3 checksums (`JBD2_FEATURE_INCOMPAT_CSUM_V3`).
#[cfg_attr(not(ktest), expect(dead_code))]
pub(super) const INCOMPAT_CSUM_V3: u32 = 0x10;
/// Fast-commit area is present (`JBD2_FEATURE_INCOMPAT_FAST_COMMIT`).
#[cfg_attr(not(ktest), expect(dead_code))]
pub(super) const INCOMPAT_FAST_COMMIT: u32 = 0x20;

/// The jbd2 INCOMPAT features Phase 4 supports.
///
/// Only [`INCOMPAT_REVOKE`] is tolerated (and its blocks are ignored during
/// recovery, the "revoke gap" — full support is Phase 7). Every other INCOMPAT
/// feature (64-bit tags, async commit, csum v2/v3, fast commit) changes the
/// on-disk layout this phase cannot parse, so a journal carrying one is
/// rejected rather than silently misread.
pub(super) const INCOMPAT_SUPP: u32 = INCOMPAT_REVOKE;

/// The 12-byte header shared by every jbd2 log block (`journal_header_t`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(super) struct RawJournalHeader {
    /// [`JBD2_MAGIC`] (`h_magic`).
    pub(super) h_magic: Be32,
    /// One of the `BLOCKTYPE_*` constants (`h_blocktype`).
    pub(super) h_blocktype: Be32,
    /// Transaction id this block belongs to (`h_sequence`).
    pub(super) h_sequence: Be32,
}

const JOURNAL_HEADER_SIZE: usize = 12;
const_assert!(size_of::<RawJournalHeader>() == JOURNAL_HEADER_SIZE);

/// Fixed-length padding for the 40-word reserved tail of the journal
/// superblock. Derived `Default` does not cover arrays longer than 32, so this
/// newtype supplies a manual one (mirroring `InodeTail`/`Reserved` elsewhere).
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct SuperblockPadding([Be32; 40]);

impl Default for SuperblockPadding {
    fn default() -> Self {
        Self([Be32::default(); 40])
    }
}

/// Fixed-length padding for the 768-byte `s_users` array of the journal
/// superblock (external-journal client UUIDs; unused here). Supplies a manual
/// `Default` for the same reason as [`SuperblockPadding`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct SuperblockUsers([u8; 768]);

impl Default for SuperblockUsers {
    fn default() -> Self {
        Self([0u8; 768])
    }
}

/// The on-disk journal superblock (`journal_superblock_t`, exactly 1024 bytes).
///
/// It occupies log block 0. Convert to [`JournalSuperblock`] via `TryFrom` for
/// the validated representation. Field names mirror jbd2's `s_*`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(super) struct RawJournalSuperblock {
    /// Shared header; `h_blocktype` is `BLOCKTYPE_SUPERBLOCK_V1`/`_V2`.
    pub(super) header: RawJournalHeader,
    /// Journal device block size in bytes (`s_blocksize`).
    pub(super) s_blocksize: Be32,
    /// Total number of blocks in the journal (`s_maxlen`).
    pub(super) s_maxlen: Be32,
    /// First log block holding log data, past the superblock (`s_first`).
    pub(super) s_first: Be32,
    /// First transaction id expected on recovery (`s_sequence`).
    pub(super) s_sequence: Be32,
    /// Log block where recovery starts; 0 means clean (`s_start`).
    pub(super) s_start: Be32,
    /// Error number recorded by the journal (`s_errno`).
    pub(super) s_errno: Be32,
    /// Compatible feature bits (`s_feature_compat`).
    pub(super) s_feature_compat: Be32,
    /// Incompatible feature bits (`s_feature_incompat`).
    pub(super) s_feature_incompat: Be32,
    /// Read-only compatible feature bits (`s_feature_ro_compat`).
    pub(super) s_feature_ro_compat: Be32,
    /// Journal UUID (`s_uuid`).
    pub(super) s_uuid: [u8; 16],
    /// Number of filesystems sharing this journal (`s_nr_users`).
    pub(super) s_nr_users: Be32,
    /// Block of the dynamic superblock copy (`s_dynsuper`).
    pub(super) s_dynsuper: Be32,
    /// Limit on the blocks per transaction (`s_max_transaction`).
    pub(super) s_max_transaction: Be32,
    /// Limit on the data blocks per transaction (`s_max_trans_data`).
    pub(super) s_max_trans_data: Be32,
    /// Checksum algorithm for the journal (`s_checksum_type`).
    pub(super) s_checksum_type: u8,
    /// Padding after `s_checksum_type` (`s_padding2`).
    pub(super) s_padding2: [u8; 3],
    /// Number of fast-commit blocks (`s_num_fc_blks`).
    pub(super) s_num_fc_blks: Be32,
    /// Head of the fast-commit list (`s_head`).
    pub(super) s_head: Be32,
    /// Reserved padding to the checksum field (`s_padding`).
    pub(super) s_padding: SuperblockPadding,
    /// crc32c of the superblock (`s_checksum`).
    pub(super) s_checksum: Be32,
    /// External-journal client UUIDs (`s_users`).
    pub(super) s_users: SuperblockUsers,
}

const JOURNAL_SUPERBLOCK_SIZE: usize = 1024;
const_assert!(size_of::<RawJournalSuperblock>() == JOURNAL_SUPERBLOCK_SIZE);

/// An 8-byte block tag in a descriptor block (`journal_block_tag_t` without the
/// 64-bit / checksum extensions).
///
/// jbd2's `t_blocknr_high` word (present only with [`INCOMPAT_64BIT`], which we
/// reject) is deliberately omitted so this struct stays 8 bytes, matching the
/// only tag layout Phase 4 parses.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(super) struct RawBlockTag {
    /// Low 32 bits of the target filesystem block (`t_blocknr`).
    pub(super) t_blocknr: Be32,
    /// Tag checksum, zero without the csum feature (`t_checksum`).
    pub(super) t_checksum: Be16,
    /// `TAG_FLAG_*` bits (`t_flags`).
    pub(super) t_flags: Be16,
}

const BLOCK_TAG_SIZE: usize = 8;
const_assert!(size_of::<RawBlockTag>() == BLOCK_TAG_SIZE);

/// The on-disk commit block (`commit_header`, exactly 60 bytes) that seals a
/// transaction.
///
/// Phase 4 writes the checksum fields as zero; journal checksums arrive in
/// Phase 6/7.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(super) struct RawCommitBlock {
    /// Shared header; `h_blocktype` is [`BLOCKTYPE_COMMIT`].
    pub(super) header: RawJournalHeader,
    /// Checksum algorithm (`h_chksum_type`).
    pub(super) h_chksum_type: u8,
    /// Checksum size in bytes (`h_chksum_size`).
    pub(super) h_chksum_size: u8,
    /// Padding after the checksum descriptor (`h_padding`).
    pub(super) h_padding: [u8; 2],
    /// Commit-block checksum words (`h_chksum`).
    pub(super) h_chksum: [Be32; 8],
    /// Commit time, seconds (`h_commit_sec`).
    pub(super) h_commit_sec: Be64,
    /// Commit time, nanoseconds (`h_commit_nsec`).
    pub(super) h_commit_nsec: Be32,
}

const COMMIT_BLOCK_SIZE: usize = 60;
const_assert!(size_of::<RawCommitBlock>() == COMMIT_BLOCK_SIZE);

/// A validated, Rust-typed view of the journal superblock.
///
/// Built from [`RawJournalSuperblock`] via `TryFrom`, which rejects a superblock
/// whose magic, block type, block size, geometry, or feature set Phase 4 cannot
/// honor.
#[derive(Clone, Copy, Debug)]
pub(super) struct JournalSuperblock {
    /// Total number of log blocks (`s_maxlen`).
    maxlen: u32,
    /// First log block holding log data, past the superblock (`s_first`).
    first: u32,
    /// First transaction id expected on recovery (`s_sequence`).
    sequence: Tid,
    /// Log block where recovery starts; 0 means clean (`s_start`).
    start: u32,
    /// Journal block size in bytes (`s_blocksize`).
    blocksize: u32,
}

impl TryFrom<RawJournalSuperblock> for JournalSuperblock {
    type Error = Error;

    fn try_from(raw: RawJournalSuperblock) -> Result<Self> {
        if raw.header.h_magic.get() != JBD2_MAGIC {
            return_errno_with_message!(Errno::EINVAL, "bad jbd2 journal superblock magic");
        }

        let blocktype = raw.header.h_blocktype.get();
        if blocktype != BLOCKTYPE_SUPERBLOCK_V2 && blocktype != BLOCKTYPE_SUPERBLOCK_V1 {
            return_errno_with_message!(Errno::EINVAL, "journal superblock has a bad block type");
        }

        let blocksize = raw.s_blocksize.get();
        if blocksize != BLOCK_SIZE as u32 {
            return_errno_with_message!(
                Errno::EINVAL,
                "unsupported journal block size (4 KiB only)"
            );
        }

        let maxlen = raw.s_maxlen.get();
        if maxlen < 2 {
            return_errno_with_message!(Errno::EUCLEAN, "journal is too small");
        }

        let first = raw.s_first.get();
        if first == 0 || first >= maxlen {
            return_errno_with_message!(Errno::EUCLEAN, "journal s_first out of range");
        }

        let feature_incompat = raw.s_feature_incompat.get();
        if feature_incompat & !INCOMPAT_SUPP != 0 {
            return_errno_with_message!(
                Errno::EINVAL,
                "journal has an unsupported incompatible feature"
            );
        }

        let start = raw.s_start.get();
        if start != 0 {
            return_errno_with_message!(
                Errno::EUCLEAN,
                "journal recovery is required but is not implemented"
            );
        }

        Ok(Self {
            maxlen,
            first,
            // `s_sequence` is the first transaction id; recovery starts here.
            sequence: raw.s_sequence.get(),
            // `s_start` may be nonzero: a dirty journal awaiting recovery. Just
            // record it.
            start,
            blocksize,
        })
    }
}

impl JournalSuperblock {
    /// Returns the total number of log blocks (`s_maxlen`).
    pub(super) const fn maxlen(&self) -> u32 {
        self.maxlen
    }

    /// Returns the first log block that holds log data (`s_first`).
    pub(super) const fn first(&self) -> u32 {
        self.first
    }

    /// Returns the first transaction id expected on recovery (`s_sequence`).
    pub(super) const fn sequence(&self) -> Tid {
        self.sequence
    }

    /// Returns the log block where recovery starts; 0 means clean (`s_start`).
    pub(super) const fn start(&self) -> u32 {
        self.start
    }

    /// Returns the journal block size in bytes (`s_blocksize`).
    pub(super) const fn blocksize(&self) -> u32 {
        self.blocksize
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    /// Builds a minimal valid raw journal superblock with the given geometry.
    fn valid_raw(maxlen: u32, first: u32, sequence: u32, start: u32) -> RawJournalSuperblock {
        RawJournalSuperblock {
            header: RawJournalHeader {
                h_magic: Be32::new(JBD2_MAGIC),
                h_blocktype: Be32::new(BLOCKTYPE_SUPERBLOCK_V2),
                h_sequence: Be32::new(0),
            },
            s_blocksize: Be32::new(BLOCK_SIZE as u32),
            s_maxlen: Be32::new(maxlen),
            s_first: Be32::new(first),
            s_sequence: Be32::new(sequence),
            s_start: Be32::new(start),
            s_nr_users: Be32::new(1),
            ..Default::default()
        }
    }

    #[ktest]
    fn be_newtypes_round_trip() {
        assert_eq!(Be16::new(0x1234).get(), 0x1234);
        assert_eq!(Be32::new(0xC03B_3998).get(), 0xC03B_3998);
        assert_eq!(
            Be64::new(0x0123_4567_89AB_CDEF).get(),
            0x0123_4567_89AB_CDEF
        );
        // The stored bytes are byte-swapped relative to host order.
        assert_eq!(Be32::new(0xC03B_3998).as_bytes(), &[0xC0, 0x3B, 0x39, 0x98]);
    }

    /// The on-disk journal superblock must reproduce the ground-truth bytes of a
    /// real `mke2fs`-made journal (physical block 521 of a `-O has_journal`
    /// image): magic, block type, and the leading geometry fields.
    #[ktest]
    fn journal_superblock_ground_truth_bytes() {
        let raw = valid_raw(1024, 1, 1, 0);
        let bytes = raw.as_bytes();
        // h_magic = 0xC03B3998, h_blocktype = 4 (SUPERBLOCK_V2), h_sequence = 0.
        assert_eq!(&bytes[0..4], &[0xC0, 0x3B, 0x39, 0x98]);
        assert_eq!(&bytes[4..8], &[0x00, 0x00, 0x00, 0x04]);
        assert_eq!(&bytes[8..12], &[0x00, 0x00, 0x00, 0x00]);
        // s_blocksize = 4096, s_maxlen = 1024, s_first = 1.
        assert_eq!(&bytes[12..16], &[0x00, 0x00, 0x10, 0x00]);
        assert_eq!(&bytes[16..20], &[0x00, 0x00, 0x04, 0x00]);
        assert_eq!(&bytes[20..24], &[0x00, 0x00, 0x00, 0x01]);
        // s_sequence = 1.
        assert_eq!(&bytes[24..28], &[0x00, 0x00, 0x00, 0x01]);
        // s_nr_users = 1 (offset 64).
        assert_eq!(&bytes[64..68], &[0x00, 0x00, 0x00, 0x01]);
    }

    #[ktest]
    fn parse_valid_journal_superblock() {
        let raw = valid_raw(1024, 1, 7, 0);
        let sb = JournalSuperblock::try_from(raw).unwrap();
        assert_eq!(sb.maxlen(), 1024);
        assert_eq!(sb.first(), 1);
        assert_eq!(sb.sequence(), 7);
        assert_eq!(sb.start(), 0);
        assert_eq!(sb.blocksize(), BLOCK_SIZE as u32);
    }

    #[ktest]
    fn reject_dirty_journal_without_recovery_support() {
        let raw = valid_raw(1024, 1, 7, 3);
        let err = JournalSuperblock::try_from(raw).unwrap_err();
        assert_eq!(err.error(), Errno::EUCLEAN);
    }

    #[ktest]
    fn reject_bad_magic() {
        let mut raw = valid_raw(1024, 1, 1, 0);
        raw.header.h_magic = Be32::new(0x1234_5678);
        assert!(JournalSuperblock::try_from(raw).is_err());
    }

    #[ktest]
    fn reject_bad_blocktype() {
        let mut raw = valid_raw(1024, 1, 1, 0);
        raw.header.h_blocktype = Be32::new(BLOCKTYPE_DESCRIPTOR);
        assert!(JournalSuperblock::try_from(raw).is_err());
    }

    #[ktest]
    fn accept_superblock_v1_blocktype() {
        let mut raw = valid_raw(1024, 1, 1, 0);
        raw.header.h_blocktype = Be32::new(BLOCKTYPE_SUPERBLOCK_V1);
        assert!(JournalSuperblock::try_from(raw).is_ok());
    }

    #[ktest]
    fn reject_bad_blocksize() {
        let mut raw = valid_raw(1024, 1, 1, 0);
        raw.s_blocksize = Be32::new(1024);
        assert!(JournalSuperblock::try_from(raw).is_err());
    }

    #[ktest]
    fn reject_bad_first() {
        let raw = valid_raw(1024, 0, 1, 0); // s_first == 0
        assert!(JournalSuperblock::try_from(raw).is_err());

        let raw = valid_raw(4, 4, 1, 0); // s_first == s_maxlen
        assert!(JournalSuperblock::try_from(raw).is_err());
    }

    #[ktest]
    fn reject_too_small() {
        let raw = valid_raw(1, 1, 1, 0);
        assert!(JournalSuperblock::try_from(raw).is_err());
    }

    #[ktest]
    fn reject_unsupported_incompat() {
        let mut raw = valid_raw(1024, 1, 1, 0);
        raw.s_feature_incompat = Be32::new(INCOMPAT_64BIT);
        assert!(JournalSuperblock::try_from(raw).is_err());

        let mut raw = valid_raw(1024, 1, 1, 0);
        raw.s_feature_incompat = Be32::new(INCOMPAT_CSUM_V3);
        assert!(JournalSuperblock::try_from(raw).is_err());
    }

    #[ktest]
    fn accept_revoke_incompat() {
        let mut raw = valid_raw(1024, 1, 1, 0);
        raw.s_feature_incompat = Be32::new(INCOMPAT_REVOKE);
        assert!(JournalSuperblock::try_from(raw).is_ok());
    }
}
