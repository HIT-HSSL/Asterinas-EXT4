// SPDX-License-Identifier: MPL-2.0

//! Mount-local dirty-page accounting, background writeback, and throttling.

mod completion;

use core::{
    fmt,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[cfg(any(feature = "ext4-research-stats", ktest))]
use completion::CompletionStatsSnapshot;
use completion::{CompletionDispatcher, CompletionEngine};
use ostd::sync::{WaitQueue, Waiter};

use super::{fs::Ext4, inode::Inode, prelude::*};
use crate::{
    thread::{AsThread, Thread, kernel_thread::ThreadOptions},
    vm::page_cache::{PageCacheWritebackObserver, WritebackCompletionDispatcher},
};

/// Default maximum number of pages snapshotted by one writeback pass.
pub(super) const WRITEBACK_BATCH_PAGES: usize = 1024;

const MAX_INFLIGHT_WRITEBACK_BIOS: usize = 1024;
const MAX_INFLIGHT_WRITEBACK_PAGES: usize = WRITEBACK_BATCH_PAGES * 2;

/// Dirty-page thresholds for one mounted EXT4 instance.
#[derive(Clone, Copy, Debug)]
pub(super) struct WritebackConfig {
    background_pages: usize,
    high_pages: usize,
    low_pages: usize,
}

impl WritebackConfig {
    pub(super) fn from_memory_bytes(memory_bytes: usize) -> Self {
        let total_pages = memory_bytes / PAGE_SIZE;
        Self::from_pages(
            (total_pages / 10).max(1),
            (total_pages / 5).max(2),
            (total_pages / 10).max(1),
        )
    }

    pub(super) fn from_capacity_bytes(capacity_bytes: usize) -> Self {
        let high_pages = (capacity_bytes / PAGE_SIZE).max(2);
        let background_pages = (high_pages / 2).max(1);
        Self::from_pages(background_pages, high_pages, background_pages)
    }

    pub(super) const fn from_pages(
        background_pages: usize,
        high_pages: usize,
        low_pages: usize,
    ) -> Self {
        assert!(low_pages <= background_pages);
        assert!(background_pages < high_pages);
        Self {
            background_pages,
            high_pages,
            low_pages,
        }
    }
}

#[derive(Default)]
struct WritebackQueue {
    inodes: VecDeque<(usize, Weak<Inode>)>,
    queued: BTreeSet<usize>,
}

#[derive(Default)]
struct SubmissionGate {
    active_submitters: usize,
    waiting_submitters: usize,
    quiesce_active: bool,
    waiting_quiesces: usize,
    submitter_permits: usize,
}

/// Coordinates writeback only for the EXT4 mount that owns this instance.
pub(super) struct PageCacheWritebackController {
    config: WritebackConfig,
    outstanding_pages: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    foreground_throttle_stalls: AtomicUsize,
    throttling: AtomicBool,
    failed: Arc<AtomicBool>,
    stopping: AtomicBool,
    started: AtomicBool,
    queue: SpinLock<WritebackQueue>,
    wait_queue: Arc<WaitQueue>,
    submission_gate: SpinLock<SubmissionGate>,
    submission_wait_queue: WaitQueue,
    worker: Mutex<Option<Arc<Thread>>>,
    completion: Arc<CompletionEngine>,
}

impl PageCacheWritebackController {
    pub(super) fn new(config: WritebackConfig) -> Self {
        let failed = Arc::new(AtomicBool::new(false));
        let wait_queue = Arc::new(WaitQueue::new());
        let error_notifier = {
            let failed = failed.clone();
            let wait_queue = wait_queue.clone();
            Arc::new(move || {
                failed.store(true, Ordering::Release);
                wait_queue.wake_all();
            })
        };
        Self {
            config,
            outstanding_pages: AtomicUsize::new(0),
            #[cfg(any(feature = "ext4-research-stats", ktest))]
            foreground_throttle_stalls: AtomicUsize::new(0),
            throttling: AtomicBool::new(false),
            failed,
            stopping: AtomicBool::new(false),
            started: AtomicBool::new(false),
            queue: SpinLock::new(WritebackQueue::default()),
            wait_queue,
            submission_gate: SpinLock::new(SubmissionGate::default()),
            submission_wait_queue: WaitQueue::new(),
            worker: Mutex::new(None),
            completion: CompletionEngine::new_with_error_notifier(
                MAX_INFLIGHT_WRITEBACK_BIOS,
                MAX_INFLIGHT_WRITEBACK_PAGES,
                error_notifier,
            ),
        }
    }

    pub(super) fn start(self: &Arc<Self>) -> Result<()> {
        if self
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return_errno_with_message!(Errno::EBUSY, "EXT4 writeback worker already started");
        }
        if let Err(err) = self.completion.start() {
            self.started.store(false, Ordering::Release);
            return Err(err);
        }

        let controller = self.clone();
        let task = ThreadOptions::new(move || controller.worker_loop()).build();
        let thread = task.as_thread().unwrap().clone();
        self.worker.lock().replace(thread.clone());
        thread.run();
        Ok(())
    }

    fn worker_loop(&self) {
        loop {
            self.wait_queue.wait_until(|| {
                (self.stopping.load(Ordering::Acquire) || !self.queue.lock().inodes.is_empty())
                    .then_some(())
            });
            if self.stopping.load(Ordering::Acquire) {
                break;
            }

            let Some(inode) = self.pop_dirty_inode() else {
                continue;
            };
            let ino = inode.ino();
            let completion_progress = self.completion.progress_sequence();
            let batch_result = self.with_submission_allowed(|| {
                if self.stopping.load(Ordering::Acquire) {
                    return Ok(None);
                }
                inode.writeback_one_batch(WRITEBACK_BATCH_PAGES).map(Some)
            });
            match batch_result {
                Ok(Some(result)) if result.had_work() => {
                    self.clear_failure_if_resolved();
                    self.enqueue_inode(inode);
                }
                Ok(Some(result)) if result.retry_after_completion() => {
                    self.clear_failure_if_resolved();
                    self.enqueue_inode(inode);
                    self.completion.wait_for_progress_since(completion_progress);
                }
                Ok(Some(_)) => {
                    self.clear_failure_if_resolved();
                }
                Ok(None) => break,
                Err(err) if err.error() == Errno::EBUSY => {
                    self.enqueue_inode(inode);
                    self.completion.wait_for_progress_since(completion_progress);
                }
                Err(err) => {
                    ostd::error!("EXT4 background writeback failed for inode {ino}: {err:?}");
                    self.failed.store(true, Ordering::Release);
                    self.wait_queue.wake_all();
                    let (retry_waiter, _) = Waiter::new_pair();
                    let retry_delay = Duration::from_millis(100);
                    let _ = retry_waiter.wait_until_or_timeout(
                        || self.stopping.load(Ordering::Acquire).then_some(()),
                        &retry_delay,
                    );
                    if !self.stopping.load(Ordering::Acquire) {
                        self.enqueue_inode(inode);
                    }
                }
            }
        }
    }

    pub(super) fn stop(&self) {
        if !self.stop_accepting() {
            return;
        }
        self.stop_submit_worker();
        if let Err(err) = self.drain() {
            ostd::error!("EXT4 completion drain failed: {err:?}");
            self.failed.store(true, Ordering::Release);
        }
        self.stop_workers();
    }

    fn stop_accepting(&self) -> bool {
        if self.stopping.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.throttling.store(false, Ordering::Release);
        self.completion.stop_accepting();
        self.wait_queue.wake_all();
        true
    }

    fn stop_submit_worker(&self) {
        if let Some(worker) = self.worker.lock().take() {
            let is_current = Thread::current()
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &worker));
            if !is_current {
                worker.join();
            }
        }
    }

    pub(super) fn drain(&self) -> Result<()> {
        self.completion.drain()
    }

    pub(super) fn with_submission_allowed<T>(
        &self,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let must_wait = {
            let mut gate = self.submission_gate.lock();
            if !gate.quiesce_active && gate.waiting_quiesces == 0 {
                gate.active_submitters += 1;
                false
            } else {
                gate.waiting_submitters += 1;
                true
            }
        };
        if must_wait {
            self.submission_wait_queue.wait_until(|| {
                let mut gate = self.submission_gate.lock();
                if gate.quiesce_active || (gate.waiting_quiesces > 0 && gate.submitter_permits == 0)
                {
                    return None;
                }
                gate.waiting_submitters -= 1;
                gate.active_submitters += 1;
                if gate.waiting_quiesces > 0 {
                    gate.submitter_permits -= 1;
                }
                Some(())
            });
        }

        let result = operation();
        let mut gate = self.submission_gate.lock();
        gate.active_submitters -= 1;
        let wake_quiesce = gate.active_submitters == 0;
        drop(gate);
        if wake_quiesce {
            self.submission_wait_queue.wake_all();
        }
        result
    }

    pub(super) fn with_submissions_quiesced<T>(
        &self,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.submission_gate.lock().waiting_quiesces += 1;
        self.submission_wait_queue.wait_until(|| {
            let mut gate = self.submission_gate.lock();
            if gate.quiesce_active || gate.active_submitters > 0 || gate.submitter_permits > 0 {
                return None;
            }
            gate.waiting_quiesces -= 1;
            gate.quiesce_active = true;
            Some(())
        });

        let result = operation();
        let mut gate = self.submission_gate.lock();
        gate.quiesce_active = false;
        if gate.waiting_quiesces > 0 && gate.waiting_submitters > 0 {
            gate.submitter_permits = gate.waiting_submitters;
        }
        drop(gate);
        self.submission_wait_queue.wake_all();
        result
    }

    pub(super) fn clear_failure_if_resolved(&self) {
        if !self.completion.has_errors() {
            self.failed.store(false, Ordering::Release);
            self.wait_queue.wake_all();
        }
    }

    fn stop_workers(&self) {
        self.completion.stop_worker();
    }

    pub(super) fn page_dirtied(&self, inode: Arc<Inode>) {
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        self.account_page_dirtied();
        self.enqueue_inode(inode);
    }

    fn account_page_dirtied(&self) {
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        let outstanding = self.outstanding_pages.fetch_add(1, Ordering::AcqRel) + 1;
        if outstanding >= self.config.high_pages {
            self.throttling.store(true, Ordering::Release);
        }
        if outstanding >= self.config.background_pages {
            self.wait_queue.wake_one();
        }
    }

    pub(super) fn page_cleaned(&self) {
        let previous = self
            .outstanding_pages
            .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_sub(1)
            })
            .expect("EXT4 writeback accounting underflow");
        if previous.saturating_sub(1) <= self.config.low_pages {
            self.throttling.store(false, Ordering::Release);
            self.wait_queue.wake_all();
        }
    }

    pub(super) fn enqueue_inode(&self, inode: Arc<Inode>) {
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        let key = Arc::as_ptr(&inode) as usize;
        let mut queue = self.queue.lock();
        if queue.queued.insert(key) {
            queue.inodes.push_back((key, Arc::downgrade(&inode)));
        }
    }

    pub(super) fn pop_dirty_inode(&self) -> Option<Arc<Inode>> {
        let mut queue = self.queue.lock();
        while let Some((key, inode)) = queue.inodes.pop_front() {
            queue.queued.remove(&key);
            if let Some(inode) = inode.upgrade() {
                return Some(inode);
            }
        }
        None
    }

    pub(super) fn should_throttle(&self) -> bool {
        self.throttling.load(Ordering::Acquire)
    }

    pub(super) fn balance_dirty_pages(&self) -> Result<()> {
        if self.stopping.load(Ordering::Acquire) {
            return_errno_with_message!(Errno::ESHUTDOWN, "EXT4 writeback controller is stopping");
        }
        if self.failed.load(Ordering::Acquire) {
            return_errno_with_message!(Errno::EIO, "EXT4 background writeback failed");
        }
        if !self.should_throttle() {
            return Ok(());
        }
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            self.foreground_throttle_stalls
                .fetch_add(1, Ordering::Relaxed);
        }
        self.wait_queue.wait_until(|| {
            (!self.should_throttle()
                || self.failed.load(Ordering::Acquire)
                || self.stopping.load(Ordering::Acquire))
            .then_some(())
        });
        if self.failed.load(Ordering::Acquire) {
            return_errno_with_message!(Errno::EIO, "EXT4 background writeback failed");
        }
        if self.stopping.load(Ordering::Acquire) {
            return_errno_with_message!(Errno::ESHUTDOWN, "EXT4 writeback controller is stopping");
        }
        Ok(())
    }

    #[cfg(ktest)]
    fn notify_completion_failure(&self) {
        self.failed.store(true, Ordering::Release);
        self.wait_queue.wake_all();
    }

    pub(super) fn write_chunk_bytes(&self) -> usize {
        self.config
            .high_pages
            .saturating_sub(self.config.low_pages)
            .clamp(1, WRITEBACK_BATCH_PAGES)
            * PAGE_SIZE
    }

    #[cfg(any(feature = "ext4-research-stats", ktest))]
    pub(super) fn stats_snapshot_and_reset(&self) -> WritebackStatsSnapshot {
        WritebackStatsSnapshot {
            completion: self.completion.stats_snapshot_and_reset(),
            foreground_throttle_stalls: self.foreground_throttle_stalls.swap(0, Ordering::Relaxed),
        }
    }

    fn completion_dispatcher(&self, ino: Ext4Ino) -> Arc<dyn WritebackCompletionDispatcher> {
        CompletionDispatcher::new(self.completion.clone(), ino)
    }

    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn outstanding_pages(&self) -> usize {
        self.outstanding_pages.load(Ordering::Acquire)
    }
}

#[cfg(any(feature = "ext4-research-stats", ktest))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::fs::fs_impls::ext4) struct WritebackStatsSnapshot {
    pub(in crate::fs::fs_impls::ext4) completion: CompletionStatsSnapshot,
    pub(in crate::fs::fs_impls::ext4) foreground_throttle_stalls: usize,
}

pub(super) struct Ext4PageWritebackObserver {
    controller: Weak<PageCacheWritebackController>,
    inode: SpinLock<Weak<Inode>>,
    ino: Ext4Ino,
}

impl Debug for Ext4PageWritebackObserver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ext4PageWritebackObserver")
            .field("ino", &self.ino)
            .finish_non_exhaustive()
    }
}

impl Ext4PageWritebackObserver {
    pub(super) fn new(fs: Weak<Ext4>, ino: Ext4Ino) -> Arc<Self> {
        let controller = fs
            .upgrade()
            .map(|fs| Arc::downgrade(fs.writeback_controller()))
            .unwrap_or_default();
        Arc::new(Self {
            controller,
            inode: SpinLock::new(Weak::new()),
            ino,
        })
    }

    pub(super) fn bind_inode(&self, inode: Weak<Inode>) {
        *self.inode.lock() = inode;
    }

    pub(super) fn touch_metadata_range(&self, fs: &Ext4, range: Range<Ext4Bid>) {
        let inode = self.inode.lock().upgrade();
        if let Some(inode) = inode {
            inode.touch_metadata_range(fs, range);
        }
    }

    pub(super) fn touch_released_metadata_range(&self, fs: &Ext4, range: Range<Ext4Bid>) {
        let inode = self.inode.lock().upgrade();
        if let Some(inode) = inode {
            inode.touch_released_metadata_range(fs, range);
        }
    }
}

impl PageCacheWritebackObserver for Ext4PageWritebackObserver {
    fn page_dirtied(&self, _page_idx: usize) {
        if let (Some(controller), Some(inode)) =
            (self.controller.upgrade(), self.inode.lock().upgrade())
        {
            controller.page_dirtied(inode);
        }
    }

    fn page_cleaned(&self, _page_idx: usize) {
        if let Some(controller) = self.controller.upgrade() {
            controller.page_cleaned();
        }
    }

    fn writeback_completion_dispatcher(&self) -> Option<Arc<dyn WritebackCompletionDispatcher>> {
        self.controller
            .upgrade()
            .map(|controller| controller.completion_dispatcher(self.ino))
    }
}

#[cfg(ktest)]
mod tests {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use ostd::prelude::ktest;

    use super::{PageCacheWritebackController, WritebackConfig};
    use crate::{
        prelude::{Arc, Errno, Vec},
        thread::{AsThread, Thread, kernel_thread::ThreadOptions},
        vm::page_cache::PageCacheWritebackObserver,
    };

    #[ktest]
    fn dirty_page_accounting_is_exact() {
        let controller = PageCacheWritebackController::new(WritebackConfig::from_pages(5, 10, 5));

        controller.account_page_dirtied();
        controller.account_page_dirtied();
        controller.account_page_dirtied();

        assert_eq!(controller.outstanding_pages(), 3);

        controller.page_cleaned();
        controller.page_cleaned();
        controller.page_cleaned();
        assert_eq!(controller.outstanding_pages(), 0);
    }

    #[ktest]
    fn high_watermark_requires_throttle_until_low_watermark() {
        let controller = PageCacheWritebackController::new(WritebackConfig::from_pages(2, 4, 2));
        for _ in 0..3 {
            controller.account_page_dirtied();
        }
        assert!(!controller.should_throttle());

        controller.account_page_dirtied();
        assert!(controller.should_throttle());

        controller.page_cleaned();
        assert!(controller.should_throttle());
        controller.page_cleaned();
        assert!(!controller.should_throttle());
    }

    #[ktest]
    fn writeback_failure_rejects_more_buffered_dirtying() {
        let controller = PageCacheWritebackController::new(WritebackConfig::from_pages(2, 4, 2));
        controller.failed.store(true, Ordering::Release);

        assert_eq!(
            controller.balance_dirty_pages().unwrap_err().error(),
            Errno::EIO
        );
    }

    #[ktest]
    fn completion_failure_wakes_all_throttled_writers() {
        let controller = Arc::new(PageCacheWritebackController::new(
            WritebackConfig::from_pages(2, 4, 2),
        ));
        for _ in 0..4 {
            controller.account_page_dirtied();
        }
        let finished = Arc::new(AtomicUsize::new(0));
        let mut waiters = Vec::new();
        for _ in 0..2 {
            let controller = controller.clone();
            let finished = finished.clone();
            waiters.push(
                ThreadOptions::new(move || {
                    assert_eq!(
                        controller.balance_dirty_pages().unwrap_err().error(),
                        Errno::EIO
                    );
                    finished.fetch_add(1, Ordering::AcqRel);
                })
                .spawn(),
            );
        }
        for _ in 0..32 {
            Thread::yield_now();
        }
        assert_eq!(finished.load(Ordering::Acquire), 0);

        controller.notify_completion_failure();
        for waiter in waiters {
            waiter.join();
        }
        assert_eq!(finished.load(Ordering::Acquire), 2);
        assert_eq!(
            controller
                .stats_snapshot_and_reset()
                .foreground_throttle_stalls,
            2
        );
    }

    #[ktest]
    fn stopping_controller_does_not_account_new_dirty_pages() {
        let controller = PageCacheWritebackController::new(WritebackConfig::from_pages(2, 4, 2));
        assert!(controller.stop_accepting());

        controller.account_page_dirtied();

        assert_eq!(controller.outstanding_pages(), 0);
        assert!(!controller.should_throttle());
        assert_eq!(
            controller.balance_dirty_pages().unwrap_err().error(),
            Errno::ESHUTDOWN
        );
    }

    #[ktest]
    fn submission_quiesce_gate_serializes_submitters() {
        let controller = Arc::new(PageCacheWritebackController::new(
            WritebackConfig::from_pages(2, 4, 2),
        ));
        let first_entered = Arc::new(AtomicBool::new(false));
        let release_first = Arc::new(AtomicBool::new(false));
        let second_entered = Arc::new(AtomicBool::new(false));

        let first = {
            let controller = controller.clone();
            let first_entered = first_entered.clone();
            let release_first = release_first.clone();
            ThreadOptions::new(move || {
                controller
                    .with_submissions_quiesced(|| {
                        first_entered.store(true, Ordering::Release);
                        while !release_first.load(Ordering::Acquire) {
                            Thread::yield_now();
                        }
                        Ok(())
                    })
                    .unwrap();
            })
            .spawn()
        };
        while !first_entered.load(Ordering::Acquire) {
            Thread::yield_now();
        }
        let second = {
            let controller = controller.clone();
            let second_entered = second_entered.clone();
            ThreadOptions::new(move || {
                controller
                    .with_submission_allowed(|| {
                        second_entered.store(true, Ordering::Release);
                        Ok(())
                    })
                    .unwrap();
            })
            .spawn()
        };
        for _ in 0..32 {
            Thread::yield_now();
        }
        assert!(!second_entered.load(Ordering::Acquire));

        release_first.store(true, Ordering::Release);
        first.join();
        second.join();
        assert!(second_entered.load(Ordering::Acquire));
    }

    #[ktest]
    fn submission_gate_allows_concurrent_submitters() {
        let controller = Arc::new(PageCacheWritebackController::new(
            WritebackConfig::from_pages(2, 4, 2),
        ));
        let first_entered = Arc::new(AtomicBool::new(false));
        let release_first = Arc::new(AtomicBool::new(false));
        let second_entered = Arc::new(AtomicBool::new(false));

        let first = {
            let controller = controller.clone();
            let first_entered = first_entered.clone();
            let release_first = release_first.clone();
            ThreadOptions::new(move || {
                controller
                    .with_submission_allowed(|| {
                        first_entered.store(true, Ordering::Release);
                        while !release_first.load(Ordering::Acquire) {
                            Thread::yield_now();
                        }
                        Ok(())
                    })
                    .unwrap();
            })
            .spawn()
        };
        while !first_entered.load(Ordering::Acquire) {
            Thread::yield_now();
        }
        let second = {
            let controller = controller.clone();
            let second_entered = second_entered.clone();
            ThreadOptions::new(move || {
                controller
                    .with_submission_allowed(|| {
                        second_entered.store(true, Ordering::Release);
                        Ok(())
                    })
                    .unwrap();
            })
            .spawn()
        };
        while !second_entered.load(Ordering::Acquire) {
            Thread::yield_now();
        }

        release_first.store(true, Ordering::Release);
        first.join();
        second.join();
    }

    #[ktest]
    fn pending_quiesce_blocks_new_submitters() {
        let controller = Arc::new(PageCacheWritebackController::new(
            WritebackConfig::from_pages(2, 4, 2),
        ));
        let first_entered = Arc::new(AtomicBool::new(false));
        let release_first = Arc::new(AtomicBool::new(false));
        let quiesce_started = Arc::new(AtomicBool::new(false));
        let quiesce_entered = Arc::new(AtomicBool::new(false));
        let second_entered = Arc::new(AtomicBool::new(false));

        let first = {
            let controller = controller.clone();
            let first_entered = first_entered.clone();
            let release_first = release_first.clone();
            ThreadOptions::new(move || {
                controller
                    .with_submission_allowed(|| {
                        first_entered.store(true, Ordering::Release);
                        while !release_first.load(Ordering::Acquire) {
                            Thread::yield_now();
                        }
                        Ok(())
                    })
                    .unwrap();
            })
            .spawn()
        };
        while !first_entered.load(Ordering::Acquire) {
            Thread::yield_now();
        }

        let quiesce = {
            let controller = controller.clone();
            let quiesce_started = quiesce_started.clone();
            let quiesce_entered = quiesce_entered.clone();
            ThreadOptions::new(move || {
                quiesce_started.store(true, Ordering::Release);
                controller
                    .with_submissions_quiesced(|| {
                        quiesce_entered.store(true, Ordering::Release);
                        Ok(())
                    })
                    .unwrap();
            })
            .spawn()
        };
        while !quiesce_started.load(Ordering::Acquire) {
            Thread::yield_now();
        }
        for _ in 0..32 {
            Thread::yield_now();
        }

        let second = {
            let controller = controller.clone();
            let second_entered = second_entered.clone();
            ThreadOptions::new(move || {
                controller
                    .with_submission_allowed(|| {
                        second_entered.store(true, Ordering::Release);
                        Ok(())
                    })
                    .unwrap();
            })
            .spawn()
        };
        for _ in 0..32 {
            Thread::yield_now();
        }
        assert!(!second_entered.load(Ordering::Acquire));
        assert!(!quiesce_entered.load(Ordering::Acquire));

        release_first.store(true, Ordering::Release);
        first.join();
        quiesce.join();
        second.join();
        assert!(quiesce_entered.load(Ordering::Acquire));
        assert!(second_entered.load(Ordering::Acquire));
    }

    #[ktest]
    fn queued_submitter_gets_a_turn_between_quiesces() {
        let controller = Arc::new(PageCacheWritebackController::new(
            WritebackConfig::from_pages(2, 4, 2),
        ));
        let first_entered = Arc::new(AtomicBool::new(false));
        let release_first = Arc::new(AtomicBool::new(false));
        let order = Arc::new(AtomicUsize::new(0));
        let first_quiesce_order = Arc::new(AtomicUsize::new(0));
        let submitter_order = Arc::new(AtomicUsize::new(0));
        let second_quiesce_order = Arc::new(AtomicUsize::new(0));

        let first = {
            let controller = controller.clone();
            let first_entered = first_entered.clone();
            let release_first = release_first.clone();
            ThreadOptions::new(move || {
                controller
                    .with_submission_allowed(|| {
                        first_entered.store(true, Ordering::Release);
                        while !release_first.load(Ordering::Acquire) {
                            Thread::yield_now();
                        }
                        Ok(())
                    })
                    .unwrap();
            })
            .spawn()
        };
        while !first_entered.load(Ordering::Acquire) {
            Thread::yield_now();
        }

        let first_quiesce = {
            let controller = controller.clone();
            let order = order.clone();
            let entered_order = first_quiesce_order.clone();
            ThreadOptions::new(move || {
                controller
                    .with_submissions_quiesced(|| {
                        entered_order
                            .store(order.fetch_add(1, Ordering::AcqRel) + 1, Ordering::Release);
                        Ok(())
                    })
                    .unwrap();
            })
            .spawn()
        };
        while controller.submission_gate.lock().waiting_quiesces != 1 {
            Thread::yield_now();
        }

        let submitter = {
            let controller = controller.clone();
            let order = order.clone();
            let entered_order = submitter_order.clone();
            ThreadOptions::new(move || {
                controller
                    .with_submission_allowed(|| {
                        entered_order
                            .store(order.fetch_add(1, Ordering::AcqRel) + 1, Ordering::Release);
                        Ok(())
                    })
                    .unwrap();
            })
            .spawn()
        };
        while controller.submission_gate.lock().waiting_submitters != 1 {
            Thread::yield_now();
        }

        let second_quiesce = {
            let controller = controller.clone();
            let order = order.clone();
            let entered_order = second_quiesce_order.clone();
            ThreadOptions::new(move || {
                controller
                    .with_submissions_quiesced(|| {
                        entered_order
                            .store(order.fetch_add(1, Ordering::AcqRel) + 1, Ordering::Release);
                        Ok(())
                    })
                    .unwrap();
            })
            .spawn()
        };

        release_first.store(true, Ordering::Release);
        first.join();
        first_quiesce.join();
        submitter.join();
        second_quiesce.join();

        assert_eq!(submitter_order.load(Ordering::Acquire), 2);
        let first_order = first_quiesce_order.load(Ordering::Acquire);
        let second_order = second_quiesce_order.load(Ordering::Acquire);
        assert_eq!(first_order + second_order, 4);
        assert_eq!(first_order * second_order, 3);
    }

    #[ktest]
    fn ext4_writeback_observer_exposes_mount_completion_dispatcher() {
        let controller = Arc::new(PageCacheWritebackController::new(
            WritebackConfig::from_pages(2, 4, 2),
        ));
        let observer = super::Ext4PageWritebackObserver {
            controller: Arc::downgrade(&controller),
            inode: crate::prelude::SpinLock::new(crate::prelude::Weak::new()),
            ino: 17,
        };

        let _dispatcher = controller.completion_dispatcher(17);
        assert!(observer.writeback_completion_dispatcher().is_some());
    }

    #[ktest]
    fn balance_blocks_at_high_watermark_and_resumes_at_low() {
        let controller = Arc::new(PageCacheWritebackController::new(
            WritebackConfig::from_pages(2, 4, 2),
        ));
        for _ in 0..4 {
            controller.account_page_dirtied();
        }

        let entered = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let waiter = {
            let controller = controller.clone();
            let entered = entered.clone();
            let finished = finished.clone();
            ThreadOptions::new(move || {
                entered.store(true, Ordering::Release);
                controller.balance_dirty_pages().unwrap();
                finished.store(true, Ordering::Release);
            })
            .spawn()
        };
        while !entered.load(Ordering::Acquire) {
            Thread::yield_now();
        }
        for _ in 0..32 {
            Thread::yield_now();
        }
        assert!(!finished.load(Ordering::Acquire));

        controller.page_cleaned();
        assert!(!finished.load(Ordering::Acquire));
        controller.page_cleaned();
        waiter.join();
        assert!(finished.load(Ordering::Acquire));
    }

    #[ktest]
    fn stop_does_not_join_the_current_worker() {
        let controller = Arc::new(PageCacheWritebackController::new(
            WritebackConfig::from_pages(2, 4, 2),
        ));
        let task = {
            let controller = controller.clone();
            ThreadOptions::new(move || controller.stop()).build()
        };
        let worker = task.as_thread().unwrap().clone();
        controller.worker.lock().replace(worker.clone());

        worker.run();
        worker.join();

        assert!(controller.stopping.load(Ordering::Acquire));
        assert!(controller.worker.lock().is_none());
    }
}
