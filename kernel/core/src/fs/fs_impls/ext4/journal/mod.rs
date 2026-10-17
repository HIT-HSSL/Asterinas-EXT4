// SPDX-License-Identifier: MPL-2.0

//! Runtime JBD2 support for ext4.
//!
//! This branch deliberately supports clean mounts and runtime metadata
//! journaling only. A dirty journal is rejected until replay is implemented.

#[cfg(any(feature = "ext4-research-stats", ktest))]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::{AtomicBool, Ordering};

use self::format::{
    BLOCKTYPE_COMMIT, BLOCKTYPE_DESCRIPTOR, Be16, Be32, Be64, JBD2_MAGIC, JournalSuperblock,
    RawBlockTag, RawCommitBlock, RawJournalHeader, RawJournalSuperblock, TAG_FLAG_ESCAPE,
    TAG_FLAG_LAST_TAG, TAG_FLAG_SAME_UUID,
};
use super::{feature::FeatureCompatSet, fs::Ext4, prelude::*};

pub(in super::super) mod format;

pub(super) type Tid = u32;

pub(super) const JOURNAL_INO: Ext4Ino = 8;

#[cfg(ktest)]
pub(in super::super) fn make_test_journal_superblock(maxlen: u32, start: u32) -> [u8; 1024] {
    let raw = RawJournalSuperblock {
        header: RawJournalHeader {
            h_magic: Be32::new(JBD2_MAGIC),
            h_blocktype: Be32::new(format::BLOCKTYPE_SUPERBLOCK_V2),
            h_sequence: Be32::new(0),
        },
        s_blocksize: Be32::new(BLOCK_SIZE as u32),
        s_maxlen: Be32::new(maxlen),
        s_first: Be32::new(1),
        s_sequence: Be32::new(1),
        s_start: Be32::new(start),
        s_nr_users: Be32::new(1),
        ..Default::default()
    };
    let mut bytes = [0u8; 1024];
    bytes.copy_from_slice(raw.as_bytes());
    bytes
}

/// Validated journal geometry and logical-to-physical block map.
#[derive(Debug)]
pub(super) struct JournalGeometry {
    block_map: Vec<Ext4Bid>,
    superblock: JournalSuperblock,
}

/// Mounted synchronous runtime journal and its single-transaction state.
pub(super) struct Journal {
    geometry: JournalGeometry,
    block_device: Arc<dyn BlockDevice>,
    state: Mutex<JournalState>,
    commit_lock: Mutex<()>,
    aborted: AtomicBool,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    stats: JournalStats,
}

#[cfg(any(feature = "ext4-research-stats", ktest))]
#[derive(Default)]
struct JournalStats {
    captured_blocks: AtomicU64,
    committed_transactions: AtomicU64,
    committed_blocks: AtomicU64,
    checkpoint_blocks: AtomicU64,
    barriers: AtomicU64,
}

#[cfg(any(feature = "ext4-research-stats", ktest))]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct JournalStatsSnapshot {
    pub(super) captured_blocks: u64,
    pub(super) committed_transactions: u64,
    pub(super) committed_blocks: u64,
    pub(super) checkpoint_blocks: u64,
    pub(super) barriers: u64,
    pub(super) aborted: bool,
}

pub(super) struct CommitReceipt {
    #[cfg_attr(not(ktest), allow(dead_code))]
    pub(super) tid: Tid,
    pub(super) images: BTreeMap<Ext4Bid, Vec<u8>>,
}

struct JournalState {
    next_tid: Tid,
    head: u32,
    running: BTreeMap<Ext4Bid, Vec<u8>>,
    committed: Option<CommittedTransaction>,
}

struct CommittedTransaction {
    tid: Tid,
    images: BTreeMap<Ext4Bid, Vec<u8>>,
}

impl Journal {
    pub(super) fn load(fs: &Arc<Ext4>) -> Result<Option<Arc<Self>>> {
        Ok(load_geometry(fs)?.map(|geometry| {
            let next_tid = geometry.sequence();
            let head = geometry.first();
            Arc::new(Self {
                geometry,
                block_device: fs.block_device().clone(),
                state: Mutex::new(JournalState {
                    next_tid,
                    head,
                    running: BTreeMap::new(),
                    committed: None,
                }),
                commit_lock: Mutex::new(()),
                aborted: AtomicBool::new(false),
                #[cfg(any(feature = "ext4-research-stats", ktest))]
                stats: JournalStats::default(),
            })
        }))
    }

    /// Records the complete latest after-image of one metadata home block.
    pub(super) fn capture(&self, bid: Ext4Bid, image: Vec<u8>) -> Result<()> {
        self.ensure_healthy()?;
        if image.len() != BLOCK_SIZE {
            return_errno_with_message!(Errno::EINVAL, "journal metadata image is not one block");
        }
        if bid > u32::MAX as Ext4Bid {
            return_errno_with_message!(
                Errno::EOVERFLOW,
                "current journal format supports only 32-bit block tags"
            );
        }
        let replaced = self.state.lock().running.insert(bid, image).is_some();
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        if !replaced {
            self.stats.captured_blocks.fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(not(any(feature = "ext4-research-stats", ktest)))]
        let _ = replaced;
        Ok(())
    }

    /// Returns the newest transaction image so a cache miss never reloads a
    /// stale home block before checkpoint.
    pub(super) fn latest_image(&self, bid: Ext4Bid) -> Option<Vec<u8>> {
        let state = self.state.lock();
        state.running.get(&bid).cloned().or_else(|| {
            state
                .committed
                .as_ref()
                .and_then(|transaction| transaction.images.get(&bid).cloned())
        })
    }

    /// Prevents an old metadata image from being checkpointed over storage
    /// that is about to be freed and potentially reused at runtime.
    pub(super) fn prepare_block_reuse(&self, range: Range<Ext4Bid>) -> Result<()> {
        self.checkpoint_all()?;
        let mut state = self.state.lock();
        for bid in range {
            state.running.remove(&bid);
        }
        Ok(())
    }

    /// Commits all metadata currently captured by the running transaction.
    /// New mutations arriving while I/O is in progress join the next one.
    pub(super) fn commit(&self) -> Result<Option<CommitReceipt>> {
        self.ensure_healthy()?;
        let _commit = self.commit_lock.lock();
        if let Err(error) = self.checkpoint_locked() {
            self.abort();
            return Err(error);
        }

        let (tid, start, images) = {
            let mut state = self.state.lock();
            if state.running.is_empty() {
                return Ok(None);
            }
            let tid = state.next_tid;
            state.next_tid = state.next_tid.wrapping_add(1);
            let start = state.head;
            let images = core::mem::take(&mut state.running);
            (tid, start, images)
        };

        if let Err(error) = self.write_transaction(tid, start, &images) {
            let mut state = self.state.lock();
            for (bid, image) in images {
                state.running.entry(bid).or_insert(image);
            }
            self.abort();
            return Err(error);
        }

        let written = u32::try_from(images.len() + 2)
            .map_err(|_| Error::with_message(Errno::EOVERFLOW, "journal transaction too large"))?;
        let mut state = self.state.lock();
        state.head = self.advance(start, written);
        let receipt_images = images.clone();
        let block_count = images.len() as u64;
        state.committed = Some(CommittedTransaction { tid, images });
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            self.stats
                .committed_transactions
                .fetch_add(1, Ordering::Relaxed);
            self.stats
                .committed_blocks
                .fetch_add(block_count, Ordering::Relaxed);
        }
        #[cfg(not(any(feature = "ext4-research-stats", ktest)))]
        let _ = block_count;
        Ok(Some(CommitReceipt {
            tid,
            images: receipt_images,
        }))
    }

    /// Writes every committed after-image to its home block and marks the log
    /// clean. Used for log reuse and clean filesystem drain.
    pub(super) fn checkpoint_all(&self) -> Result<()> {
        self.ensure_healthy()?;
        let _commit = self.commit_lock.lock();
        let result = self.checkpoint_locked();
        if result.is_err() {
            self.abort();
        }
        result
    }

    fn checkpoint_locked(&self) -> Result<()> {
        let transaction = self.state.lock().committed.take();
        let Some(transaction) = transaction else {
            return Ok(());
        };
        let checkpoint_result = (|| {
            for (bid, image) in &transaction.images {
                self.block_device
                    .write_bytes(Bid::new(*bid).to_offset(), image)
                    .map_err(|_| {
                        Error::with_message(Errno::EIO, "journal checkpoint write failed")
                    })?;
            }
            self.barrier()?;
            self.write_journal_superblock(0, transaction.tid.wrapping_add(1), true)
        })();
        if let Err(error) = checkpoint_result {
            self.state.lock().committed = Some(transaction);
            return Err(error);
        }
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            self.stats
                .checkpoint_blocks
                .fetch_add(transaction.images.len() as u64, Ordering::Relaxed);
        }
        Ok(())
    }

    fn write_transaction(
        &self,
        tid: Tid,
        start: u32,
        images: &BTreeMap<Ext4Bid, Vec<u8>>,
    ) -> Result<()> {
        let capacity = (BLOCK_SIZE - size_of::<RawJournalHeader>() - 16) / size_of::<RawBlockTag>();
        if images.len() > capacity || images.len() + 2 > self.usable_blocks() as usize {
            return_errno_with_message!(Errno::ENOSPC, "journal transaction exceeds log capacity");
        }

        let (descriptor, escaped) = self.build_descriptor(tid, images)?;
        let mut log = start;
        self.write_log_block(log, &descriptor)?;
        for ((_, image), needs_escape) in images.iter().zip(escaped) {
            log = self.next_log_block(log);
            let mut payload = [0u8; BLOCK_SIZE];
            payload.copy_from_slice(image);
            if needs_escape {
                payload[..4].fill(0);
            }
            self.write_log_block(log, &payload)?;
        }
        self.barrier()?;

        let commit_log = self.next_log_block(log);
        let commit = self.build_commit(tid);
        self.write_log_block(commit_log, &commit)?;
        self.barrier()?;
        self.write_journal_superblock(start, tid, false)
    }

    fn build_descriptor(
        &self,
        tid: Tid,
        images: &BTreeMap<Ext4Bid, Vec<u8>>,
    ) -> Result<([u8; BLOCK_SIZE], Vec<bool>)> {
        let mut block = [0u8; BLOCK_SIZE];
        let header = RawJournalHeader {
            h_magic: Be32::new(JBD2_MAGIC),
            h_blocktype: Be32::new(BLOCKTYPE_DESCRIPTOR),
            h_sequence: Be32::new(tid),
        };
        block[..size_of::<RawJournalHeader>()].copy_from_slice(header.as_bytes());
        let mut offset = size_of::<RawJournalHeader>();
        let mut escaped = Vec::with_capacity(images.len());
        for (index, (bid, image)) in images.iter().enumerate() {
            let first = index == 0;
            let last = index + 1 == images.len();
            let needs_escape = image[..4] == JBD2_MAGIC.to_be_bytes();
            let mut flags = if first { 0 } else { TAG_FLAG_SAME_UUID };
            if last {
                flags |= TAG_FLAG_LAST_TAG;
            }
            if needs_escape {
                flags |= TAG_FLAG_ESCAPE;
            }
            let tag = RawBlockTag {
                t_blocknr: Be32::new(*bid as u32),
                t_checksum: Be16::new(0),
                t_flags: Be16::new(flags),
            };
            let end = offset + size_of::<RawBlockTag>();
            block[offset..end].copy_from_slice(tag.as_bytes());
            offset = end + if first { 16 } else { 0 };
            escaped.push(needs_escape);
        }
        Ok((block, escaped))
    }

    fn build_commit(&self, tid: Tid) -> [u8; BLOCK_SIZE] {
        let now = super::utils::now();
        let commit = RawCommitBlock {
            header: RawJournalHeader {
                h_magic: Be32::new(JBD2_MAGIC),
                h_blocktype: Be32::new(BLOCKTYPE_COMMIT),
                h_sequence: Be32::new(tid),
            },
            h_chksum_type: 0,
            h_chksum_size: 0,
            h_padding: [0; 2],
            h_chksum: [Be32::new(0); 8],
            h_commit_sec: Be64::new(now.as_secs()),
            h_commit_nsec: Be32::new(now.subsec_nanos()),
        };
        let mut block = [0u8; BLOCK_SIZE];
        block[..size_of::<RawCommitBlock>()].copy_from_slice(commit.as_bytes());
        block
    }

    fn write_log_block(&self, log: u32, block: &[u8; BLOCK_SIZE]) -> Result<()> {
        let pblock = self
            .geometry
            .log_block_to_physical(log)
            .ok_or_else(|| Error::with_message(Errno::EUCLEAN, "journal log block is unmapped"))?;
        self.block_device
            .write_bytes(Bid::new(pblock).to_offset(), block)
            .map_err(|_| Error::with_message(Errno::EIO, "journal log write failed"))
    }

    fn write_journal_superblock(&self, start: u32, sequence: Tid, clean: bool) -> Result<()> {
        let pblock = self
            .geometry
            .log_block_to_physical(0)
            .ok_or_else(|| Error::with_message(Errno::EUCLEAN, "journal superblock is unmapped"))?;
        let offset = Bid::new(pblock).to_offset();
        let mut raw: RawJournalSuperblock = self
            .block_device
            .read_val(offset)
            .map_err(|_| Error::with_message(Errno::EIO, "journal superblock read failed"))?;
        raw.s_start = Be32::new(start);
        raw.s_sequence = Be32::new(sequence);
        if clean {
            raw.s_head = Be32::new(self.state.lock().head);
        }
        self.block_device
            .write_val(offset, &raw)
            .map_err(|_| Error::with_message(Errno::EIO, "journal superblock write failed"))?;
        self.barrier()
    }

    fn barrier(&self) -> Result<()> {
        match self
            .block_device
            .sync()
            .map_err(|_| Error::with_message(Errno::EIO, "journal barrier failed"))?
        {
            BioStatus::Complete => {
                #[cfg(any(feature = "ext4-research-stats", ktest))]
                {
                    self.stats.barriers.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            }
            _ => return_errno_with_message!(Errno::EIO, "journal barrier did not complete"),
        }
    }

    fn usable_blocks(&self) -> u32 {
        self.geometry.maxlen() - self.geometry.first()
    }

    fn next_log_block(&self, current: u32) -> u32 {
        let next = current + 1;
        if next >= self.geometry.maxlen() {
            self.geometry.first()
        } else {
            next
        }
    }

    fn advance(&self, mut current: u32, count: u32) -> u32 {
        for _ in 0..count {
            current = self.next_log_block(current);
        }
        current
    }

    fn ensure_healthy(&self) -> Result<()> {
        if self.aborted.load(Ordering::Acquire) {
            return_errno_with_message!(Errno::EIO, "JBD2 journal is aborted");
        }
        Ok(())
    }

    fn abort(&self) {
        self.aborted.store(true, Ordering::Release);
    }

    #[cfg(feature = "ext4-research-stats")]
    pub(super) fn stats_snapshot(&self) -> JournalStatsSnapshot {
        JournalStatsSnapshot {
            captured_blocks: self.stats.captured_blocks.load(Ordering::Relaxed),
            committed_transactions: self.stats.committed_transactions.load(Ordering::Relaxed),
            committed_blocks: self.stats.committed_blocks.load(Ordering::Relaxed),
            checkpoint_blocks: self.stats.checkpoint_blocks.load(Ordering::Relaxed),
            barriers: self.stats.barriers.load(Ordering::Relaxed),
            aborted: self.aborted.load(Ordering::Acquire),
        }
    }
}

impl JournalGeometry {
    pub(super) fn maxlen(&self) -> u32 {
        self.superblock.maxlen()
    }

    pub(super) fn first(&self) -> u32 {
        self.superblock.first()
    }

    pub(super) fn sequence(&self) -> Tid {
        self.superblock.sequence()
    }

    pub(super) fn start(&self) -> u32 {
        self.superblock.start()
    }

    pub(super) fn blocksize(&self) -> u32 {
        self.superblock.blocksize()
    }

    pub(super) fn log_block_to_physical(&self, log: u32) -> Option<Ext4Bid> {
        self.block_map.get(log as usize).copied()
    }
}

/// Loads inode 8 and validates the clean JBD2 journal superblock.
pub(super) fn load_geometry(fs: &Arc<Ext4>) -> Result<Option<JournalGeometry>> {
    if !fs
        .super_block()
        .feature_compat()
        .contains(FeatureCompatSet::HAS_JOURNAL)
    {
        return Ok(None);
    }

    let inode = fs.read_inode(JOURNAL_INO)?;
    let nblocks = inode.file_size().div_ceil(BLOCK_SIZE) as u32;
    if nblocks < 2 {
        return_errno_with_message!(Errno::EUCLEAN, "journal inode is too small");
    }
    let block_map = inode.map_allocated_blocks(nblocks)?;
    let raw: RawJournalSuperblock = fs
        .block_device()
        .read_val(block_map[0] as usize * BLOCK_SIZE)
        .map_err(|_| Error::with_message(Errno::EIO, "failed to read journal superblock"))?;
    let superblock = JournalSuperblock::try_from(raw)?;
    if superblock.maxlen() as usize > block_map.len() {
        return_errno_with_message!(Errno::EUCLEAN, "journal length exceeds inode mapping");
    }

    Ok(Some(JournalGeometry {
        block_map,
        superblock,
    }))
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{super::test_utils::Ext4FixtureBuilder, *};

    const JOURNAL_START_BLOCK: u32 = 200;

    fn journal_fixture(start: u32) -> super::super::test_utils::Ext4Fixture {
        Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_journal(JOURNAL_START_BLOCK, 8, start)
            .build()
            .unwrap()
    }

    #[ktest]
    fn load_clean_geometry_end_to_end() {
        let fixture = journal_fixture(0);
        let geometry = load_geometry(&fixture.ext4).unwrap().unwrap();
        assert_eq!(geometry.maxlen(), 8);
        assert_eq!(geometry.first(), 1);
        assert_eq!(geometry.sequence(), 1);
        assert_eq!(geometry.start(), 0);
        assert_eq!(geometry.blocksize(), BLOCK_SIZE as u32);
        assert_eq!(geometry.log_block_to_physical(0), Some(200));
        assert_eq!(geometry.log_block_to_physical(1), Some(201));
    }

    #[ktest]
    fn commit_then_checkpoint_writes_real_jbd2_layout() {
        let fixture = journal_fixture(0);
        let journal = fixture.ext4.journal().unwrap();
        let mut image = vec![0u8; BLOCK_SIZE];
        image[..8].copy_from_slice(b"JOURNAL!");
        journal.capture(300, image.clone()).unwrap();

        let receipt = journal.commit().unwrap().unwrap();
        assert_eq!(receipt.tid, 1);
        let descriptor: RawJournalHeader = fixture
            .disk
            .segment()
            .read_val((JOURNAL_START_BLOCK as usize + 1) * BLOCK_SIZE)
            .unwrap();
        assert_eq!(descriptor.h_magic.get(), JBD2_MAGIC);
        assert_eq!(descriptor.h_blocktype.get(), BLOCKTYPE_DESCRIPTOR);
        assert_eq!(descriptor.h_sequence.get(), 1);
        assert_eq!(
            fixture
                .disk
                .segment()
                .read_val::<RawJournalHeader>((JOURNAL_START_BLOCK as usize + 3) * BLOCK_SIZE)
                .unwrap()
                .h_blocktype
                .get(),
            BLOCKTYPE_COMMIT
        );
        assert_eq!(journal.latest_image(300).unwrap(), image);

        journal.checkpoint_all().unwrap();
        let mut home = vec![0u8; BLOCK_SIZE];
        fixture
            .disk
            .segment()
            .read_bytes(300 * BLOCK_SIZE, &mut home)
            .unwrap();
        assert_eq!(home, image);
        let raw: RawJournalSuperblock = fixture
            .disk
            .segment()
            .read_val(JOURNAL_START_BLOCK as usize * BLOCK_SIZE)
            .unwrap();
        assert_eq!(raw.s_start.get(), 0);
    }

    #[ktest]
    fn reject_dirty_journal() {
        let fixture = journal_fixture(1);
        let err = load_geometry(&fixture.ext4).unwrap_err();
        assert_eq!(err.error(), Errno::EUCLEAN);
    }
}
