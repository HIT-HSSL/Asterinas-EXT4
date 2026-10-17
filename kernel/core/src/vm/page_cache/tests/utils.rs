// SPDX-License-Identifier: MPL-2.0

//! Test-only helpers for deterministic page-cache I/O scheduling.

use alloc::{format, vec};
use core::{
    fmt,
    sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
};

use aster_block::{
    BlockDevice, BlockDeviceMeta, SECTOR_SIZE,
    bio::{Bio, BioCompleteFn, BioEnqueueError, BioSegment, BioStatus, BioType, SubmittedBio},
    id::Sid,
};
use device_id::DeviceId;
use io_util::batch::{IoBatch, IoCompletion as BatchIoCompletion};
use ostd::mm::VmIo;

use crate::{
    prelude::*,
    thread::Thread,
    vm::page_cache::{
        BlockAsPageCacheBackend, CachePage, CachePageExt, LockedCachePage, PageCacheBackend,
        PageCacheWritebackObserver, PreparedWritebackCompletion, WritebackCompletion,
        WritebackCompletionDispatcher, WritebackPage, WritebackSequence,
        submit_one_block_write_page,
    },
};

/// Maximum number of cooperative yields before a test wait times out.
const MAX_WAIT_YIELDS: usize = 1_000_000;

/// Distinguishes read BIOs from write BIOs in the mock backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IoKind {
    Read,
    Write,
}

impl IoKind {
    fn from_bio_type(type_: BioType) -> Self {
        match type_ {
            BioType::Read => Self::Read,
            BioType::Write => Self::Write,
            _ => unimplemented!("mock page-cache backend only supports read/write BIOs"),
        }
    }

    fn bio_type(self) -> BioType {
        match self {
            Self::Read => BioType::Read,
            Self::Write => BioType::Write,
        }
    }
}

/// Selects whether a mock BIO completes inline or waits for the test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IoCompletion {
    Immediate,
    Deferred,
}

#[derive(Debug)]
struct DeferredBio {
    page_idx: usize,
    bio: SubmittedBio,
}

#[derive(Debug)]
struct MockBackendState {
    persisted_pages: Vec<Vec<u8>>,
    read_counts: Vec<usize>,
    write_counts: Vec<usize>,
    read_submit_failures: BTreeSet<usize>,
    write_submit_failures: BTreeSet<usize>,
    read_completion: IoCompletion,
    write_completion: IoCompletion,
    deferred_reads: VecDeque<DeferredBio>,
    deferred_writes: VecDeque<DeferredBio>,
    write_batch_sizes: Vec<usize>,
    fail_write_batch_before_submission: bool,
}

impl MockBackendState {
    fn new(num_pages: usize) -> Self {
        Self {
            persisted_pages: vec![vec![0; PAGE_SIZE]; num_pages],
            read_counts: vec![0; num_pages],
            write_counts: vec![0; num_pages],
            read_submit_failures: BTreeSet::new(),
            write_submit_failures: BTreeSet::new(),
            read_completion: IoCompletion::Immediate,
            write_completion: IoCompletion::Immediate,
            deferred_reads: VecDeque::new(),
            deferred_writes: VecDeque::new(),
            write_batch_sizes: Vec::new(),
            fail_write_batch_before_submission: false,
        }
    }

    fn completion_mode(&self, kind: IoKind) -> IoCompletion {
        match kind {
            IoKind::Read => self.read_completion,
            IoKind::Write => self.write_completion,
        }
    }

    fn set_completion(&mut self, kind: IoKind, completion: IoCompletion) {
        match kind {
            IoKind::Read => self.read_completion = completion,
            IoKind::Write => self.write_completion = completion,
        }
    }

    fn should_fail_submit(&self, kind: IoKind, page_idx: usize) -> bool {
        let failures = match kind {
            IoKind::Read => &self.read_submit_failures,
            IoKind::Write => &self.write_submit_failures,
        };
        failures.contains(&page_idx)
    }

    fn record_submission(&mut self, kind: IoKind, page_idx: usize) {
        match kind {
            IoKind::Read => self.read_counts[page_idx] += 1,
            IoKind::Write => self.write_counts[page_idx] += 1,
        }
    }

    fn deferred_bios(&self, kind: IoKind) -> &VecDeque<DeferredBio> {
        match kind {
            IoKind::Read => &self.deferred_reads,
            IoKind::Write => &self.deferred_writes,
        }
    }

    fn deferred_bios_mut(&mut self, kind: IoKind) -> &mut VecDeque<DeferredBio> {
        match kind {
            IoKind::Read => &mut self.deferred_reads,
            IoKind::Write => &mut self.deferred_writes,
        }
    }

    fn submission_count(&self, kind: IoKind, page_idx: usize) -> usize {
        match kind {
            IoKind::Read => self.read_counts[page_idx],
            IoKind::Write => self.write_counts[page_idx],
        }
    }
}

/// A mock backend that lets tests observe and manually drive page-cache I/O.
///
/// It models a page-addressable backend directly, so tests can coordinate
/// backend completion order without an extra fake-device wrapper or timer-based
/// sleeps.
pub(super) struct MockPageCacheBackend {
    state: SpinLock<MockBackendState>,
    num_pages: usize,
    observer: Arc<MockWritebackObserver>,
}

#[derive(Debug)]
struct MockWritebackObserver {
    outstanding_pages: AtomicUsize,
    dispatcher: Option<Arc<MockWritebackDispatcher>>,
}

#[derive(Debug)]
pub(super) struct MockWritebackDispatcher {
    state: Mutex<MockWritebackDispatcherState>,
    pause_before_prepare: AtomicBool,
    prepare_entered: AtomicBool,
}

impl Default for MockWritebackDispatcher {
    fn default() -> Self {
        Self {
            state: Mutex::new(MockWritebackDispatcherState::default()),
            pause_before_prepare: AtomicBool::new(false),
            prepare_entered: AtomicBool::new(false),
        }
    }
}

struct MockPreparedCompletion {
    sequence: WritebackSequence,
    completion: WritebackCompletion,
    status: Arc<AtomicU32>,
}

#[derive(Default)]
struct MockWritebackDispatcherState {
    prepared: VecDeque<MockPreparedCompletion>,
    next_sequence: u64,
    completion_progress: u64,
    fail_prepare: bool,
    max_prepared: Option<usize>,
    wait_count: usize,
    errors: Vec<(WritebackSequence, Error)>,
}

impl Debug for MockWritebackDispatcherState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MockWritebackDispatcherState")
            .field("prepared_count", &self.prepared.len())
            .field("next_sequence", &self.next_sequence)
            .finish()
    }
}

impl MockWritebackDispatcher {
    pub(super) fn finish_next(&self) -> Option<Result<()>> {
        let prepared = self.state.lock().prepared.pop_front()?;
        let result = {
            let status = BioStatus::try_from(prepared.status.load(Ordering::Acquire))
                .unwrap_or(BioStatus::IoError);
            prepared.completion.finish(status)
        };
        if let Err(error) = result {
            self.state.lock().errors.push((prepared.sequence, error));
        }
        self.state.lock().completion_progress += 1;
        Some(result)
    }

    pub(super) fn prepared_count(&self) -> usize {
        self.state.lock().prepared.len()
    }

    pub(super) fn set_fail_prepare(&self, fail: bool) {
        self.state.lock().fail_prepare = fail;
    }

    pub(super) fn set_max_prepared(&self, max_prepared: usize) {
        self.state.lock().max_prepared = Some(max_prepared);
    }

    pub(super) fn wait_count(&self) -> usize {
        self.state.lock().wait_count
    }

    pub(super) fn pause_prepare(&self) {
        self.prepare_entered.store(false, Ordering::Release);
        self.pause_before_prepare.store(true, Ordering::Release);
    }

    pub(super) fn wait_until_prepare_entered(&self) {
        while !self.prepare_entered.load(Ordering::Acquire) {
            Thread::yield_now();
        }
    }

    pub(super) fn resume_prepare(&self) {
        self.pause_before_prepare.store(false, Ordering::Release);
    }
}

impl WritebackCompletionDispatcher for MockWritebackDispatcher {
    fn prepare(
        &self,
        completion: &mut Option<WritebackCompletion>,
        _nr_pages: usize,
    ) -> Result<PreparedWritebackCompletion> {
        if self.pause_before_prepare.load(Ordering::Acquire) {
            self.prepare_entered.store(true, Ordering::Release);
            while self.pause_before_prepare.load(Ordering::Acquire) {
                Thread::yield_now();
            }
        }
        if self.state.lock().fail_prepare {
            return_errno!(Errno::EIO);
        }
        {
            let state = self.state.lock();
            if state
                .max_prepared
                .is_some_and(|limit| state.prepared.len() >= limit)
            {
                return_errno!(Errno::EBUSY);
            }
        }
        let status = Arc::new(AtomicU32::new(BioStatus::Init as u32));
        let completion_status = status.clone();
        let mut state = self.state.lock();
        state.next_sequence += 1;
        let sequence = WritebackSequence::new(state.next_sequence);
        state.prepared.push_back(MockPreparedCompletion {
            sequence,
            completion: completion.take().unwrap(),
            status,
        });
        Ok(PreparedWritebackCompletion {
            complete_fn: Box::new(move |status| {
                completion_status.store(status as u32, Ordering::Release);
            }),
            sequence,
        })
    }

    fn finish_submission_failure(&self, sequence: WritebackSequence, _error: Error) {
        let mut state = self.state.lock();
        let index = state
            .prepared
            .iter()
            .position(|prepared| prepared.sequence == sequence)
            .expect("submission failure must match a prepared completion");
        let prepared = state.prepared.remove(index).unwrap();
        drop(state);
        assert!(prepared.completion.finish(BioStatus::IoError).is_err());
        self.state.lock().completion_progress += 1;
    }

    fn completion_progress(&self) -> u64 {
        self.state.lock().completion_progress
    }

    fn wait_for_completion_progress_since(&self, observed: u64) {
        if self.state.lock().completion_progress != observed {
            return;
        }
        let Some(sequence) = self.latest_sequence() else {
            return;
        };
        let _ = self.wait_through(sequence);
    }

    fn latest_sequence(&self) -> Option<WritebackSequence> {
        let sequence = self.state.lock().next_sequence;
        (sequence != 0).then(|| WritebackSequence::new(sequence))
    }

    fn wait_through(&self, sequence: WritebackSequence) -> Result<()> {
        self.state.lock().wait_count += 1;
        let mut first_error = self
            .state
            .lock()
            .errors
            .iter()
            .find(|(error_sequence, _)| *error_sequence <= sequence)
            .map(|(_, error)| *error);
        loop {
            let ready = {
                let state = self.state.lock();
                let Some(index) = state
                    .prepared
                    .iter()
                    .position(|prepared| prepared.sequence <= sequence)
                else {
                    return match first_error {
                        Some(error) => Err(error),
                        None => Ok(()),
                    };
                };
                (state.prepared[index].status.load(Ordering::Acquire) != BioStatus::Init as u32)
                    .then_some(index)
            };
            let Some(index) = ready else {
                Thread::yield_now();
                continue;
            };
            let prepared = self.state.lock().prepared.remove(index).unwrap();
            let status = BioStatus::try_from(prepared.status.load(Ordering::Acquire))
                .unwrap_or(BioStatus::IoError);
            if let Err(error) = prepared.completion.finish(status) {
                self.state.lock().errors.push((prepared.sequence, error));
                first_error.get_or_insert(error);
            }
            self.state.lock().completion_progress += 1;
        }
    }

    fn consume_error_through(&self, sequence: WritebackSequence) -> Option<Error> {
        let mut state = self.state.lock();
        let first_error = state
            .errors
            .iter()
            .find(|(error_sequence, _)| *error_sequence <= sequence)
            .map(|(_, error)| *error);
        state
            .errors
            .retain(|(error_sequence, _)| *error_sequence > sequence);
        first_error
    }
}

impl PageCacheWritebackObserver for MockWritebackObserver {
    fn page_dirtied(&self, _page_idx: usize) {
        self.outstanding_pages.fetch_add(1, Ordering::AcqRel);
    }

    fn page_cleaned(&self, _page_idx: usize) {
        let previous = self.outstanding_pages.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0);
    }

    fn writeback_completion_dispatcher(&self) -> Option<Arc<dyn WritebackCompletionDispatcher>> {
        self.dispatcher
            .as_ref()
            .map(|dispatcher| dispatcher.clone() as Arc<dyn WritebackCompletionDispatcher>)
    }
}

/// A direct backend whose observer has a dispatcher, but whose write path does
/// not opt into detached completion ownership.
#[derive(Debug)]
pub(super) struct MockDirectPageCacheBackend {
    observer: Arc<MockWritebackObserver>,
    write_submitted: Arc<AtomicBool>,
    write_completed: Arc<AtomicBool>,
}

impl MockDirectPageCacheBackend {
    pub(super) fn new() -> Self {
        Self {
            observer: Arc::new(MockWritebackObserver {
                outstanding_pages: AtomicUsize::new(0),
                dispatcher: Some(Arc::new(MockWritebackDispatcher::default())),
            }),
            write_submitted: Arc::new(AtomicBool::new(false)),
            write_completed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn wait_for_write_submission(&self) {
        wait_until(|| self.write_submitted.load(Ordering::Acquire));
    }

    pub(super) fn complete_write(&self) {
        self.write_completed.store(true, Ordering::Release);
    }
}

struct MockDirectWriteCompletion {
    page: CachePage,
    completed: Arc<AtomicBool>,
}

impl BatchIoCompletion for MockDirectWriteCompletion {
    fn wait(&self) -> core::result::Result<(), io_util::IoError> {
        wait_until(|| self.completed.load(Ordering::Acquire));
        self.page.clone().lock().clear_writing_back();
        Ok(())
    }
}

impl PageCacheBackend for MockDirectPageCacheBackend {
    fn writeback_observer(&self) -> Option<Arc<dyn PageCacheWritebackObserver>> {
        Some(self.observer.clone())
    }

    fn read_page_async(
        &self,
        _idx: usize,
        _locked_page: LockedCachePage,
        _io_batch: &mut IoBatch,
    ) -> Result<()> {
        unreachable!("dispatcher opt-in test does not submit I/O")
    }

    fn write_page_async(
        &self,
        _idx: usize,
        locked_page: LockedCachePage,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        locked_page.set_writing_back();
        locked_page.set_up_to_date();
        io_batch.push(Arc::new(MockDirectWriteCompletion {
            page: locked_page.unlock(),
            completed: self.write_completed.clone(),
        }));
        self.write_submitted.store(true, Ordering::Release);
        Ok(())
    }
}

impl MockPageCacheBackend {
    /// Creates a backend with `num_pages` zero-filled persisted pages.
    pub(super) fn new(num_pages: usize) -> Arc<Self> {
        Arc::new(Self {
            state: SpinLock::new(MockBackendState::new(num_pages)),
            num_pages,
            observer: Arc::new(MockWritebackObserver {
                outstanding_pages: AtomicUsize::new(0),
                dispatcher: None,
            }),
        })
    }

    pub(super) fn new_with_detached_writeback(
        num_pages: usize,
    ) -> (Arc<Self>, Arc<MockWritebackDispatcher>) {
        let dispatcher = Arc::new(MockWritebackDispatcher::default());
        let backend = Arc::new(Self {
            state: SpinLock::new(MockBackendState::new(num_pages)),
            num_pages,
            observer: Arc::new(MockWritebackObserver {
                outstanding_pages: AtomicUsize::new(0),
                dispatcher: Some(dispatcher.clone()),
            }),
        });
        (backend, dispatcher)
    }

    /// Sets how future BIOs of `kind` complete.
    pub(super) fn set_completion(&self, kind: IoKind, completion: IoCompletion) {
        self.state.lock().set_completion(kind, completion);
    }

    /// Waits until the backend has queued `expected_count` deferred BIOs.
    ///
    /// Tests use this to freeze the backend at the "BIO submitted but not yet
    /// completed" point before releasing the next scheduling step.
    pub(super) fn wait_for_deferred_bios(&self, kind: IoKind, expected_count: usize) {
        let failure_message =
            format!("timed out waiting for {expected_count} deferred {kind:?} BIOs");
        wait_until_with_failure_message(
            || self.state.lock().deferred_bios(kind).len() >= expected_count,
            &failure_message,
        );
    }

    /// Completes the next deferred BIO of `kind`.
    pub(super) fn complete_next_deferred_bio(&self, kind: IoKind, success: bool) -> bool {
        let deferred_bio = self.state.lock().deferred_bios_mut(kind).pop_front();
        let Some(deferred_bio) = deferred_bio else {
            return false;
        };

        self.complete_bio(kind, deferred_bio.page_idx, deferred_bio.bio, success);
        true
    }

    /// Returns how many read BIOs were submitted for `page_idx`.
    pub(super) fn read_count(&self, page_idx: usize) -> usize {
        self.state.lock().submission_count(IoKind::Read, page_idx)
    }

    /// Returns how many write BIOs were submitted for `page_idx`.
    pub(super) fn write_count(&self, page_idx: usize) -> usize {
        self.state.lock().submission_count(IoKind::Write, page_idx)
    }

    /// Returns the page counts delivered through each batched write call.
    pub(super) fn write_batch_sizes(&self) -> Vec<usize> {
        self.state.lock().write_batch_sizes.clone()
    }

    pub(super) fn outstanding_pages(&self) -> usize {
        self.observer.outstanding_pages.load(Ordering::Acquire)
    }

    pub(super) fn set_fail_write_batch_before_submission(&self, fail: bool) {
        self.state.lock().fail_write_batch_before_submission = fail;
    }

    pub(super) fn set_write_submit_failure(&self, page_idx: usize, fail: bool) {
        let mut state = self.state.lock();
        if fail {
            state.write_submit_failures.insert(page_idx);
        } else {
            state.write_submit_failures.remove(&page_idx);
        }
    }

    /// Returns the bytes currently persisted for `page_idx`.
    pub(super) fn persisted_page_bytes(&self, page_idx: usize) -> Vec<u8> {
        self.state.lock().persisted_pages[page_idx].clone()
    }

    /// Preloads the persisted bytes for `page_idx`.
    pub(super) fn set_persisted_page_bytes(&self, page_idx: usize, data: &[u8]) {
        assert_eq!(data.len(), PAGE_SIZE);
        self.state.lock().persisted_pages[page_idx].copy_from_slice(data);
    }

    fn maybe_fail_submission(&self, kind: IoKind, page_idx: usize) -> Result<()> {
        let mut state = self.state.lock();
        if state.should_fail_submit(kind, page_idx) {
            state.record_submission(kind, page_idx);
            return_errno!(Errno::EIO);
        }

        Ok(())
    }

    fn submit_bio(
        &self,
        kind: IoKind,
        page_idx: usize,
        bio_segment: BioSegment,
        complete_fn: BioCompleteFn,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        self.maybe_fail_submission(kind, page_idx)?;

        let bio = Bio::new(
            kind.bio_type(),
            Sid::from_offset(page_idx * PAGE_SIZE),
            vec![bio_segment],
            Some(complete_fn),
        );
        bio.submit(self, io_batch)
            .map_err(ostd::Error::from)
            .map_err(Error::from)
    }

    fn complete_bio(&self, kind: IoKind, page_idx: usize, bio: SubmittedBio, success: bool) {
        let status = {
            let mut state = self.state.lock();
            if success {
                let segment = &bio.segments()[0];
                match kind {
                    IoKind::Read => segment
                        .inner_dma()
                        .write_bytes(0, &state.persisted_pages[page_idx])
                        .unwrap(),
                    IoKind::Write => {
                        let mut persisted_page = vec![0; PAGE_SIZE];
                        segment
                            .inner_dma()
                            .read_bytes(0, &mut persisted_page)
                            .unwrap();
                        state.persisted_pages[page_idx] = persisted_page;
                    }
                }
                BioStatus::Complete
            } else {
                BioStatus::IoError
            }
        };

        bio.complete(status);
    }
}

/// Waits until a test predicate becomes true.
pub(super) fn wait_until(mut condition: impl FnMut() -> bool) {
    wait_until_with_failure_message(&mut condition, "timed out waiting for test condition");
}

fn wait_until_with_failure_message(mut condition: impl FnMut() -> bool, failure_message: &str) {
    for _ in 0..MAX_WAIT_YIELDS {
        if condition() {
            return;
        }

        Thread::yield_now();
    }

    panic!("{failure_message}");
}

impl Debug for MockPageCacheBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MockPageCacheBackend")
            .field("num_pages", &self.num_pages)
            .finish_non_exhaustive()
    }
}

impl BlockDevice for MockPageCacheBackend {
    fn enqueue(&self, bio: SubmittedBio) -> Result<(), BioEnqueueError> {
        let page_idx = bio.sid_range().start.to_offset() / PAGE_SIZE;
        let kind = IoKind::from_bio_type(bio.type_());

        let completion = {
            let mut state = self.state.lock();
            state.record_submission(kind, page_idx);
            let completion = state.completion_mode(kind);
            if matches!(completion, IoCompletion::Deferred) {
                state
                    .deferred_bios_mut(kind)
                    .push_back(DeferredBio { page_idx, bio });
                return Ok(());
            }
            completion
        };

        debug_assert_eq!(completion, IoCompletion::Immediate);
        self.complete_bio(kind, page_idx, bio, true);
        Ok(())
    }

    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            max_nr_segments_per_bio: 1,
            nr_sectors: self.num_pages * (PAGE_SIZE / SECTOR_SIZE),
        }
    }

    fn name(&self) -> &str {
        "mock-page-cache"
    }

    fn id(&self) -> DeviceId {
        DeviceId::null()
    }
}

impl BlockAsPageCacheBackend for MockPageCacheBackend {
    fn writeback_observer(&self) -> Option<Arc<dyn PageCacheWritebackObserver>> {
        Some(self.observer.clone())
    }

    fn submit_read_bio(
        &self,
        page_idx: usize,
        bio_segment: BioSegment,
        complete_fn: BioCompleteFn,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        if page_idx >= self.num_pages {
            return_errno!(Errno::EINVAL);
        }
        self.submit_bio(IoKind::Read, page_idx, bio_segment, complete_fn, io_batch)
    }

    fn submit_write_bio(
        &self,
        page_idx: usize,
        bio_segment: BioSegment,
        complete_fn: BioCompleteFn,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        if page_idx >= self.num_pages {
            return_errno!(Errno::EINVAL);
        }
        self.submit_bio(IoKind::Write, page_idx, bio_segment, complete_fn, io_batch)
    }

    fn submit_write_pages(&self, pages: Vec<WritebackPage>, io_batch: &mut IoBatch) -> Result<()> {
        {
            let mut state = self.state.lock();
            if state.fail_write_batch_before_submission {
                return_errno!(Errno::EIO);
            }
            state.write_batch_sizes.push(pages.len());
        }
        for page in pages {
            submit_one_block_write_page(self, page, io_batch)?;
        }
        Ok(())
    }
}
