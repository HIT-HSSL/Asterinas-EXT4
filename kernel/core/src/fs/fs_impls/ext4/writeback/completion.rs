// SPDX-License-Identifier: MPL-2.0

//! Mount-local serial completion processing for EXT4 writeback.

use core::{
    fmt::{Formatter, Result as FmtResult},
    sync::atomic::{AtomicBool, Ordering},
};

use ostd::sync::WaitQueue;

use super::super::prelude::*;
use crate::{
    thread::{AsThread, Thread, kernel_thread::ThreadOptions},
    vm::page_cache::{
        PreparedWritebackCompletion, WritebackCompletion, WritebackCompletionDispatcher,
        WritebackSequence,
    },
};

pub(super) enum CompletionJob {
    PageCache(WritebackCompletion),
    #[cfg(ktest)]
    Test(Box<dyn FnOnce(BioStatus) -> Result<()> + Send>),
    #[cfg(ktest)]
    MergeableTest {
        group: u64,
        merged_jobs: usize,
        observed: Arc<Mutex<Vec<usize>>>,
    },
}

impl CompletionJob {
    fn finish(self, status: BioStatus) -> Result<()> {
        match self {
            Self::PageCache(completion) => completion.finish(status),
            #[cfg(ktest)]
            Self::Test(finish) => finish(status),
            #[cfg(ktest)]
            Self::MergeableTest {
                merged_jobs,
                observed,
                ..
            } => {
                observed.lock().push(merged_jobs);
                if status == BioStatus::Complete {
                    Ok(())
                } else {
                    Err(Error::with_message(Errno::EIO, "test completion failure"))
                }
            }
        }
    }

    fn try_merge(&mut self, next: CompletionJob) -> core::result::Result<(), CompletionJob> {
        match self {
            #[cfg(not(ktest))]
            Self::PageCache(current) => {
                let Self::PageCache(next) = next;
                current.try_merge(next).map_err(Self::PageCache)
            }
            #[cfg(ktest)]
            Self::PageCache(current) => match next {
                Self::PageCache(next) => current.try_merge(next).map_err(Self::PageCache),
                next => Err(next),
            },
            #[cfg(ktest)]
            Self::MergeableTest {
                group,
                merged_jobs,
                observed,
            } => match next {
                Self::MergeableTest {
                    group: next_group,
                    merged_jobs: next_jobs,
                    observed: next_observed,
                } if *group == next_group && Arc::ptr_eq(observed, &next_observed) => {
                    *merged_jobs += next_jobs;
                    Ok(())
                }
                next => Err(next),
            },
            #[cfg(ktest)]
            Self::Test(_) => Err(next),
        }
    }
}

#[derive(Clone, Copy)]
struct CompletionRecord {
    ino: Ext4Ino,
    sequence: WritebackSequence,
    raw_sequence: u64,
    nr_pages: usize,
    suppress_error: bool,
}

struct CompletionItem {
    ino: Ext4Ino,
    sequence: WritebackSequence,
    raw_sequence: u64,
    nr_pages: usize,
    status: BioStatus,
    suppress_error: bool,
    job: CompletionJob,
}

#[derive(Default)]
struct InodeCompletionProgress {
    next_sequence: u64,
    completed_through: u64,
    completed_out_of_order: BTreeSet<u64>,
    errors: BTreeMap<u64, Error>,
}

struct CompletionState {
    slots: Vec<Option<CompletionItem>>,
    pending: VecDeque<CompletionItem>,
    inode_progress: BTreeMap<Ext4Ino, InodeCompletionProgress>,
    inflight_bios: usize,
    inflight_pages: usize,
    completion_progress: u64,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    stats: CompletionStats,
    accepting: bool,
    stopping: bool,
}

impl CompletionState {
    fn new(max_inflight_bios: usize) -> Self {
        let mut slots = Vec::with_capacity(max_inflight_bios);
        slots.resize_with(max_inflight_bios, || None);
        Self {
            slots,
            pending: VecDeque::with_capacity(max_inflight_bios),
            inode_progress: BTreeMap::new(),
            inflight_bios: 0,
            inflight_pages: 0,
            completion_progress: 0,
            #[cfg(any(feature = "ext4-research-stats", ktest))]
            stats: CompletionStats::default(),
            accepting: true,
            stopping: false,
        }
    }
}

#[cfg(any(feature = "ext4-research-stats", ktest))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::fs::fs_impls::ext4) struct CompletionStatsSnapshot {
    pub(in crate::fs::fs_impls::ext4) submitted_bios: usize,
    pub(in crate::fs::fs_impls::ext4) submitted_pages: usize,
    pub(in crate::fs::fs_impls::ext4) completed_bios: usize,
    pub(in crate::fs::fs_impls::ext4) completed_pages: usize,
    pub(in crate::fs::fs_impls::ext4) inflight_bio_samples: usize,
    pub(in crate::fs::fs_impls::ext4) inflight_bio_sum: usize,
    pub(in crate::fs::fs_impls::ext4) inflight_page_sum: usize,
    pub(in crate::fs::fs_impls::ext4) peak_inflight_bios: usize,
    pub(in crate::fs::fs_impls::ext4) peak_inflight_pages: usize,
    pub(in crate::fs::fs_impls::ext4) completion_queue_hwm: usize,
    pub(in crate::fs::fs_impls::ext4) credit_stalls: usize,
    pub(in crate::fs::fs_impls::ext4) post_io_jobs: usize,
    pub(in crate::fs::fs_impls::ext4) coalesced_jobs: usize,
    pub(in crate::fs::fs_impls::ext4) device_failures: usize,
    pub(in crate::fs::fs_impls::ext4) post_io_failures: usize,
}

#[cfg(any(feature = "ext4-research-stats", ktest))]
#[derive(Default)]
struct CompletionStats {
    snapshot: CompletionStatsSnapshot,
}

#[cfg(any(feature = "ext4-research-stats", ktest))]
impl CompletionStats {
    fn snapshot_and_reset(
        &mut self,
        inflight_bios: usize,
        inflight_pages: usize,
    ) -> CompletionStatsSnapshot {
        let snapshot = self.snapshot;
        self.snapshot = CompletionStatsSnapshot {
            peak_inflight_bios: inflight_bios,
            peak_inflight_pages: inflight_pages,
            ..CompletionStatsSnapshot::default()
        };
        snapshot
    }
}

impl CompletionItem {
    fn record(&self) -> CompletionRecord {
        CompletionRecord {
            ino: self.ino,
            sequence: self.sequence,
            raw_sequence: self.raw_sequence,
            nr_pages: self.nr_pages,
            suppress_error: self.suppress_error,
        }
    }
}

/// Executes all post-I/O work serially for one EXT4 mount.
pub(super) struct CompletionEngine {
    state: SpinLock<CompletionState>,
    work_wait_queue: WaitQueue,
    progress_wait_queue: WaitQueue,
    started: AtomicBool,
    worker: Mutex<Option<Arc<Thread>>>,
    max_inflight_bios: usize,
    max_inflight_pages: usize,
    error_notifier: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Debug for CompletionEngine {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        let state = self.state.disable_irq().lock();
        f.debug_struct("CompletionEngine")
            .field("inflight_bios", &state.inflight_bios)
            .field("inflight_pages", &state.inflight_pages)
            .field("pending", &state.pending.len())
            .field("accepting", &state.accepting)
            .field("stopping", &state.stopping)
            .finish()
    }
}

impl CompletionEngine {
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn new(max_inflight_bios: usize, max_inflight_pages: usize) -> Arc<Self> {
        Self::new_inner(max_inflight_bios, max_inflight_pages, None)
    }

    pub(super) fn new_with_error_notifier(
        max_inflight_bios: usize,
        max_inflight_pages: usize,
        error_notifier: Arc<dyn Fn() + Send + Sync>,
    ) -> Arc<Self> {
        Self::new_inner(max_inflight_bios, max_inflight_pages, Some(error_notifier))
    }

    fn new_inner(
        max_inflight_bios: usize,
        max_inflight_pages: usize,
        error_notifier: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Arc<Self> {
        assert!(max_inflight_bios > 0);
        assert!(max_inflight_pages > 0);
        Arc::new(Self {
            state: SpinLock::new(CompletionState::new(max_inflight_bios)),
            work_wait_queue: WaitQueue::new(),
            progress_wait_queue: WaitQueue::new(),
            started: AtomicBool::new(false),
            worker: Mutex::new(None),
            max_inflight_bios,
            max_inflight_pages,
            error_notifier,
        })
    }

    pub(super) fn start(self: &Arc<Self>) -> Result<()> {
        if self
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return_errno_with_message!(Errno::EBUSY, "EXT4 completion worker already started");
        }

        let engine = self.clone();
        let task = ThreadOptions::new(move || engine.worker_loop()).build();
        let thread = task.as_thread().unwrap().clone();
        self.worker.lock().replace(thread.clone());
        thread.run();
        Ok(())
    }

    fn worker_loop(&self) {
        loop {
            self.work_wait_queue.wait_until(|| {
                let state = self.state.disable_irq().lock();
                (state.stopping || !state.pending.is_empty()).then_some(())
            });

            let item = {
                let mut state = self.state.disable_irq().lock();
                match state.pending.pop_front() {
                    Some(item) => Some(item),
                    None if state.stopping => None,
                    None => continue,
                }
            };
            let Some(mut item) = item else {
                break;
            };

            let mut records = Vec::new();
            records.push(item.record());
            while item.status == BioStatus::Complete && !item.suppress_error {
                let next = {
                    let mut state = self.state.disable_irq().lock();
                    let Some(next) = state.pending.front() else {
                        break;
                    };
                    let previous = records.last().unwrap();
                    if next.status != BioStatus::Complete
                        || next.suppress_error
                        || next.ino != previous.ino
                        || previous.raw_sequence.checked_add(1) != Some(next.raw_sequence)
                    {
                        break;
                    }
                    state.pending.pop_front().unwrap()
                };

                let record = next.record();
                let CompletionItem {
                    ino,
                    sequence,
                    raw_sequence,
                    nr_pages,
                    status,
                    suppress_error,
                    job,
                } = next;
                if let Err(job) = item.job.try_merge(job) {
                    self.state
                        .disable_irq()
                        .lock()
                        .pending
                        .push_front(CompletionItem {
                            ino,
                            sequence,
                            raw_sequence,
                            nr_pages,
                            status,
                            suppress_error,
                            job,
                        });
                    break;
                }
                records.push(record);
            }

            let result = item.job.finish(item.status);
            let completion_error = result.err();
            #[cfg(any(feature = "ext4-research-stats", ktest))]
            {
                let mut state = self.state.disable_irq().lock();
                state.stats.snapshot.post_io_jobs =
                    state.stats.snapshot.post_io_jobs.saturating_add(1);
                state.stats.snapshot.coalesced_jobs = state
                    .stats
                    .snapshot
                    .coalesced_jobs
                    .saturating_add(records.len().saturating_sub(1));
                let reportable_records = records
                    .iter()
                    .filter(|record| !record.suppress_error)
                    .count();
                if item.status != BioStatus::Complete {
                    state.stats.snapshot.device_failures = state
                        .stats
                        .snapshot
                        .device_failures
                        .saturating_add(reportable_records);
                } else if completion_error.is_some() {
                    state.stats.snapshot.post_io_failures = state
                        .stats
                        .snapshot
                        .post_io_failures
                        .saturating_add(reportable_records);
                }
            }
            let notify_error =
                completion_error.is_some() && records.iter().any(|record| !record.suppress_error);
            for record in records {
                let error = if record.suppress_error {
                    None
                } else {
                    completion_error
                };
                self.finish_item(
                    record.ino,
                    record.sequence,
                    record.raw_sequence,
                    record.nr_pages,
                    error,
                );
            }
            if notify_error && let Some(notifier) = self.error_notifier.as_ref() {
                notifier();
            }
        }
    }

    pub(super) fn prepare_job(
        self: &Arc<Self>,
        ino: Ext4Ino,
        job: &mut Option<CompletionJob>,
        nr_pages: usize,
    ) -> Result<PreparedWritebackCompletion> {
        if nr_pages == 0 {
            return_errno_with_message!(Errno::EINVAL, "writeback completion has no pages");
        }

        let mut state = self.state.disable_irq().lock();
        if !state.accepting {
            return_errno_with_message!(Errno::ESHUTDOWN, "EXT4 completion engine is stopping");
        }
        if state.inflight_bios >= self.max_inflight_bios
            || state.inflight_pages.saturating_add(nr_pages) > self.max_inflight_pages
        {
            #[cfg(any(feature = "ext4-research-stats", ktest))]
            {
                state.stats.snapshot.credit_stalls += 1;
            }
            return_errno_with_message!(Errno::EBUSY, "EXT4 completion credits exhausted");
        }
        let slot = state
            .slots
            .iter()
            .position(Option::is_none)
            .expect("completion credits guarantee a free preallocated slot");
        let progress = state.inode_progress.entry(ino).or_default();
        progress.next_sequence = progress
            .next_sequence
            .checked_add(1)
            .expect("EXT4 completion sequence overflow");
        let raw_sequence = progress.next_sequence;
        let sequence = WritebackSequence::new(raw_sequence);
        let item = CompletionItem {
            ino,
            sequence,
            raw_sequence,
            nr_pages,
            status: BioStatus::Init,
            suppress_error: false,
            job: job.take().expect("completion job must be present"),
        };
        state.slots[slot] = Some(item);
        state.inflight_bios += 1;
        state.inflight_pages += nr_pages;
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            state.stats.snapshot.submitted_bios =
                state.stats.snapshot.submitted_bios.saturating_add(1);
            state.stats.snapshot.submitted_pages = state
                .stats
                .snapshot
                .submitted_pages
                .saturating_add(nr_pages);
            state.stats.snapshot.inflight_bio_samples =
                state.stats.snapshot.inflight_bio_samples.saturating_add(1);
            state.stats.snapshot.inflight_bio_sum = state
                .stats
                .snapshot
                .inflight_bio_sum
                .saturating_add(state.inflight_bios);
            state.stats.snapshot.inflight_page_sum = state
                .stats
                .snapshot
                .inflight_page_sum
                .saturating_add(state.inflight_pages);
            state.stats.snapshot.peak_inflight_bios = state
                .stats
                .snapshot
                .peak_inflight_bios
                .max(state.inflight_bios);
            state.stats.snapshot.peak_inflight_pages = state
                .stats
                .snapshot
                .peak_inflight_pages
                .max(state.inflight_pages);
        }
        drop(state);

        let engine = self.clone();
        Ok(PreparedWritebackCompletion {
            complete_fn: Box::new(move |status| engine.publish(slot, status)),
            sequence,
        })
    }

    fn publish(&self, slot: usize, status: BioStatus) {
        let mut state = self.state.disable_irq().lock();
        let Some(mut item) = state.slots.get_mut(slot).and_then(Option::take) else {
            debug_assert!(false, "EXT4 writeback completion published twice");
            return;
        };
        item.status = status;
        debug_assert!(state.pending.len() < state.pending.capacity());
        state.pending.push_back(item);
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            state.stats.snapshot.completion_queue_hwm = state
                .stats
                .snapshot
                .completion_queue_hwm
                .max(state.pending.len());
        }
        drop(state);
        self.work_wait_queue.wake_one();
    }

    fn finish_item(
        &self,
        ino: Ext4Ino,
        sequence: WritebackSequence,
        raw_sequence: u64,
        nr_pages: usize,
        error: Option<Error>,
    ) {
        let mut state = self.state.disable_irq().lock();
        state.inflight_bios = state
            .inflight_bios
            .checked_sub(1)
            .expect("EXT4 completion BIO credit underflow");
        state.inflight_pages = state
            .inflight_pages
            .checked_sub(nr_pages)
            .expect("EXT4 completion page credit underflow");
        state.completion_progress = state.completion_progress.wrapping_add(1);
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            state.stats.snapshot.completed_bios =
                state.stats.snapshot.completed_bios.saturating_add(1);
            state.stats.snapshot.completed_pages = state
                .stats
                .snapshot
                .completed_pages
                .saturating_add(nr_pages);
        }

        let progress = state
            .inode_progress
            .get_mut(&ino)
            .expect("prepared completion must have inode progress");
        debug_assert_eq!(sequence.value(), raw_sequence);
        if let Some(error) = error {
            progress.errors.insert(raw_sequence, error);
        }
        if raw_sequence == progress.completed_through + 1 {
            progress.completed_through = raw_sequence;
            while progress
                .completed_out_of_order
                .remove(&(progress.completed_through + 1))
            {
                progress.completed_through += 1;
            }
        } else if raw_sequence > progress.completed_through {
            progress.completed_out_of_order.insert(raw_sequence);
        }
        drop(state);
        self.progress_wait_queue.wake_all();
    }

    pub(super) fn wait_through(&self, ino: Ext4Ino, sequence: WritebackSequence) -> Result<()> {
        self.progress_wait_queue.wait_until(|| {
            let state = self.state.disable_irq().lock();
            let progress = state.inode_progress.get(&ino)?;
            let raw_sequence = sequence.value();
            (progress.completed_through >= raw_sequence).then(|| {
                progress
                    .errors
                    .range(..=raw_sequence)
                    .next()
                    .map(|(_, error)| Err(*error))
                    .unwrap_or(Ok(()))
            })
        })
    }

    pub(super) fn progress_sequence(&self) -> u64 {
        self.state.disable_irq().lock().completion_progress
    }

    pub(super) fn has_errors(&self) -> bool {
        self.state
            .disable_irq()
            .lock()
            .inode_progress
            .values()
            .any(|progress| !progress.errors.is_empty())
    }

    pub(super) fn latest_sequence(&self, ino: Ext4Ino) -> Option<WritebackSequence> {
        self.state
            .disable_irq()
            .lock()
            .inode_progress
            .get(&ino)
            .and_then(|progress| {
                (progress.next_sequence != 0)
                    .then(|| WritebackSequence::new(progress.next_sequence))
            })
    }

    #[cfg(any(feature = "ext4-research-stats", ktest))]
    pub(super) fn stats_snapshot_and_reset(&self) -> CompletionStatsSnapshot {
        let mut state = self.state.disable_irq().lock();
        let inflight_bios = state.inflight_bios;
        let inflight_pages = state.inflight_pages;
        state
            .stats
            .snapshot_and_reset(inflight_bios, inflight_pages)
    }

    pub(super) fn wait_for_progress_since(&self, observed: u64) {
        self.progress_wait_queue.wait_until(|| {
            let state = self.state.disable_irq().lock();
            (!state.accepting || state.stopping || state.completion_progress != observed)
                .then_some(())
        });
    }

    pub(super) fn consume_error_through(
        &self,
        ino: Ext4Ino,
        sequence: WritebackSequence,
    ) -> Option<Error> {
        let mut state = self.state.disable_irq().lock();
        let progress = state.inode_progress.get_mut(&ino)?;
        let keys: Vec<_> = progress
            .errors
            .range(..=sequence.value())
            .map(|(sequence, _)| *sequence)
            .collect();
        let first = keys
            .first()
            .and_then(|sequence| progress.errors.get(sequence))
            .copied();
        for sequence in keys {
            progress.errors.remove(&sequence);
        }
        first
    }

    pub(super) fn finish_submission_failure(
        &self,
        slot_sequence: WritebackSequence,
        ino: Ext4Ino,
        error: Error,
    ) {
        let slot = {
            let mut state = self.state.disable_irq().lock();
            let slot = state.slots.iter().position(|item| {
                item.as_ref()
                    .is_some_and(|item| item.ino == ino && item.sequence == slot_sequence)
            });
            if let Some(item) = slot.and_then(|slot| state.slots[slot].as_mut()) {
                item.suppress_error = error.error() == Errno::EBUSY;
            }
            slot
        };
        if let Some(slot) = slot {
            self.publish(slot, BioStatus::IoError);
        }
    }

    pub(super) fn stop_accepting(&self) {
        self.state.disable_irq().lock().accepting = false;
        self.progress_wait_queue.wake_all();
    }

    pub(super) fn drain(&self) -> Result<()> {
        self.progress_wait_queue.wait_until(|| {
            let state = self.state.disable_irq().lock();
            (state.inflight_bios == 0).then_some(())
        });
        let state = self.state.disable_irq().lock();
        state
            .inode_progress
            .values()
            .flat_map(|progress| progress.errors.values())
            .next()
            .copied()
            .map(Err)
            .unwrap_or(Ok(()))
    }

    pub(super) fn stop_worker(&self) {
        self.state.disable_irq().lock().stopping = true;
        self.work_wait_queue.wake_all();
        self.progress_wait_queue.wake_all();
        if let Some(worker) = self.worker.lock().take() {
            let is_current = Thread::current()
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &worker));
            if !is_current {
                worker.join();
            }
        }
    }
}

pub(super) struct CompletionDispatcher {
    engine: Arc<CompletionEngine>,
    ino: Ext4Ino,
}

impl CompletionDispatcher {
    pub(super) fn new(engine: Arc<CompletionEngine>, ino: Ext4Ino) -> Arc<Self> {
        Arc::new(Self { engine, ino })
    }
}

impl Debug for CompletionDispatcher {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.debug_struct("CompletionDispatcher")
            .field("ino", &self.ino)
            .finish_non_exhaustive()
    }
}

impl WritebackCompletionDispatcher for CompletionDispatcher {
    fn prepare(
        &self,
        completion: &mut Option<WritebackCompletion>,
        nr_pages: usize,
    ) -> Result<PreparedWritebackCompletion> {
        let mut job = Some(CompletionJob::PageCache(
            completion
                .take()
                .expect("PageCache completion must be present"),
        ));
        match self.engine.prepare_job(self.ino, &mut job, nr_pages) {
            Ok(prepared) => Ok(prepared),
            Err(error) => {
                let Some(CompletionJob::PageCache(restored)) = job.take() else {
                    unreachable!("failed preparation must preserve its completion job")
                };
                *completion = Some(restored);
                Err(error)
            }
        }
    }

    fn finish_submission_failure(&self, sequence: WritebackSequence, error: Error) {
        self.engine
            .finish_submission_failure(sequence, self.ino, error);
    }

    fn completion_progress(&self) -> u64 {
        self.engine.progress_sequence()
    }

    fn wait_for_completion_progress_since(&self, observed: u64) {
        self.engine.wait_for_progress_since(observed);
    }

    fn latest_sequence(&self) -> Option<WritebackSequence> {
        self.engine.latest_sequence(self.ino)
    }

    fn wait_through(&self, sequence: WritebackSequence) -> Result<()> {
        self.engine.wait_through(self.ino, sequence)
    }

    fn consume_error_through(&self, sequence: WritebackSequence) -> Option<Error> {
        self.engine.consume_error_through(self.ino, sequence)
    }
}

#[cfg(ktest)]
mod tests {
    use core::sync::atomic::{AtomicBool, Ordering};

    use aster_block::bio::BioStatus;
    use ostd::prelude::ktest;

    use super::{CompletionEngine, CompletionJob};
    use crate::{
        fs::fs_impls::ext4::inode::Ext4Ino,
        prelude::{Arc, Box, Errno, Error, Mutex, Vec, Weak},
        thread::Thread,
    };

    fn prepare_job(
        engine: &Arc<CompletionEngine>,
        ino: Ext4Ino,
        nr_pages: usize,
        job: Box<dyn FnOnce(BioStatus) -> crate::prelude::Result<()> + Send>,
    ) -> crate::prelude::Result<crate::vm::page_cache::PreparedWritebackCompletion> {
        let mut job = Some(CompletionJob::Test(job));
        engine.prepare_job(ino, &mut job, nr_pages)
    }

    fn prepare_mergeable_job(
        engine: &Arc<CompletionEngine>,
        ino: Ext4Ino,
        group: u64,
        observed: Arc<Mutex<Vec<usize>>>,
    ) -> crate::prelude::Result<crate::vm::page_cache::PreparedWritebackCompletion> {
        let mut job = Some(CompletionJob::MergeableTest {
            group,
            merged_jobs: 1,
            observed,
        });
        engine.prepare_job(ino, &mut job, 1)
    }

    #[ktest]
    fn ext4_completion_fence_waits_for_all_lower_sequences_after_out_of_order_device_completion() {
        let engine = CompletionEngine::new(4, 16);
        engine.start().unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));

        let first = prepare_job(&engine, 11, 1, {
            let order = order.clone();
            Box::new(move |_| {
                order.lock().push(1);
                Ok(())
            })
        })
        .unwrap();
        let second = prepare_job(&engine, 11, 1, {
            let order = order.clone();
            Box::new(move |_| {
                order.lock().push(2);
                Ok(())
            })
        })
        .unwrap();

        let waiter_finished = Arc::new(AtomicBool::new(false));
        let waiter = {
            let engine = engine.clone();
            let waiter_finished = waiter_finished.clone();
            let fence = second.sequence;
            crate::thread::kernel_thread::ThreadOptions::new(move || {
                engine.wait_through(11, fence).unwrap();
                waiter_finished.store(true, Ordering::Release);
            })
            .spawn()
        };

        (second.complete_fn)(BioStatus::Complete);
        while order.lock().is_empty() {
            Thread::yield_now();
        }
        assert_eq!(order.lock().as_slice(), &[2]);
        assert!(!waiter_finished.load(Ordering::Acquire));

        (first.complete_fn)(BioStatus::Complete);
        waiter.join();
        assert!(waiter_finished.load(Ordering::Acquire));
        assert_eq!(order.lock().as_slice(), &[2, 1]);

        engine.drain().unwrap();
        engine.stop_worker();
    }

    #[ktest]
    fn ext4_completion_worker_executes_only_one_job_at_a_time() {
        let engine = CompletionEngine::new(4, 16);
        engine.start().unwrap();
        let first_entered = Arc::new(AtomicBool::new(false));
        let release_first = Arc::new(AtomicBool::new(false));
        let second_entered = Arc::new(AtomicBool::new(false));

        let first = prepare_job(&engine, 12, 1, {
            let first_entered = first_entered.clone();
            let release_first = release_first.clone();
            Box::new(move |_| {
                first_entered.store(true, Ordering::Release);
                while !release_first.load(Ordering::Acquire) {
                    Thread::yield_now();
                }
                Ok(())
            })
        })
        .unwrap();
        let second = prepare_job(&engine, 13, 1, {
            let second_entered = second_entered.clone();
            Box::new(move |_| {
                second_entered.store(true, Ordering::Release);
                Ok(())
            })
        })
        .unwrap();

        (first.complete_fn)(BioStatus::Complete);
        while !first_entered.load(Ordering::Acquire) {
            Thread::yield_now();
        }
        (second.complete_fn)(BioStatus::Complete);
        for _ in 0..32 {
            Thread::yield_now();
        }
        assert!(!second_entered.load(Ordering::Acquire));

        release_first.store(true, Ordering::Release);
        engine.drain().unwrap();
        assert!(second_entered.load(Ordering::Acquire));
        engine.stop_worker();
    }

    #[ktest]
    fn ext4_completion_worker_coalesces_adjacent_successful_jobs() {
        let engine = CompletionEngine::new(4, 4);
        let merged_counts = Arc::new(Mutex::new(Vec::new()));
        let first = prepare_mergeable_job(&engine, 21, 7, merged_counts.clone()).unwrap();
        let second = prepare_mergeable_job(&engine, 21, 7, merged_counts.clone()).unwrap();

        (first.complete_fn)(BioStatus::Complete);
        (second.complete_fn)(BioStatus::Complete);
        engine.start().unwrap();
        engine.drain().unwrap();

        assert_eq!(merged_counts.lock().as_slice(), &[2]);
        let stats = engine.stats_snapshot_and_reset();
        assert_eq!(stats.submitted_bios, 2);
        assert_eq!(stats.submitted_pages, 2);
        assert_eq!(stats.completed_bios, 2);
        assert_eq!(stats.completed_pages, 2);
        assert_eq!(stats.inflight_bio_samples, 2);
        assert_eq!(stats.inflight_bio_sum, 3);
        assert_eq!(stats.inflight_page_sum, 3);
        assert_eq!(stats.peak_inflight_bios, 2);
        assert_eq!(stats.peak_inflight_pages, 2);
        assert_eq!(stats.completion_queue_hwm, 2);
        assert_eq!(stats.post_io_jobs, 1);
        assert_eq!(stats.coalesced_jobs, 1);
        assert_eq!(stats.device_failures, 0);
        assert_eq!(stats.post_io_failures, 0);
        engine.stop_worker();
    }

    #[ktest]
    fn ext4_completion_worker_does_not_coalesce_across_failure_or_inode() {
        let engine = CompletionEngine::new(4, 4);
        let merged_counts = Arc::new(Mutex::new(Vec::new()));
        let first = prepare_mergeable_job(&engine, 22, 8, merged_counts.clone()).unwrap();
        let failed = prepare_mergeable_job(&engine, 22, 8, merged_counts.clone()).unwrap();
        let other_inode = prepare_mergeable_job(&engine, 23, 8, merged_counts.clone()).unwrap();

        (first.complete_fn)(BioStatus::Complete);
        (failed.complete_fn)(BioStatus::IoError);
        (other_inode.complete_fn)(BioStatus::Complete);
        engine.start().unwrap();
        assert_eq!(engine.drain().unwrap_err().error(), Errno::EIO);

        assert_eq!(merged_counts.lock().as_slice(), &[1, 1, 1]);
        engine.stop_worker();
    }

    #[ktest]
    fn ext4_completion_worker_requeues_a_rejected_merge_candidate() {
        let engine = CompletionEngine::new(4, 4);
        let merged_counts = Arc::new(Mutex::new(Vec::new()));
        let first = prepare_mergeable_job(&engine, 24, 9, merged_counts.clone()).unwrap();
        let non_adjacent = prepare_mergeable_job(&engine, 24, 10, merged_counts.clone()).unwrap();

        (first.complete_fn)(BioStatus::Complete);
        (non_adjacent.complete_fn)(BioStatus::Complete);
        engine.start().unwrap();
        engine.drain().unwrap();

        assert_eq!(merged_counts.lock().as_slice(), &[1, 1]);
        let state = engine.state.disable_irq().lock();
        assert_eq!(state.inflight_bios, 0);
        assert_eq!(state.inflight_pages, 0);
        assert!(state.slots.iter().all(Option::is_none));
        drop(state);
        engine.stop_worker();
    }

    #[ktest]
    fn ext4_completion_preparation_reserves_bounded_bio_and_page_credits() {
        let engine = CompletionEngine::new(1, 2);
        engine.start().unwrap();
        let first = prepare_job(&engine, 14, 2, Box::new(|_| Ok(()))).unwrap();

        let mut rejected_job = Some(CompletionJob::Test(Box::new(|_| Ok(()))));
        let err = match engine.prepare_job(14, &mut rejected_job, 1) {
            Ok(_) => panic!("completion preparation should exhaust credits"),
            Err(err) => err,
        };
        assert_eq!(err.error(), Errno::EBUSY);
        assert!(rejected_job.is_some());
        assert_eq!(engine.stats_snapshot_and_reset().credit_stalls, 1);

        (first.complete_fn)(BioStatus::Complete);
        engine.drain().unwrap();
        let completed = engine.stats_snapshot_and_reset();
        assert_eq!(completed.completed_bios, 1);
        assert_eq!(completed.device_failures, 0);
        assert_eq!(completed.post_io_failures, 0);
        engine.stop_worker();
    }

    #[ktest]
    fn ext4_completion_errors_remain_visible_to_repeated_fence_waits() {
        let engine = CompletionEngine::new(2, 2);
        engine.start().unwrap();
        let prepared = prepare_job(
            &engine,
            15,
            1,
            Box::new(|_| Err(Error::with_message(Errno::EIO, "test completion failure"))),
        )
        .unwrap();
        let fence = prepared.sequence;

        (prepared.complete_fn)(BioStatus::Complete);
        assert_eq!(
            engine.wait_through(15, fence).unwrap_err().error(),
            Errno::EIO
        );
        assert_eq!(
            engine.wait_through(15, fence).unwrap_err().error(),
            Errno::EIO
        );
        assert_eq!(
            engine.consume_error_through(15, fence).unwrap().error(),
            Errno::EIO
        );
        engine.wait_through(15, fence).unwrap();

        engine.drain().unwrap();
        engine.stop_worker();
    }

    #[ktest]
    fn completion_failure_notifier_runs_after_post_io_failure() {
        let notified = Arc::new(AtomicBool::new(false));
        let error_was_visible = Arc::new(AtomicBool::new(false));
        let engine_ref = Arc::new(Mutex::new(Weak::<CompletionEngine>::new()));
        let notifier = {
            let notified = notified.clone();
            let error_was_visible = error_was_visible.clone();
            let engine_ref = engine_ref.clone();
            Arc::new(move || {
                if let Some(engine) = engine_ref.lock().upgrade() {
                    error_was_visible.store(engine.has_errors(), Ordering::Release);
                }
                notified.store(true, Ordering::Release);
            })
        };
        let engine = CompletionEngine::new_with_error_notifier(1, 1, notifier);
        *engine_ref.lock() = Arc::downgrade(&engine);
        engine.start().unwrap();
        let prepared = prepare_job(
            &engine,
            26,
            1,
            Box::new(|_| Err(Error::with_message(Errno::EIO, "completion failure"))),
        )
        .unwrap();

        (prepared.complete_fn)(BioStatus::Complete);
        assert_eq!(
            engine
                .wait_through(26, prepared.sequence)
                .unwrap_err()
                .error(),
            Errno::EIO
        );
        assert!(notified.load(Ordering::Acquire));
        assert!(error_was_visible.load(Ordering::Acquire));

        engine.stop_accepting();
        assert!(engine.drain().is_err());
        engine.stop_worker();
    }

    #[ktest]
    fn consuming_a_fence_clears_all_errors_through_that_fence() {
        let engine = CompletionEngine::new(2, 2);
        engine.start().unwrap();
        let first = prepare_job(
            &engine,
            25,
            1,
            Box::new(|_| Err(Error::with_message(Errno::EIO, "first failure"))),
        )
        .unwrap();
        let second = prepare_job(
            &engine,
            25,
            1,
            Box::new(|_| Err(Error::with_message(Errno::ENOSPC, "second failure"))),
        )
        .unwrap();
        let fence = second.sequence;

        (first.complete_fn)(BioStatus::Complete);
        (second.complete_fn)(BioStatus::Complete);
        assert_eq!(
            engine.wait_through(25, fence).unwrap_err().error(),
            Errno::EIO
        );
        assert_eq!(
            engine.consume_error_through(25, fence).unwrap().error(),
            Errno::EIO
        );
        engine.wait_through(25, fence).unwrap();

        engine.drain().unwrap();
        engine.stop_worker();
    }

    #[ktest]
    fn ext4_completion_stopping_rejects_new_preparation_and_wakes_the_idle_worker() {
        let engine = CompletionEngine::new(2, 2);
        engine.start().unwrap();
        engine.stop_accepting();

        let mut job = Some(CompletionJob::Test(Box::new(|_| Ok(()))));
        let err = match engine.prepare_job(16, &mut job, 1) {
            Ok(_) => panic!("stopped completion engine accepted new work"),
            Err(err) => err,
        };
        assert_eq!(err.error(), Errno::ESHUTDOWN);
        assert!(job.is_some());

        engine.drain().unwrap();
        engine.stop_worker();
    }

    #[ktest]
    fn ext4_completion_rejects_a_second_worker_start() {
        let engine = CompletionEngine::new(2, 2);
        engine.start().unwrap();
        assert_eq!(engine.start().unwrap_err().error(), Errno::EBUSY);
        engine.stop_accepting();
        engine.drain().unwrap();
        engine.stop_worker();
    }

    #[ktest]
    fn ext4_completion_progress_waits_for_a_later_completion() {
        let engine = CompletionEngine::new(2, 2);
        engine.start().unwrap();
        let observed = engine.progress_sequence();
        let prepared = prepare_job(&engine, 18, 1, Box::new(|_| Ok(()))).unwrap();
        let waiter_finished = Arc::new(AtomicBool::new(false));
        let waiter = {
            let engine = engine.clone();
            let waiter_finished = waiter_finished.clone();
            crate::thread::kernel_thread::ThreadOptions::new(move || {
                engine.wait_for_progress_since(observed);
                waiter_finished.store(true, Ordering::Release);
            })
            .spawn()
        };

        for _ in 0..32 {
            Thread::yield_now();
        }
        assert!(!waiter_finished.load(Ordering::Acquire));
        (prepared.complete_fn)(BioStatus::Complete);
        waiter.join();
        assert!(waiter_finished.load(Ordering::Acquire));

        engine.stop_accepting();
        engine.drain().unwrap();
        engine.stop_worker();
    }

    #[ktest]
    fn ext4_completion_stop_accepting_wakes_progress_waiters() {
        let engine = CompletionEngine::new(2, 2);
        engine.start().unwrap();
        let observed = engine.progress_sequence();
        let waiter_finished = Arc::new(AtomicBool::new(false));
        let waiter = {
            let engine = engine.clone();
            let waiter_finished = waiter_finished.clone();
            crate::thread::kernel_thread::ThreadOptions::new(move || {
                engine.wait_for_progress_since(observed);
                waiter_finished.store(true, Ordering::Release);
            })
            .spawn()
        };

        for _ in 0..32 {
            Thread::yield_now();
        }
        assert!(!waiter_finished.load(Ordering::Acquire));
        engine.stop_accepting();
        waiter.join();
        assert!(waiter_finished.load(Ordering::Acquire));

        engine.drain().unwrap();
        engine.stop_worker();
    }

    #[ktest]
    fn ext4_completion_queue_pressure_does_not_publish_a_persistent_error() {
        let notified = Arc::new(AtomicBool::new(false));
        let notifier = {
            let notified = notified.clone();
            Arc::new(move || notified.store(true, Ordering::Release))
        };
        let engine = CompletionEngine::new_with_error_notifier(2, 2, notifier);
        engine.start().unwrap();
        let prepared = prepare_job(
            &engine,
            19,
            1,
            Box::new(|status| {
                if status == BioStatus::IoError {
                    Err(Error::with_message(Errno::EIO, "test submit failure"))
                } else {
                    Ok(())
                }
            }),
        )
        .unwrap();
        let fence = prepared.sequence;

        engine.finish_submission_failure(
            fence,
            19,
            Error::with_message(Errno::EBUSY, "test queue pressure"),
        );
        engine.wait_through(19, fence).unwrap();
        engine.drain().unwrap();
        assert!(!notified.load(Ordering::Acquire));

        engine.stop_accepting();
        engine.stop_worker();
    }

    #[ktest]
    fn ext4_completion_terminal_submit_failure_remains_visible() {
        let engine = CompletionEngine::new(2, 2);
        engine.start().unwrap();
        let prepared = prepare_job(
            &engine,
            20,
            1,
            Box::new(|status| {
                if status == BioStatus::IoError {
                    Err(Error::with_message(Errno::EIO, "test submit failure"))
                } else {
                    Ok(())
                }
            }),
        )
        .unwrap();
        let fence = prepared.sequence;

        engine.finish_submission_failure(
            fence,
            20,
            Error::with_message(Errno::EIO, "test terminal submission failure"),
        );
        assert_eq!(
            engine.wait_through(20, fence).unwrap_err().error(),
            Errno::EIO
        );
        assert_eq!(engine.drain().unwrap_err().error(), Errno::EIO);

        engine.stop_accepting();
        engine.stop_worker();
    }
}
