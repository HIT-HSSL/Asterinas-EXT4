// SPDX-License-Identifier: MPL-2.0

//! Cached home blocks for nojournal metadata writeback.

use super::{
    journal::{CommitReceipt, Journal},
    nv::Ext4NvState,
    prelude::*,
};

/// A stable handle to one cached metadata home block.
#[derive(Clone)]
pub(super) struct MetadataBlockHandle {
    block: Arc<MetadataBlock>,
}

/// Filesystem-wide identity map for cached metadata home blocks.
pub(super) struct MetadataBlockCache {
    block_device: Arc<dyn BlockDevice>,
    nv_state: Option<Arc<Ext4NvState>>,
    block_size: usize,
    clean_capacity: usize,
    blocks: Mutex<BTreeMap<Ext4Bid, Arc<MetadataBlock>>>,
    journal: Mutex<Option<Weak<Journal>>>,
}

struct MetadataBlock {
    bid: Ext4Bid,
    state: Mutex<MetadataBlockState>,
    /// Serializes submissions for this block without preventing new mutations.
    writeback: Mutex<()>,
}

struct MetadataBlockState {
    data: Vec<u8>,
    dirty: bool,
    writeback: bool,
    writeback_failed: bool,
}

impl MetadataBlockState {
    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn begin_writeback(&mut self) -> Option<Vec<u8>> {
        debug_assert!(!self.writeback);
        if !self.dirty {
            return None;
        }
        let snapshot = self.data.clone();
        self.dirty = false;
        self.writeback = true;
        Some(snapshot)
    }

    fn finish_writeback(&mut self, succeeded: bool) {
        debug_assert!(self.writeback);
        self.writeback = false;
        if succeeded {
            self.writeback_failed = false;
        } else {
            self.dirty = true;
            self.writeback_failed = true;
        }
    }

    fn is_clean(&self) -> bool {
        !self.dirty && !self.writeback
    }

    fn finish_journal_commit(&mut self, committed: &[u8]) {
        if !self.writeback && self.data == committed {
            self.dirty = false;
            self.writeback_failed = false;
        }
    }

    fn handle(&self, block: Arc<MetadataBlock>) -> Option<MetadataBlockHandle> {
        (self.dirty || self.writeback).then_some(MetadataBlockHandle { block })
    }
}

impl MetadataBlockCache {
    pub(super) fn new(
        block_device: Arc<dyn BlockDevice>,
        nv_state: Option<Arc<Ext4NvState>>,
        block_size: usize,
        clean_capacity: usize,
    ) -> Self {
        Self {
            block_device,
            nv_state,
            block_size,
            clean_capacity,
            blocks: Mutex::new(BTreeMap::new()),
            journal: Mutex::new(None),
        }
    }

    pub(super) fn attach_journal(&self, journal: &Arc<Journal>) {
        *self.journal.lock() = Some(Arc::downgrade(journal));
    }

    fn journal(&self) -> Option<Arc<Journal>> {
        self.journal.lock().as_ref().and_then(Weak::upgrade)
    }

    fn get_or_load(&self, bid: Ext4Bid) -> Result<Arc<MetadataBlock>> {
        if let Some(block) = self.blocks.lock().get(&bid).cloned() {
            return Ok(block);
        }

        let offset = bid
            .checked_mul(self.block_size as Ext4Bid)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or_else(|| {
                Error::with_message(Errno::EOVERFLOW, "metadata block offset overflow")
            })?;
        let mut data = vec![0; self.block_size];
        if let Some(nv_state) = &self.nv_state {
            nv_state.read_meta_block(bid)?.read(&mut data)?;
        } else {
            self.block_device
                .read_bytes(offset, &mut data)
                .map_err(|_| Error::with_message(Errno::EIO, "failed to read metadata block"))?;
        }
        if let Some(journal) = self.journal()
            && let Some(snapshot) = journal.latest_image(bid)
        {
            data.copy_from_slice(&snapshot);
        }

        let loaded = Arc::new(MetadataBlock {
            bid,
            state: Mutex::new(MetadataBlockState {
                data,
                dirty: false,
                writeback: false,
                writeback_failed: false,
            }),
            writeback: Mutex::new(()),
        });
        let mut blocks = self.blocks.lock();
        if let Some(block) = blocks.get(&bid) {
            return Ok(block.clone());
        }
        while blocks.len() >= self.clean_capacity {
            let victim = blocks
                .iter()
                .find(|(_, block)| Arc::strong_count(block) == 1 && block.state.lock().is_clean())
                .map(|(&bid, _)| bid);
            if let Some(victim) = victim {
                blocks.remove(&victim);
            } else {
                break;
            }
        }
        blocks.insert(bid, loaded.clone());
        Ok(loaded)
    }

    pub(super) fn read<R>(&self, bid: Ext4Bid, read: impl FnOnce(&[u8]) -> Result<R>) -> Result<R> {
        let block = self.get_or_load(bid)?;
        let state = block.state.lock();
        read(&state.data)
    }

    pub(super) fn modify(
        &self,
        bid: Ext4Bid,
        modify: impl FnOnce(&mut [u8]) -> Result<()>,
    ) -> Result<MetadataBlockHandle> {
        let block = self.get_or_load(bid)?;
        let snapshot = {
            let mut state = block.state.lock();
            let old_data = state.data.clone();
            if let Err(error) = modify(&mut state.data) {
                state.data = old_data;
                return Err(error);
            }
            state.mark_dirty();
            state.data.clone()
        };
        if let Some(journal) = self.journal() {
            journal.capture(bid, snapshot)?;
        }
        Ok(MetadataBlockHandle { block })
    }

    /// Persists the latest dirty snapshot of `handle` when this call obtains
    /// the block's writeback ownership.
    pub(super) fn sync(&self, handle: &MetadataBlockHandle) -> Result<()> {
        // Journal mode publishes metadata at the enclosing operation's commit
        // boundary. Per-block sync calls only establish ordering dependencies;
        // committing here would fragment one fsync into many tiny transactions.
        if self.journal().is_some() {
            return Ok(());
        }
        let block = &handle.block;
        let _writeback = block.writeback.lock();
        let offset = block
            .bid
            .checked_mul(self.block_size as Ext4Bid)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or_else(|| {
                Error::with_message(Errno::EOVERFLOW, "metadata block offset overflow")
            })?;
        let Some(snapshot) = block.state.lock().begin_writeback() else {
            return Ok(());
        };
        let result = if let Some(nv_state) = &self.nv_state {
            nv_state.update_meta_block(block.bid, |bytes| {
                bytes.copy_from_slice(&snapshot);
                Ok(())
            })
        } else {
            let mut io_batch = IoBatch::with_capacity(1);
            let submit_result = self
                .block_device
                .write_bytes_async(offset, &snapshot, &mut io_batch)
                .map_err(Error::from);
            match submit_result {
                Ok(()) => io_batch.wait_all().map_err(Error::from),
                Err(error) => Err(error),
            }
        };

        block.state.lock().finish_writeback(result.is_ok());
        result
    }

    /// Persists every block that was dirty when this operation started.
    pub(super) fn sync_all(&self) -> Result<()> {
        let handles = {
            let blocks = self.blocks.lock();
            blocks
                .values()
                .filter_map(|block| block.state.lock().handle(block.clone()))
                .collect::<Vec<_>>()
        };
        for handle in handles {
            self.sync(&handle)?;
        }
        Ok(())
    }

    /// Cleans blocks whose current bytes still match the transaction that was
    /// just committed. A concurrently redirtied block remains dirty.
    pub(super) fn finish_journal_commit(&self, receipt: &CommitReceipt) {
        let blocks = self.blocks.lock();
        for (bid, committed) in &receipt.images {
            let Some(block) = blocks.get(bid) else {
                continue;
            };
            let mut state = block.state.lock();
            state.finish_journal_commit(committed);
        }
    }

    /// Invalidates a clean cached block. Dirty state is never discarded.
    #[cfg(ktest)]
    pub(super) fn invalidate(&self, bid: Ext4Bid) {
        let mut blocks = self.blocks.lock();
        let can_remove = blocks
            .get(&bid)
            .map(|block| {
                let state = block.state.lock();
                state.is_clean()
            })
            .unwrap_or(false);
        if can_remove {
            blocks.remove(&bid);
        }
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn writeback_completion_does_not_clean_redirtied_block() {
        let mut state = MetadataBlockState {
            data: vec![0; 16],
            dirty: false,
            writeback: false,
            writeback_failed: false,
        };

        state.mark_dirty();
        let _snapshot = state.begin_writeback().unwrap();
        assert!(!state.dirty);
        assert!(state.writeback);

        state.mark_dirty();
        state.finish_writeback(true);

        assert!(!state.is_clean());
        assert!(state.dirty);
        assert!(!state.writeback);
        assert!(!state.writeback_failed);
    }

    #[ktest]
    fn failed_writeback_remains_dirty_and_can_retry() {
        let mut state = MetadataBlockState {
            data: vec![0; 16],
            dirty: false,
            writeback: false,
            writeback_failed: false,
        };

        state.mark_dirty();
        let _first_attempt = state.begin_writeback().unwrap();
        state.finish_writeback(false);
        assert!(!state.is_clean());
        assert!(state.dirty);
        assert!(!state.writeback);
        assert!(state.writeback_failed);

        let _retry = state.begin_writeback().unwrap();
        state.finish_writeback(true);
        assert!(state.is_clean());
        assert!(!state.writeback_failed);
    }

    #[ktest]
    fn clean_block_has_no_writeback_snapshot() {
        let mut state = MetadataBlockState {
            data: vec![0; 16],
            dirty: false,
            writeback: false,
            writeback_failed: false,
        };

        assert!(state.begin_writeback().is_none());
        assert!(state.is_clean());
    }

    #[ktest]
    fn journal_commit_only_cleans_matching_after_image() {
        let mut state = MetadataBlockState {
            data: vec![1; 16],
            dirty: true,
            writeback: false,
            writeback_failed: false,
        };

        state.finish_journal_commit(&vec![0; 16]);
        assert!(state.dirty);

        state.finish_journal_commit(&vec![1; 16]);
        assert!(state.is_clean());
    }
}
