// SPDX-License-Identifier: MPL-2.0

//! Regression tests for the page-cache subsystem in the
//! concurrency scenarios.

use alloc::vec;

use ostd::{
    mm::{VmIo, io::util::HasVmReaderWriter},
    prelude::ktest,
};

use self::utils::{
    IoCompletion, IoKind, MockDirectPageCacheBackend, MockPageCacheBackend, wait_until,
};
use super::{
    PageCache, PageCacheBackend, Vmo, VmoCommitError, VmoMapMode, WritebackBatchResult,
    WritebackMode,
};
use crate::{
    prelude::*,
    thread::{Thread, kernel_thread::ThreadOptions},
    vm::page_cache::vmo::PageCacheMark,
};

mod utils;

/// Creates a page cache with `num_pages` backend pages for test scenarios.
fn new_backend_page_cache(backend: &Arc<MockPageCacheBackend>, num_pages: usize) -> PageCache {
    let backend_dyn: Arc<dyn PageCacheBackend> = backend.clone();
    PageCache::new_with_backend(num_pages * PAGE_SIZE, Arc::downgrade(&backend_dyn)).unwrap()
}

/// Creates a locked page cache that models the inode-level lock held by
/// filesystem callers before touching the page cache.
fn new_locked_backend_page_cache(
    backend: &Arc<MockPageCacheBackend>,
    num_pages: usize,
) -> Arc<RwMutex<PageCache>> {
    Arc::new(RwMutex::new(new_backend_page_cache(backend, num_pages)))
}

/// Simulates stores through a shared writable VMO mapping.
///
/// A store page fault first commits page 0 for shared writes, which marks an
/// up-to-date backend page dirty. The actual byte copy then writes the
/// committed frame directly, matching mapped-memory stores without routing
/// through the page-cache write API.
fn write_vmo_bytes(vmo: &Vmo, bytes: &[u8]) {
    assert!(!bytes.is_empty());
    assert!(bytes.len() <= PAGE_SIZE);

    vmo.commit_on(0, VmoMapMode::SharedWrite).unwrap();
    let (page, _) = vmo.try_commit_page(0, VmoMapMode::SharedWrite).unwrap();

    let mut reader = VmReader::from(bytes);
    let mut writer = page.writer();
    assert_eq!(writer.write(&mut reader), bytes.len());
}

/// Dirty-page discovery merges adjacent pages while preserving holes and the
/// caller's byte-range boundary. Filesystems use this snapshot to drive
/// delayed allocation before writeback.
#[ktest]
fn dirty_ranges_merge_adjacent_pages_and_preserve_holes() {
    let backend = MockPageCacheBackend::new(5);
    let page_cache = new_backend_page_cache(&backend, 5);

    page_cache.write_bytes(0, &[0x11]).unwrap();
    page_cache.write_bytes(PAGE_SIZE, &[0x22]).unwrap();
    page_cache.write_bytes(3 * PAGE_SIZE, &[0x33]).unwrap();

    assert_eq!(
        page_cache.dirty_ranges(PAGE_SIZE / 2..4 * PAGE_SIZE),
        vec![PAGE_SIZE / 2..2 * PAGE_SIZE, 3 * PAGE_SIZE..4 * PAGE_SIZE]
    );
}

/// A large flush must complete bounded windows instead of snapshotting the
/// entire dirty set before waiting for any I/O completion.
#[ktest]
fn flush_range_limits_each_backend_batch() {
    const BATCH_PAGES: usize = 1024;
    const NUM_PAGES: usize = BATCH_PAGES + 1;

    let backend = MockPageCacheBackend::new(NUM_PAGES);
    let page_cache = new_backend_page_cache(&backend, NUM_PAGES);
    page_cache
        .write_bytes(0, &vec![0x5a; NUM_PAGES * PAGE_SIZE])
        .unwrap();

    page_cache.flush_range(0..NUM_PAGES * PAGE_SIZE).unwrap();

    assert_eq!(backend.write_batch_sizes(), vec![BATCH_PAGES, 1]);
    assert_eq!(backend.write_count(0), 1);
    assert_eq!(backend.write_count(NUM_PAGES - 1), 1);
}

#[ktest]
fn flush_range_batch_rejects_zero_page_limit() {
    let backend = MockPageCacheBackend::new(1);
    let page_cache = new_backend_page_cache(&backend, 1);

    assert_eq!(
        page_cache
            .flush_range_batch(0..PAGE_SIZE, 0)
            .unwrap_err()
            .error(),
        Errno::EINVAL
    );
}

#[ktest]
fn direct_backend_requires_explicit_detached_completion_opt_in() {
    let backend = Arc::new(MockDirectPageCacheBackend::new());
    let backend_dyn: Arc<dyn PageCacheBackend> = backend.clone();
    let page_cache = Arc::new(RwMutex::new(
        PageCache::new_with_backend(PAGE_SIZE, Arc::downgrade(&backend_dyn)).unwrap(),
    ));
    page_cache.write().write_bytes(0, &[0x11]).unwrap();
    let result = Arc::new(Mutex::new(None::<Result<()>>));
    let thread = {
        let page_cache = page_cache.clone();
        let result = result.clone();
        ThreadOptions::new(move || {
            *result.lock() = Some(page_cache.read().flush_range(0..PAGE_SIZE));
        })
        .spawn()
    };

    assert!(backend.writeback_observer().is_some());
    assert!(backend.writeback_completion_dispatcher().is_none());
    backend.wait_for_write_submission();
    assert!(result.lock().is_none());
    backend.complete_write();
    thread.join();
    assert!(result.lock().take().unwrap().is_ok());
}

/// A detached completion must not erase a write that raced after its snapshot.
#[ktest]
fn detached_completion_preserves_concurrent_redirty() {
    let (backend, dispatcher) = MockPageCacheBackend::new_with_detached_writeback(1);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);
    let page_cache = new_backend_page_cache(&backend, 1);
    let vmo = page_cache.as_vmo().clone();
    page_cache.write_bytes(0, &[0x11]).unwrap();

    page_cache
        .flush_range_batch_with_mode(0..PAGE_SIZE, 1, WritebackMode::Background)
        .unwrap();
    backend.wait_for_deferred_bios(IoKind::Write, 1);
    write_vmo_bytes(&vmo, &[0x22]);

    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    assert!(dispatcher.finish_next().unwrap().is_ok());
    assert_eq!(page_cache.dirty_ranges(0..PAGE_SIZE), vec![0..PAGE_SIZE]);
}

/// A failed detached completion must restore Dirty after ending Writeback.
#[ktest]
fn detached_completion_failure_redirties_and_returns_io_error() {
    let (backend, dispatcher) = MockPageCacheBackend::new_with_detached_writeback(1);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);
    let page_cache = new_backend_page_cache(&backend, 1);
    page_cache.write_bytes(0, &[0x11]).unwrap();

    page_cache
        .flush_range_batch_with_mode(0..PAGE_SIZE, 1, WritebackMode::Background)
        .unwrap();
    backend.wait_for_deferred_bios(IoKind::Write, 1);

    assert!(backend.complete_next_deferred_bio(IoKind::Write, false));
    assert_eq!(
        dispatcher.finish_next().unwrap().unwrap_err().error(),
        Errno::EIO
    );
    assert_eq!(page_cache.dirty_ranges(0..PAGE_SIZE), vec![0..PAGE_SIZE]);
}

/// A consumed prepared completion cannot be finished a second time.
#[ktest]
fn detached_completion_is_finished_exactly_once() {
    let (backend, dispatcher) = MockPageCacheBackend::new_with_detached_writeback(1);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);
    let page_cache = new_backend_page_cache(&backend, 1);
    page_cache.write_bytes(0, &[0x11]).unwrap();

    page_cache
        .flush_range_batch_with_mode(0..PAGE_SIZE, 1, WritebackMode::Background)
        .unwrap();
    backend.wait_for_deferred_bios(IoKind::Write, 1);
    assert_eq!(dispatcher.prepared_count(), 1);
    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    assert!(dispatcher.finish_next().unwrap().is_ok());
    assert!(dispatcher.finish_next().is_none());
}

/// Backends without a dispatcher continue to wait through `IoBatch::wait_all`.
#[ktest]
fn default_writeback_backend_waits_for_io_batch() {
    let backend = MockPageCacheBackend::new(1);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);
    let page_cache = new_locked_backend_page_cache(&backend, 1);
    page_cache.write().write_bytes(0, &[0x11]).unwrap();
    let result = Arc::new(Mutex::new(None::<Result<()>>));
    let thread = {
        let page_cache = page_cache.clone();
        let result = result.clone();
        ThreadOptions::new(move || {
            *result.lock() = Some(page_cache.read().flush_range(0..PAGE_SIZE));
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Write, 1);
    assert!(result.lock().is_none());
    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    thread.join();
    assert!(result.lock().take().unwrap().is_ok());
}

#[ktest]
fn sync_detached_writeback_waits_through_its_fence() {
    let (backend, dispatcher) = MockPageCacheBackend::new_with_detached_writeback(1);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);
    let page_cache = new_locked_backend_page_cache(&backend, 1);
    page_cache.write().write_bytes(0, &[0x11]).unwrap();
    let result = Arc::new(Mutex::new(None::<Result<()>>));
    let thread = {
        let page_cache = page_cache.clone();
        let result = result.clone();
        ThreadOptions::new(move || {
            *result.lock() = Some(
                page_cache
                    .read()
                    .flush_range_batch_with_mode(0..PAGE_SIZE, 1, WritebackMode::Sync)
                    .and_then(WritebackBatchResult::wait),
            );
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Write, 1);
    assert_eq!(dispatcher.wait_count(), 1);
    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    thread.join();
    assert!(result.lock().take().unwrap().is_ok());
    assert_eq!(dispatcher.wait_count(), 2);
}

#[ktest]
fn sync_detached_writeback_waits_for_completion_credit() {
    let (backend, dispatcher) = MockPageCacheBackend::new_with_detached_writeback(2);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);
    dispatcher.set_max_prepared(1);
    let page_cache = Arc::new(new_backend_page_cache(&backend, 2));
    page_cache
        .write_bytes(0, &vec![0x11; 2 * PAGE_SIZE])
        .unwrap();

    page_cache
        .flush_range_batch_with_mode(0..PAGE_SIZE, 1, WritebackMode::Background)
        .unwrap();
    backend.wait_for_deferred_bios(IoKind::Write, 1);

    let result = Arc::new(Mutex::new(None::<Result<()>>));
    let sync_thread = {
        let page_cache = page_cache.clone();
        let result = result.clone();
        ThreadOptions::new(move || {
            *result.lock() = Some(page_cache.flush_range(PAGE_SIZE..2 * PAGE_SIZE));
        })
        .spawn()
    };

    while dispatcher.wait_count() == 0 && result.lock().is_none() {
        Thread::yield_now();
    }
    assert!(dispatcher.wait_count() > 0);
    assert!(result.lock().is_none());
    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    backend.wait_for_deferred_bios(IoKind::Write, 1);
    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    sync_thread.join();

    assert!(result.lock().take().unwrap().is_ok());
    assert_eq!(backend.write_count(0), 1);
    assert_eq!(backend.write_count(1), 1);
}

#[ktest]
fn sync_observes_failure_from_an_already_inflight_page() {
    let (backend, dispatcher) = MockPageCacheBackend::new_with_detached_writeback(1);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);
    let page_cache = Arc::new(new_backend_page_cache(&backend, 1));
    page_cache.write_bytes(0, &[0x11]).unwrap();

    page_cache
        .flush_range_batch_with_mode(0..PAGE_SIZE, 1, WritebackMode::Background)
        .unwrap();
    backend.wait_for_deferred_bios(IoKind::Write, 1);
    let result = Arc::new(Mutex::new(None::<Result<WritebackBatchResult>>));
    let sync_thread = {
        let page_cache = page_cache.clone();
        let result = result.clone();
        ThreadOptions::new(move || {
            *result.lock() =
                Some(page_cache.flush_range_batch_with_mode(0..PAGE_SIZE, 1, WritebackMode::Sync));
        })
        .spawn()
    };
    assert!(backend.complete_next_deferred_bio(IoKind::Write, false));
    sync_thread.join();

    let error = match result.lock().take().unwrap() {
        Ok(_) => panic!("sync unexpectedly ignored the in-flight writeback failure"),
        Err(error) => error,
    };
    assert_eq!(error.error(), Errno::EIO);
    assert_eq!(dispatcher.wait_count(), 2);
    assert_eq!(page_cache.dirty_ranges(0..PAGE_SIZE), vec![0..PAGE_SIZE]);
}

#[ktest]
fn sync_closes_writeback_sequence_registration_window() {
    let (backend, dispatcher) = MockPageCacheBackend::new_with_detached_writeback(1);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);
    dispatcher.pause_prepare();
    let page_cache = Arc::new(new_backend_page_cache(&backend, 1));
    page_cache.write_bytes(0, &[0x11]).unwrap();

    let background = {
        let page_cache = page_cache.clone();
        ThreadOptions::new(move || {
            page_cache
                .flush_range_batch_with_mode(0..PAGE_SIZE, 1, WritebackMode::Background)
                .unwrap();
        })
        .spawn()
    };
    dispatcher.wait_until_prepare_entered();

    let result = Arc::new(Mutex::new(None::<Result<WritebackBatchResult>>));
    let sync_thread = {
        let page_cache = page_cache.clone();
        let result = result.clone();
        ThreadOptions::new(move || {
            *result.lock() =
                Some(page_cache.flush_range_batch_with_mode(0..PAGE_SIZE, 1, WritebackMode::Sync));
        })
        .spawn()
    };
    for _ in 0..32 {
        Thread::yield_now();
    }
    assert!(result.lock().is_none());

    dispatcher.resume_prepare();
    background.join();
    backend.wait_for_deferred_bios(IoKind::Write, 1);
    assert!(backend.complete_next_deferred_bio(IoKind::Write, false));
    assert!(dispatcher.finish_next().unwrap().is_err());
    sync_thread.join();

    let error = match result.lock().take().unwrap() {
        Ok(_) => panic!("sync unexpectedly ignored the registration-window failure"),
        Err(error) => error,
    };
    assert_eq!(error.error(), Errno::EIO);
    assert_eq!(page_cache.dirty_ranges(0..PAGE_SIZE), vec![0..PAGE_SIZE]);
}

#[ktest]
fn detached_batch_fence_excludes_later_submission() {
    let (backend, dispatcher) = MockPageCacheBackend::new_with_detached_writeback(2);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);
    let page_cache = new_backend_page_cache(&backend, 2);

    page_cache.write_bytes(0, &[0x11]).unwrap();
    let first_batch = page_cache
        .flush_range_batch_with_mode(0..PAGE_SIZE, 1, WritebackMode::Background)
        .unwrap();
    page_cache.write_bytes(PAGE_SIZE, &[0x22]).unwrap();
    let _second_batch = page_cache
        .flush_range_batch_with_mode(PAGE_SIZE..2 * PAGE_SIZE, 1, WritebackMode::Background)
        .unwrap();
    backend.wait_for_deferred_bios(IoKind::Write, 2);

    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    first_batch.wait().unwrap();

    assert_eq!(dispatcher.prepared_count(), 1);
    assert_eq!(dispatcher.wait_count(), 1);
    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    assert!(dispatcher.finish_next().unwrap().is_ok());
}

#[ktest]
fn detached_prepare_failure_finishes_writeback_and_redirties_page() {
    let (backend, dispatcher) = MockPageCacheBackend::new_with_detached_writeback(1);
    dispatcher.set_fail_prepare(true);
    let page_cache = new_backend_page_cache(&backend, 1);
    page_cache.write_bytes(0, &[0x11]).unwrap();

    let err =
        match page_cache.flush_range_batch_with_mode(0..PAGE_SIZE, 1, WritebackMode::Background) {
            Ok(_) => panic!("prepare failure unexpectedly produced a batch result"),
            Err(err) => err,
        };
    assert_eq!(err.error(), Errno::EIO);
    assert_eq!(page_cache.dirty_ranges(0..PAGE_SIZE), vec![0..PAGE_SIZE]);
}

#[ktest]
fn detached_submit_failure_finishes_prepared_completion() {
    let (backend, dispatcher) = MockPageCacheBackend::new_with_detached_writeback(1);
    backend.set_write_submit_failure(0, true);
    let page_cache = new_backend_page_cache(&backend, 1);
    page_cache.write_bytes(0, &[0x11]).unwrap();

    let err =
        match page_cache.flush_range_batch_with_mode(0..PAGE_SIZE, 1, WritebackMode::Background) {
            Ok(_) => panic!("submit failure unexpectedly produced a batch result"),
            Err(err) => err,
        };
    assert_eq!(err.error(), Errno::EIO);
    assert_eq!(dispatcher.prepared_count(), 0);
    assert_eq!(page_cache.dirty_ranges(0..PAGE_SIZE), vec![0..PAGE_SIZE]);
}

#[ktest]
fn background_detached_writeback_skips_existing_writeback_pages() {
    let (backend, dispatcher) = MockPageCacheBackend::new_with_detached_writeback(1);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);
    let page_cache = new_backend_page_cache(&backend, 1);
    let vmo = page_cache.as_vmo().clone();
    page_cache.write_bytes(0, &[0x11]).unwrap();

    page_cache
        .flush_range_batch_with_mode(0..PAGE_SIZE, 1, WritebackMode::Background)
        .unwrap();
    backend.wait_for_deferred_bios(IoKind::Write, 1);
    write_vmo_bytes(&vmo, &[0x22]);
    let second_batch = page_cache
        .flush_range_batch_with_mode(0..PAGE_SIZE, 1, WritebackMode::Background)
        .unwrap();

    assert_eq!(backend.write_count(0), 1);
    assert_eq!(dispatcher.prepared_count(), 1);
    assert!(!second_batch.had_work());
    assert!(second_batch.retry_after_completion());

    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    assert!(dispatcher.finish_next().unwrap().is_ok());
    page_cache
        .flush_range_batch_with_mode(0..PAGE_SIZE, 1, WritebackMode::Background)
        .unwrap();
    assert_eq!(backend.write_count(0), 2);
    backend.wait_for_deferred_bios(IoKind::Write, 1);
    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    assert!(dispatcher.finish_next().unwrap().is_ok());
}

#[ktest]
fn later_submit_failure_does_not_finish_an_earlier_prepared_completion() {
    let (backend, dispatcher) = MockPageCacheBackend::new_with_detached_writeback(2);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);
    backend.set_write_submit_failure(1, true);
    let page_cache = new_backend_page_cache(&backend, 2);
    page_cache
        .write_bytes(0, &vec![0x11; 2 * PAGE_SIZE])
        .unwrap();

    let err = match page_cache.flush_range_batch_with_mode(
        0..2 * PAGE_SIZE,
        2,
        WritebackMode::Background,
    ) {
        Ok(_) => panic!("later submit failure unexpectedly produced a batch result"),
        Err(err) => err,
    };
    assert_eq!(err.error(), Errno::EIO);
    assert_eq!(dispatcher.prepared_count(), 1);
    assert_eq!(backend.write_count(0), 1);
    assert_eq!(backend.write_count(1), 1);

    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    assert!(dispatcher.finish_next().unwrap().is_ok());
    assert_eq!(
        page_cache.dirty_ranges(0..2 * PAGE_SIZE),
        vec![PAGE_SIZE..2 * PAGE_SIZE]
    );
}

/// Dirty accounting follows page generations rather than write calls.
#[ktest]
fn writeback_observer_counts_dirty_generations() {
    let backend = MockPageCacheBackend::new(1);
    let page_cache = new_locked_backend_page_cache(&backend, 1);

    page_cache.write().write_bytes(0, &[0x11]).unwrap();
    page_cache.write().write_bytes(1, &[0x22]).unwrap();
    assert_eq!(backend.outstanding_pages(), 1);

    page_cache.read().flush_range(0..PAGE_SIZE).unwrap();
    assert_eq!(backend.outstanding_pages(), 0);
}

#[ktest]
fn pre_submission_failure_preserves_one_outstanding_page() {
    let backend = MockPageCacheBackend::new(1);
    let page_cache = new_backend_page_cache(&backend, 1);
    page_cache.write_bytes(0, &[0x11]).unwrap();
    assert_eq!(backend.outstanding_pages(), 1);

    backend.set_fail_write_batch_before_submission(true);
    assert_eq!(
        page_cache.flush_range(0..PAGE_SIZE).unwrap_err().error(),
        Errno::EIO
    );
    assert_eq!(backend.outstanding_pages(), 1);

    backend.set_fail_write_batch_before_submission(false);
    page_cache.flush_range(0..PAGE_SIZE).unwrap();
    assert_eq!(backend.outstanding_pages(), 0);
}

#[ktest]
fn partial_batch_submission_failure_preserves_per_page_accounting() {
    let backend = MockPageCacheBackend::new(2);
    let page_cache = new_backend_page_cache(&backend, 2);
    page_cache
        .write_bytes(0, &vec![0x11; 2 * PAGE_SIZE])
        .unwrap();
    assert_eq!(backend.outstanding_pages(), 2);

    backend.set_write_submit_failure(1, true);
    assert_eq!(
        page_cache
            .flush_range(0..2 * PAGE_SIZE)
            .unwrap_err()
            .error(),
        Errno::EIO
    );
    assert_eq!(backend.outstanding_pages(), 1);

    backend.set_write_submit_failure(1, false);
    page_cache.flush_range(0..2 * PAGE_SIZE).unwrap();
    assert_eq!(backend.outstanding_pages(), 0);
}

/// Truncating a dirty page discards its pending generation and must release
/// the owner's dirty-page budget.
#[ktest]
fn truncate_releases_dirty_generation_accounting() {
    let backend = MockPageCacheBackend::new(2);
    let mut page_cache = new_backend_page_cache(&backend, 2);
    page_cache.write_bytes(PAGE_SIZE, &[0x33]).unwrap();
    assert_eq!(backend.outstanding_pages(), 1);

    page_cache.resize(PAGE_SIZE, 2 * PAGE_SIZE).unwrap();
    assert_eq!(backend.outstanding_pages(), 0);
}

/// Dirty and writeback marks are independent secondary indexes, and marked
/// range lookup must not visit candidates outside the requested range.
#[ktest]
fn page_state_marks_are_independent_and_range_bounded() {
    let backend = MockPageCacheBackend::new(8);
    let page_cache = new_backend_page_cache(&backend, 8);
    let vmo = page_cache.as_vmo();
    let backed_vmo = vmo.as_backed_vmo().unwrap();

    let mut byte = [0u8; 1];
    for index in [1, 3, 6] {
        page_cache.read_bytes(index * PAGE_SIZE, &mut byte).unwrap();
    }

    let (page1, _) = vmo
        .try_commit_page(PAGE_SIZE, VmoMapMode::SharedRead)
        .unwrap();
    let (page3, _) = vmo
        .try_commit_page(3 * PAGE_SIZE, VmoMapMode::SharedRead)
        .unwrap();
    let (page6, _) = vmo
        .try_commit_page(6 * PAGE_SIZE, VmoMapMode::SharedRead)
        .unwrap();

    assert!(backed_vmo.set_page_mark_if_same(1, &page1, PageCacheMark::Dirty));
    assert!(backed_vmo.set_page_mark_if_same(3, &page3, PageCacheMark::Writeback));
    assert!(backed_vmo.set_page_mark_if_same(6, &page6, PageCacheMark::Dirty));

    let dirty_indices: Vec<_> = backed_vmo
        .collect_marked_pages(&(2..7), PageCacheMark::Dirty)
        .into_iter()
        .map(|(index, _)| index)
        .collect();
    let writeback_indices: Vec<_> = backed_vmo
        .collect_marked_pages(&(0..8), PageCacheMark::Writeback)
        .into_iter()
        .map(|(index, _)| index)
        .collect();

    assert_eq!(dirty_indices, vec![6]);
    assert_eq!(writeback_indices, vec![3]);
}

/// Every backend-page dirtying path must publish the Dirty XArray mark, while
/// read-only page commits must not create false dirty candidates.
#[ktest]
fn buffered_and_mapped_writes_publish_dirty_marks() {
    let backend = MockPageCacheBackend::new(3);
    let page_cache = new_backend_page_cache(&backend, 3);
    let vmo = page_cache.as_vmo();

    page_cache.write_bytes(0, &[0x31]).unwrap();

    let mut clean_byte = [0u8; 1];
    page_cache.read_bytes(PAGE_SIZE, &mut clean_byte).unwrap();

    vmo.commit_on(2, VmoMapMode::SharedWrite).unwrap();
    let (mapped_page, _) = vmo
        .try_commit_page(2 * PAGE_SIZE, VmoMapMode::SharedWrite)
        .unwrap();
    let mut mapped_reader = VmReader::from(&[0x72u8][..]);
    assert_eq!(mapped_page.writer().write(&mut mapped_reader), 1);

    let dirty_indices: Vec<_> = vmo
        .as_backed_vmo()
        .unwrap()
        .collect_marked_pages(&(0..3), PageCacheMark::Dirty)
        .into_iter()
        .map(|(index, _)| index)
        .collect();

    assert_eq!(dirty_indices, vec![0, 2]);
}

/// A page moves from the Dirty index to the Writeback index while its BIO is
/// in flight, then leaves both indexes after successful completion.
#[ktest]
fn writeback_marks_follow_inflight_io() {
    let backend = MockPageCacheBackend::new(1);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);

    let page_cache = new_locked_backend_page_cache(&backend, 1);
    let vmo = page_cache.read().as_vmo().clone();
    page_cache.write().write_bytes(0, &[0x5a]).unwrap();

    let flush_result = Arc::new(Mutex::new(None::<Result<()>>));
    let flush_thread = {
        let page_cache = page_cache.clone();
        let flush_result = flush_result.clone();
        ThreadOptions::new(move || {
            *flush_result.lock() = Some(page_cache.read().flush_range(0..PAGE_SIZE));
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Write, 1);
    let backed_vmo = vmo.as_backed_vmo().unwrap();
    assert!(
        backed_vmo
            .collect_marked_pages(&(0..1), PageCacheMark::Dirty)
            .is_empty()
    );
    assert_eq!(
        backed_vmo
            .collect_marked_pages(&(0..1), PageCacheMark::Writeback)
            .len(),
        1
    );

    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    flush_thread.join();
    assert!(flush_result.lock().take().unwrap().is_ok());
    assert!(
        backed_vmo
            .collect_marked_pages(&(0..1), PageCacheMark::Dirty)
            .is_empty()
    );
    assert!(
        backed_vmo
            .collect_marked_pages(&(0..1), PageCacheMark::Writeback)
            .is_empty()
    );
}

/// Serializes a cold read and a later overwrite with the caller-provided
/// buffered-I/O lock required by the page-cache synchronization model.
#[ktest]
fn concurrent_read_and_write() {
    let backend = MockPageCacheBackend::new(1);
    backend.set_completion(IoKind::Read, IoCompletion::Deferred);

    let old_pattern = vec![0x3c; PAGE_SIZE];
    let new_pattern = vec![0xa5; PAGE_SIZE];
    backend.set_persisted_page_bytes(0, &old_pattern);

    let page_cache = Arc::new(Mutex::new(new_backend_page_cache(&backend, 1)));
    let observed_read_result = Arc::new(Mutex::new(None::<Vec<u8>>));
    let writer_started = Arc::new(Mutex::new(false));
    let writer_finished = Arc::new(Mutex::new(false));

    // Start a cold-page read and hold backend completion so the read keeps the
    // caller-provided buffered-I/O lock while the writer attempts to enter.
    let read_thread = {
        let page_cache = page_cache.clone();
        let observed_read_result = observed_read_result.clone();
        ThreadOptions::new(move || {
            let page_cache = page_cache.lock();
            let mut read_buffer = vec![0; PAGE_SIZE];
            page_cache.read_bytes(0, &mut read_buffer).unwrap();
            *observed_read_result.lock() = Some(read_buffer);
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Read, 1);
    assert_eq!(backend.read_count(0), 1);

    // Issue a full-page overwrite against the same range. The page-cache layer
    // relies on this higher-level lock for buffered read/write ordering, so the
    // writer must not enter `write_bytes()` until the read-side critical section
    // has completed.
    let write_thread = {
        let page_cache = page_cache.clone();
        let writer_started = writer_started.clone();
        let writer_finished = writer_finished.clone();
        let new_pattern = new_pattern.clone();
        ThreadOptions::new(move || {
            *writer_started.lock() = true;
            let page_cache = page_cache.lock();
            page_cache.write_bytes(0, &new_pattern).unwrap();
            *writer_finished.lock() = true;
        })
        .spawn()
    };

    wait_until(|| *writer_started.lock());
    assert!(!*writer_finished.lock());

    // Finish the backend read. The serialized reader observes the persisted old
    // bytes, and the later writer's overwrite becomes visible afterwards.
    assert!(backend.complete_next_deferred_bio(IoKind::Read, true));
    read_thread.join();
    write_thread.join();

    assert_eq!(&*observed_read_result.lock(), &Some(old_pattern));
    assert!(*writer_finished.lock());
    assert_eq!(backend.read_count(0), 1);

    let mut read_buffer = vec![0; PAGE_SIZE];
    page_cache.lock().read_bytes(0, &mut read_buffer).unwrap();
    assert_eq!(read_buffer, new_pattern);
}

/// Flushes a dirty page while another task re-dirties it, so writeback reaches
/// the backend and the newest dirty bytes are not silently lost.
#[ktest]
fn concurrent_write_and_flush() {
    let backend = MockPageCacheBackend::new(1);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);

    let page_cache = new_locked_backend_page_cache(&backend, 1);
    let vmo = page_cache.read().as_vmo().clone();
    let first_dirty_pattern = vec![0x11; PAGE_SIZE];
    let latest_dirty_pattern = vec![0x22; PAGE_SIZE];
    page_cache
        .write()
        .write_bytes(0, &first_dirty_pattern)
        .unwrap();
    assert_eq!(backend.outstanding_pages(), 1);

    let flush_result = Arc::new(Mutex::new(None::<Result<()>>));
    let writer_finished = Arc::new(Mutex::new(false));

    // Start writeback and pin it in the deferred state so a concurrent writer
    // can dirty the page again before the first flush completes.
    let flush_thread = {
        let page_cache = page_cache.clone();
        let flush_result = flush_result.clone();
        ThreadOptions::new(move || {
            *flush_result.lock() = Some(page_cache.read().flush_range(0..PAGE_SIZE));
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Write, 1);

    // Re-dirty the same page while the first writeback is in flight. A later
    // flush must persist this newest version instead of silently dropping it.
    let writer_thread = {
        let vmo = vmo.clone();
        let writer_finished = writer_finished.clone();
        let latest_dirty_pattern = latest_dirty_pattern.clone();
        ThreadOptions::new(move || {
            write_vmo_bytes(&vmo, &latest_dirty_pattern);
            *writer_finished.lock() = true;
        })
        .spawn()
    };

    writer_thread.join();
    assert!(*writer_finished.lock());
    assert_eq!(backend.outstanding_pages(), 1);

    // Complete the first writeback, then flush again and confirm the backend
    // eventually stores the latest dirty bytes.
    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    flush_thread.join();
    assert!(flush_result.lock().take().unwrap().is_ok());
    assert_eq!(backend.outstanding_pages(), 1);

    backend.set_completion(IoKind::Write, IoCompletion::Immediate);
    page_cache.read().flush_range(0..PAGE_SIZE).unwrap();

    let mut read_buffer = vec![0; PAGE_SIZE];
    page_cache.read().read_bytes(0, &mut read_buffer).unwrap();
    assert_eq!(read_buffer, latest_dirty_pattern);
    assert_eq!(backend.write_count(0), 2);
    assert_eq!(backend.persisted_page_bytes(0), latest_dirty_pattern);
    assert_eq!(backend.outstanding_pages(), 0);
}

/// A truncate that removes a page while failed writeback is in flight must
/// release that page's outstanding budget after the failure redirties it.
#[ktest]
fn truncate_discards_failed_inflight_writeback_accounting() {
    let backend = MockPageCacheBackend::new(1);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);

    let page_cache = new_locked_backend_page_cache(&backend, 1);
    let vmo = page_cache.read().as_vmo().clone();
    page_cache.write().write_bytes(0, &[0x5a]).unwrap();
    assert_eq!(backend.outstanding_pages(), 1);

    let flush_result = Arc::new(Mutex::new(None::<Result<()>>));
    let flush_thread = {
        let vmo = vmo.clone();
        let flush_result = flush_result.clone();
        ThreadOptions::new(move || {
            *flush_result.lock() = Some(
                vmo.as_backed_vmo()
                    .unwrap()
                    .flush_dirty_pages(&(0..PAGE_SIZE)),
            );
        })
        .spawn()
    };
    backend.wait_for_deferred_bios(IoKind::Write, 1);

    let resize_result = Arc::new(Mutex::new(None::<Result<()>>));
    let resize_thread = {
        let page_cache = page_cache.clone();
        let resize_result = resize_result.clone();
        ThreadOptions::new(move || {
            *resize_result.lock() = Some(page_cache.write().resize(0, PAGE_SIZE));
        })
        .spawn()
    };

    assert!(backend.complete_next_deferred_bio(IoKind::Write, false));
    flush_thread.join();
    resize_thread.join();

    assert!(flush_result.lock().take().unwrap().is_err());
    assert!(resize_result.lock().take().unwrap().is_ok());
    assert_eq!(backend.outstanding_pages(), 0);
}

/// Re-dirties a page while another task runs `flush_range()` and
/// `evict_range()`, ensuring the newest dirty page is kept cached.
#[ktest]
fn concurrent_write_and_evict() {
    let backend = MockPageCacheBackend::new(1);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);

    let page_cache = new_locked_backend_page_cache(&backend, 1);
    let vmo = page_cache.read().as_vmo().clone();
    let first_dirty_pattern = vec![0x52; PAGE_SIZE];
    let latest_dirty_pattern = vec![0x7d; PAGE_SIZE];
    page_cache
        .write()
        .write_bytes(0, &first_dirty_pattern)
        .unwrap();

    // Race a flush+evict sequence against a new writer after writeback has
    // already started. This checks that a page re-dirtied before eviction
    // stays cached instead of being silently dropped.
    let flush_and_evict_result = Arc::new(Mutex::new(None::<Result<()>>));
    let flush_and_evict_thread = {
        let page_cache = page_cache.clone();
        let flush_and_evict_result = flush_and_evict_result.clone();
        ThreadOptions::new(move || {
            let page_cache = page_cache.read();
            let result = page_cache
                .flush_range(0..PAGE_SIZE)
                .and_then(|()| page_cache.evict_range(0..PAGE_SIZE));
            *flush_and_evict_result.lock() = Some(result);
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Write, 1);

    // Dirty the page again before the first writeback completes. Eviction
    // should leave this newest dirty page resident in cache.
    let writer_thread = {
        let vmo = vmo.clone();
        let latest_dirty_pattern = latest_dirty_pattern.clone();
        ThreadOptions::new(move || {
            write_vmo_bytes(&vmo, &latest_dirty_pattern);
        })
        .spawn()
    };

    writer_thread.join();

    // After the first writeback finishes, the flush+evict path should return,
    // but the re-dirtied page must still be readable from cache.
    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    flush_and_evict_thread.join();
    assert!(flush_and_evict_result.lock().take().unwrap().is_ok());

    let mut read_buffer = vec![0; PAGE_SIZE];
    page_cache.read().read_bytes(0, &mut read_buffer).unwrap();
    assert_eq!(read_buffer, latest_dirty_pattern);
    assert_eq!(backend.read_count(0), 0);

    backend.set_completion(IoKind::Write, IoCompletion::Immediate);
    page_cache.read().flush_range(0..PAGE_SIZE).unwrap();
    assert_eq!(backend.write_count(0), 2);
    assert_eq!(backend.persisted_page_bytes(0), latest_dirty_pattern);
}

/// Commits a page while a concurrent truncate shrinks the page cache, ensuring
/// pages beyond the new size stay inaccessible.
#[ktest]
fn concurrent_commit_and_truncate() {
    let backend = MockPageCacheBackend::new(2);
    backend.set_completion(IoKind::Read, IoCompletion::Deferred);
    backend.set_persisted_page_bytes(1, &[0x9b; PAGE_SIZE]);

    let page_cache = new_locked_backend_page_cache(&backend, 2);
    let vmo = page_cache.read().as_vmo().clone();
    let commit_second_page_result = Arc::new(Mutex::new(None::<Result<()>>));
    let resize_result = Arc::new(Mutex::new(None::<Result<()>>));
    let resize_started = Arc::new(Mutex::new(false));
    let resize_finished = Arc::new(Mutex::new(false));

    // Commit page 1 and pause its backend read so truncate can shrink the VMO
    // while that commit is still waiting for initialization to finish.
    let commit_thread = {
        let vmo = vmo.clone();
        let commit_second_page_result = commit_second_page_result.clone();
        ThreadOptions::new(move || {
            *commit_second_page_result.lock() =
                Some(vmo.commit_on(1, VmoMapMode::SharedRead).map(|_| ()));
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Read, 1);

    let resize_thread = {
        let page_cache = page_cache.clone();
        let resize_result = resize_result.clone();
        let resize_started = resize_started.clone();
        let resize_finished = resize_finished.clone();
        ThreadOptions::new(move || {
            *resize_started.lock() = true;
            *resize_result.lock() = Some(page_cache.write().resize(PAGE_SIZE, 2 * PAGE_SIZE));
            *resize_finished.lock() = true;
        })
        .spawn()
    };

    wait_until(|| *resize_started.lock());
    assert!(!*resize_finished.lock());

    // Let the blocked commit finish, then verify the truncated page is no
    // longer accessible through subsequent VMO operations.
    assert!(backend.complete_next_deferred_bio(IoKind::Read, true));
    commit_thread.join();
    resize_thread.join();

    assert!(commit_second_page_result.lock().take().unwrap().is_ok());
    assert!(resize_result.lock().take().unwrap().is_ok());
    assert_eq!(
        vmo.commit_on(1, VmoMapMode::SharedRead)
            .unwrap_err()
            .error(),
        Errno::EINVAL
    );

    let mut read_buffer = vec![0; PAGE_SIZE];
    page_cache
        .read()
        .read_bytes(PAGE_SIZE, &mut read_buffer)
        .unwrap();
    assert_eq!(read_buffer, vec![0; PAGE_SIZE]);
}

/// Faults the same cold page through `try_commit_page()` and `commit_on()`,
/// verifying that one backend read initializes the page for all waiters.
#[ktest]
fn concurrent_page_faults() {
    let backend = MockPageCacheBackend::new(1);
    backend.set_completion(IoKind::Read, IoCompletion::Deferred);
    backend.set_persisted_page_bytes(0, &[0x6b; PAGE_SIZE]);

    let page_cache = new_backend_page_cache(&backend, 1);
    let vmo = page_cache.as_vmo().clone();
    // The first page-fault style probe should report that backend I/O is
    // needed because the page has not been committed yet.
    assert!(matches!(
        vmo.try_commit_page(0, VmoMapMode::SharedRead),
        Err(VmoCommitError::NeedIo { index: 0 })
    ));

    let first_commit_finished = Arc::new(Mutex::new(false));
    let second_commit_finished = Arc::new(Mutex::new(false));

    // Start one blocking commit to issue the backend read. Other faulting
    // callers for the same page should observe the in-progress initialization.
    let first_thread = {
        let vmo = vmo.clone();
        let first_commit_finished = first_commit_finished.clone();
        ThreadOptions::new(move || {
            vmo.commit_on(0, VmoMapMode::SharedRead).unwrap();
            *first_commit_finished.lock() = true;
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Read, 1);
    match vmo.try_commit_page(0, VmoMapMode::SharedRead) {
        Err(VmoCommitError::NeedIo { index: 0 }) => {}
        other => panic!("unexpected page-fault state: {other:?}"),
    }

    // A second blocking commit should join the same initialization instead of
    // submitting a duplicate read BIO for the same page.
    let second_thread = {
        let vmo = vmo.clone();
        let second_commit_finished = second_commit_finished.clone();
        ThreadOptions::new(move || {
            vmo.commit_on(0, VmoMapMode::SharedRead).unwrap();
            *second_commit_finished.lock() = true;
        })
        .spawn()
    };

    assert_eq!(backend.read_count(0), 1);
    assert!(!*first_commit_finished.lock());
    assert!(!*second_commit_finished.lock());

    // Release the single deferred read and check that both waiters complete
    // and observe the initialized page contents.
    assert!(backend.complete_next_deferred_bio(IoKind::Read, true));
    first_thread.join();
    second_thread.join();

    assert!(*first_commit_finished.lock());
    assert!(*second_commit_finished.lock());
    assert_eq!(backend.read_count(0), 1);

    let mut read_buffer = vec![0; PAGE_SIZE];
    let mut writer = VmWriter::from(read_buffer.as_mut_slice()).to_fallible();
    vmo.read(0, &mut writer).unwrap();
    assert_eq!(read_buffer, vec![0x6b; PAGE_SIZE]);
}

/// Keeps backend reads failing for one page and checks both the initial
/// committer and a later waiter return `EIO` instead of livelocking.
#[ktest]
fn persistent_backend_errors() {
    let backend = MockPageCacheBackend::new(1);
    backend.set_completion(IoKind::Read, IoCompletion::Deferred);

    let page_cache = new_backend_page_cache(&backend, 1);
    let vmo = page_cache.as_vmo().clone();
    let first_error = Arc::new(Mutex::new(None::<Errno>));
    let second_error = Arc::new(Mutex::new(None::<Errno>));

    // Start one commit that blocks on the backend read. A second commit for
    // the same page then goes through `ensure_init()` and should also return
    // promptly once initialization fails.
    let first_thread = {
        let vmo = vmo.clone();
        let first_error = first_error.clone();
        ThreadOptions::new(move || {
            *first_error.lock() = Some(
                vmo.commit_on(0, VmoMapMode::SharedRead)
                    .unwrap_err()
                    .error(),
            );
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Read, 1);

    let second_thread = {
        let vmo = vmo.clone();
        let second_error = second_error.clone();
        ThreadOptions::new(move || {
            *second_error.lock() = Some(
                vmo.commit_on(0, VmoMapMode::SharedRead)
                    .unwrap_err()
                    .error(),
            );
        })
        .spawn()
    };

    // Fail the first backend read, then fail the retry as well. This checks
    // that both callers get `EIO` back rather than spinning forever.
    assert!(backend.complete_next_deferred_bio(IoKind::Read, false));
    backend.wait_for_deferred_bios(IoKind::Read, 1);
    assert!(backend.complete_next_deferred_bio(IoKind::Read, false));

    first_thread.join();
    second_thread.join();

    assert_eq!(&*first_error.lock(), &Some(Errno::EIO));
    assert_eq!(&*second_error.lock(), &Some(Errno::EIO));
    assert_eq!(backend.read_count(0), 2);
}

/// Delays BIO completion on demand to exercise shared read I/O, writeback
/// hand-off, and waiting for `is_writing_back` to clear.
#[ktest]
fn delayed_io_completion() {
    let backend = MockPageCacheBackend::new(1);
    backend.set_completion(IoKind::Read, IoCompletion::Deferred);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);

    let persisted_pattern = vec![0x5a; PAGE_SIZE];
    let first_dirty_pattern = vec![0x11; PAGE_SIZE];
    let latest_dirty_pattern = vec![0x33; PAGE_SIZE];
    backend.set_persisted_page_bytes(0, &persisted_pattern);

    let page_cache = new_locked_backend_page_cache(&backend, 1);
    let vmo = page_cache.read().as_vmo().clone();
    let first_read_result = Arc::new(Mutex::new(None::<Vec<u8>>));
    let second_read_result = Arc::new(Mutex::new(None::<Vec<u8>>));

    // Start two cold-page readers while read completion is delayed. They
    // should share one backend read and both see the initialized page.
    let first_reader = {
        let page_cache = page_cache.clone();
        let first_read_result = first_read_result.clone();
        ThreadOptions::new(move || {
            let mut read_buffer = vec![0; PAGE_SIZE];
            page_cache.read().read_bytes(0, &mut read_buffer).unwrap();
            *first_read_result.lock() = Some(read_buffer);
        })
        .spawn()
    };
    let second_reader = {
        let page_cache = page_cache.clone();
        let second_read_result = second_read_result.clone();
        ThreadOptions::new(move || {
            let mut read_buffer = vec![0; PAGE_SIZE];
            page_cache.read().read_bytes(0, &mut read_buffer).unwrap();
            *second_read_result.lock() = Some(read_buffer);
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Read, 1);
    assert_eq!(backend.read_count(0), 1);
    assert!(backend.complete_next_deferred_bio(IoKind::Read, true));

    first_reader.join();
    second_reader.join();

    assert_eq!(&*first_read_result.lock(), &Some(persisted_pattern.clone()));
    assert_eq!(&*second_read_result.lock(), &Some(persisted_pattern));

    // Start one deferred writeback, then dirty the page again before it
    // completes so a second flush must wait for `is_writing_back` to clear.
    page_cache
        .write()
        .write_bytes(0, &first_dirty_pattern)
        .unwrap();

    let first_flush_result = Arc::new(Mutex::new(None::<Result<()>>));
    let second_flush_result = Arc::new(Mutex::new(None::<Result<()>>));
    let second_flush_started = Arc::new(Mutex::new(false));
    let second_flush_finished = Arc::new(Mutex::new(false));

    let first_flush_thread = {
        let page_cache = page_cache.clone();
        let first_flush_result = first_flush_result.clone();
        ThreadOptions::new(move || {
            *first_flush_result.lock() = Some(page_cache.read().flush_range(0..PAGE_SIZE));
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Write, 1);

    write_vmo_bytes(&vmo, &latest_dirty_pattern);

    let second_flush_thread = {
        let page_cache = page_cache.clone();
        let second_flush_result = second_flush_result.clone();
        let second_flush_started = second_flush_started.clone();
        let second_flush_finished = second_flush_finished.clone();
        ThreadOptions::new(move || {
            *second_flush_started.lock() = true;
            *second_flush_result.lock() = Some(page_cache.read().flush_range(0..PAGE_SIZE));
            *second_flush_finished.lock() = true;
        })
        .spawn()
    };

    wait_until(|| *second_flush_started.lock());
    assert_eq!(backend.write_count(0), 1);
    assert!(!*second_flush_finished.lock());

    // Complete the first and second writebacks one by one. This exercises the
    // ownership hand-off in async writeback and the wait-for-writeback path.
    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    backend.wait_for_deferred_bios(IoKind::Write, 1);
    assert_eq!(backend.write_count(0), 2);
    assert!(!*second_flush_finished.lock());

    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    first_flush_thread.join();
    second_flush_thread.join();

    assert!(first_flush_result.lock().take().unwrap().is_ok());
    assert!(second_flush_result.lock().take().unwrap().is_ok());
    assert_eq!(backend.persisted_page_bytes(0), latest_dirty_pattern);
}

/// A failed asynchronous write completion must leave the page dirty so a
/// later flush can retry instead of silently losing the buffered update.
#[ktest]
fn failed_write_completion_redirties_page_for_retry() {
    let backend = MockPageCacheBackend::new(1);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);

    let page_cache = new_locked_backend_page_cache(&backend, 1);
    let dirty_pattern = vec![0x7d; PAGE_SIZE];
    page_cache.write().write_bytes(0, &dirty_pattern).unwrap();

    let first_flush_result = Arc::new(Mutex::new(None::<Result<()>>));
    let flush_thread = {
        let page_cache = page_cache.clone();
        let first_flush_result = first_flush_result.clone();
        ThreadOptions::new(move || {
            *first_flush_result.lock() = Some(page_cache.read().flush_range(0..PAGE_SIZE));
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Write, 1);
    assert!(backend.complete_next_deferred_bio(IoKind::Write, false));
    flush_thread.join();

    assert_eq!(
        first_flush_result
            .lock()
            .take()
            .unwrap()
            .unwrap_err()
            .error(),
        Errno::EIO
    );
    assert_eq!(
        page_cache.read().dirty_ranges(0..PAGE_SIZE),
        vec![0..PAGE_SIZE]
    );

    backend.set_completion(IoKind::Write, IoCompletion::Immediate);
    page_cache.read().flush_range(0..PAGE_SIZE).unwrap();
    assert_eq!(backend.write_count(0), 2);
    assert_eq!(backend.persisted_page_bytes(0), dirty_pattern);
}
