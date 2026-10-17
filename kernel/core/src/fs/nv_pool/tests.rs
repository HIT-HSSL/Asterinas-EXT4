// SPDX-License-Identifier: MPL-2.0

use core::sync::atomic::{AtomicUsize, Ordering};

use ostd::prelude::ktest;

use super::{
    backend::NvMemoryBackend,
    capacity::PoolClass,
    config::{NvPoolConfig, NvSimMode},
    data_page::DataPageState,
    dram::{DramNvBackend, DramNvImage},
    fault::FaultPoint,
    frame::NvPool,
    id::{NvFrameId, NvPageId},
    persist::PersistEngine,
};
use crate::{
    prelude::*,
    thread::{Thread, kernel_thread::ThreadOptions},
};

#[ktest]
fn boot_config_initializes_global_backend() {
    component::init_all(
        component::InitStage::Bootstrap,
        component::parse_metadata!(),
    )
    .unwrap();
    let configured_pages = NvPoolConfig::from_command_line()
        .unwrap()
        .expect("this test requires nv_pool_size")
        .total_pages();

    super::init();

    assert_eq!(super::pool().unwrap().stats().total_pages, configured_pages);
}

#[ktest]
fn config_is_disabled_without_pool_size() {
    assert_eq!(
        NvPoolConfig::from_bytes(None, None, None, None).unwrap(),
        None
    );
}

#[ktest]
fn config_accepts_page_aligned_capacity_split() {
    let config = NvPoolConfig::from_bytes(
        Some(16 * PAGE_SIZE),
        Some(4 * PAGE_SIZE),
        Some(12 * PAGE_SIZE),
        Some(NvSimMode::Fast),
    )
    .unwrap()
    .unwrap();

    assert_eq!(config.total_pages(), 16);
    assert_eq!(config.meta_reserve_pages(), 4);
    assert_eq!(config.data_limit_pages(), 12);
    assert_eq!(config.mode(), NvSimMode::Fast);
}

#[ktest]
fn config_accepts_model_restart_mode() {
    let config = NvPoolConfig::from_params(Some("16K"), Some("4K"), Some("12K"), Some("model"))
        .unwrap()
        .unwrap();
    assert_eq!(config.mode(), NvSimMode::Model);
}

#[ktest]
fn config_rejects_unaligned_and_inconsistent_capacities() {
    assert!(NvPoolConfig::from_bytes(Some(PAGE_SIZE + 1), None, None, None).is_err());
    assert!(
        NvPoolConfig::from_bytes(
            Some(8 * PAGE_SIZE),
            Some(9 * PAGE_SIZE),
            Some(4 * PAGE_SIZE),
            None,
        )
        .is_err()
    );
    assert!(
        NvPoolConfig::from_bytes(
            Some(8 * PAGE_SIZE),
            Some(4 * PAGE_SIZE),
            Some(5 * PAGE_SIZE),
            None,
        )
        .is_err()
    );
    assert!(
        NvPoolConfig::from_bytes(
            Some((u32::MAX as usize + 2) * PAGE_SIZE),
            Some(PAGE_SIZE),
            Some(0),
            None,
        )
        .is_err()
    );
}

#[ktest]
fn config_parses_kernel_parameter_values() {
    let config = NvPoolConfig::from_params(Some("16K"), Some("4K"), Some("12K"), Some("fast"))
        .unwrap()
        .unwrap();

    assert_eq!(config.total_pages(), 4);
    assert_eq!(config.meta_reserve_pages(), 1);
    assert_eq!(config.data_limit_pages(), 3);
    assert_eq!(config.mode(), NvSimMode::Fast);
}

#[ktest]
fn config_rejects_invalid_kernel_parameter_values() {
    assert!(NvPoolConfig::from_params(Some("4097"), Some("4K"), Some("0"), None).is_err());
    assert!(NvPoolConfig::from_params(Some("8K"), Some("4K"), Some("4K"), Some("slow")).is_err());
    assert!(NvPoolConfig::from_params(None, None, None, Some("fast")).is_err());
}

#[ktest]
fn pool_ids_use_checked_conversions() {
    assert_eq!(NvFrameId::try_from(7usize).unwrap().index(), 7);
    assert_eq!(NvPageId::try_from(11usize).unwrap().index(), 11);

    if usize::BITS > u32::BITS {
        let too_large = u32::MAX as usize + 1;
        assert!(NvFrameId::try_from(too_large).is_err());
        assert!(NvPageId::try_from(too_large).is_err());
    }
}

#[ktest]
fn dram_backend_locates_pages_across_chunks() {
    let config = NvPoolConfig::from_bytes(
        Some(5 * PAGE_SIZE),
        Some(PAGE_SIZE),
        Some(4 * PAGE_SIZE),
        None,
    )
    .unwrap()
    .unwrap();
    let backend = DramNvBackend::allocate_with_chunk_pages(config, 2).unwrap();
    let pattern = [0x5a; PAGE_SIZE];

    backend
        .write_page(NvPageId::try_from(2usize).unwrap(), &pattern)
        .unwrap();
    let mut actual = [0; PAGE_SIZE];
    backend
        .read_page(NvPageId::try_from(2usize).unwrap(), &mut actual)
        .unwrap();

    assert_eq!(backend.capacity_pages(), 5);
    assert_eq!(actual, pattern);
}

#[ktest]
fn dram_backend_rejects_invalid_page_access() {
    let config =
        NvPoolConfig::from_bytes(Some(2 * PAGE_SIZE), Some(PAGE_SIZE), Some(PAGE_SIZE), None)
            .unwrap()
            .unwrap();
    let backend = DramNvBackend::allocate_with_chunk_pages(config, 1).unwrap();

    assert!(
        backend
            .write_page(NvPageId::try_from(2usize).unwrap(), &[0; PAGE_SIZE])
            .is_err()
    );
    assert!(
        backend
            .write_page(NvPageId::try_from(0usize).unwrap(), &[0; 1])
            .is_err()
    );
}

#[ktest]
fn fast_mode_counts_persist_and_fence_boundaries() {
    let config = NvPoolConfig::from_bytes(
        Some(2 * PAGE_SIZE),
        Some(PAGE_SIZE),
        Some(PAGE_SIZE),
        Some(NvSimMode::Fast),
    )
    .unwrap()
    .unwrap();
    let backend = DramNvBackend::allocate_with_chunk_pages(config, 1).unwrap();

    backend.persist(0, PAGE_SIZE).unwrap();
    backend.fence().unwrap();
    backend.persist(PAGE_SIZE, PAGE_SIZE).unwrap();

    let stats = backend.stats();
    assert_eq!(stats.persist_calls, 2);
    assert_eq!(stats.fence_calls, 1);
}

#[ktest]
fn dram_backend_owns_its_frames_exclusively() {
    const TEST_PAGES: usize = 3;
    let config = NvPoolConfig::from_bytes(
        Some(TEST_PAGES * PAGE_SIZE),
        Some(PAGE_SIZE),
        Some(2 * PAGE_SIZE),
        None,
    )
    .unwrap()
    .unwrap();
    let before = osdk_frame_allocator::load_total_free_size();

    let backend = DramNvBackend::allocate_with_chunk_pages(config, 2).unwrap();
    let while_owned = osdk_frame_allocator::load_total_free_size();
    assert!(before.saturating_sub(while_owned) >= TEST_PAGES * PAGE_SIZE);

    drop(backend);
    let after_drop = osdk_frame_allocator::load_total_free_size();
    assert!(after_drop.saturating_sub(while_owned) >= TEST_PAGES * PAGE_SIZE);
}

#[ktest]
fn dram_backend_reports_impossible_capacity() {
    let addressable_pages = u32::MAX as usize + 1;
    let config = NvPoolConfig::from_bytes(
        Some(addressable_pages * PAGE_SIZE),
        Some(PAGE_SIZE),
        Some(0),
        Some(NvSimMode::Fast),
    )
    .unwrap()
    .unwrap();

    assert!(DramNvBackend::allocate_with_chunk_pages(config, 1).is_err());
}

fn test_pool(
    total_pages: usize,
    meta_reserve_pages: usize,
    data_limit_pages: usize,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Arc<NvPool> {
    let config = NvPoolConfig::from_bytes(
        Some(total_pages * PAGE_SIZE),
        Some(meta_reserve_pages * PAGE_SIZE),
        Some(data_limit_pages * PAGE_SIZE),
        Some(NvSimMode::Fast),
    )
    .unwrap()
    .unwrap();
    let backend: Arc<dyn NvMemoryBackend> =
        Arc::new(DramNvBackend::allocate_with_chunk_pages(config, 2).unwrap());
    Arc::new(NvPool::new(backend, config, wake).unwrap())
}

#[ktest]
fn capacity_policy_preserves_metadata_progress() {
    for (total, reserve, data_limit) in [(8, 2, 6), (7, 3, 4), (6, 2, 3)] {
        let pool = test_pool(total, reserve, data_limit, None);
        let mut data_pages = Vec::new();
        for _ in 0..data_limit {
            data_pages.push(pool.alloc_data_page().unwrap());
        }
        assert_eq!(pool.alloc_data_page().unwrap_err().error(), Errno::EAGAIN);

        let mut metadata_frames = Vec::new();
        for _ in 0..total - data_limit {
            metadata_frames.push(pool.alloc_frame(PoolClass::Metadata).unwrap());
        }
        assert_eq!(
            pool.alloc_frame(PoolClass::Metadata).unwrap_err().error(),
            Errno::ENOSPC
        );

        for frame in metadata_frames {
            pool.free_frame(frame).unwrap();
        }
        for page in data_pages {
            pool.free_data_page(page).unwrap();
        }
    }
}

#[ktest]
fn metadata_can_borrow_unused_data_capacity() {
    let pool = test_pool(8, 2, 6, None);
    let mut frames = Vec::new();
    for _ in 0..7 {
        frames.push(pool.alloc_frame(PoolClass::Metadata).unwrap());
    }
    let page = pool.alloc_data_page().unwrap();

    assert_eq!(pool.stats().metadata_pages, 7);
    assert_eq!(pool.stats().data_pages, 1);

    pool.free_data_page(page).unwrap();
    for frame in frames {
        pool.free_frame(frame).unwrap();
    }
}

#[ktest]
fn high_watermark_wakes_and_hard_limit_backpressures() {
    let wakes = Arc::new(AtomicUsize::new(0));
    let callback_wakes = wakes.clone();
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        callback_wakes.fetch_add(1, Ordering::Relaxed);
    });
    let pool = test_pool(8, 2, 6, Some(wake));

    let mut pages = Vec::new();
    for _ in 0..6 {
        pages.push(pool.alloc_data_page().unwrap());
    }
    assert_eq!(wakes.load(Ordering::Relaxed), 1);
    assert_eq!(pool.alloc_data_page().unwrap_err().error(), Errno::EAGAIN);
    let stats = pool.stats();
    assert_eq!(stats.high_water_events, 1);
    assert_eq!(stats.backpressure_events, 1);

    pool.free_data_page(pages.pop().unwrap()).unwrap();
    pages.push(pool.alloc_data_page().unwrap());
    assert_eq!(wakes.load(Ordering::Relaxed), 2);
    for page in pages {
        pool.free_data_page(page).unwrap();
    }

    let low_limit_wakes = Arc::new(AtomicUsize::new(0));
    let callback_wakes = low_limit_wakes.clone();
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        callback_wakes.fetch_add(1, Ordering::Relaxed);
    });
    let low_limit_pool = test_pool(8, 6, 2, Some(wake));
    let _first = low_limit_pool.alloc_data_page().unwrap();
    let _second = low_limit_pool.alloc_data_page().unwrap();
    assert_eq!(low_limit_wakes.load(Ordering::Relaxed), 1);
    assert_eq!(
        low_limit_pool.alloc_data_page().unwrap_err().error(),
        Errno::EAGAIN
    );
}

#[ktest]
fn pinned_and_repeated_free_are_rejected() {
    let pool = test_pool(4, 1, 3, None);
    let frame = pool.alloc_frame(PoolClass::Metadata).unwrap();
    let pin = pool.pin_frame(frame).unwrap();

    assert_eq!(pool.free_frame(frame).unwrap_err().error(), Errno::EBUSY);
    drop(pin);
    pool.free_frame(frame).unwrap();
    assert_eq!(pool.free_frame(frame).unwrap_err().error(), Errno::EINVAL);
}

#[ktest]
fn data_page_transitions_are_checked() {
    let pool = test_pool(4, 1, 3, None);
    let page = pool.alloc_data_page().unwrap();
    assert_eq!(pool.data_page_state(page).unwrap(), DataPageState::Staged);

    assert!(
        pool.transition_data_page(page, DataPageState::Staged, DataPageState::InFlight)
            .is_err()
    );
    pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
        .unwrap();
    pool.transition_data_page(page, DataPageState::NvDirty, DataPageState::InFlight)
        .unwrap();
    assert_eq!(pool.free_data_page(page).unwrap_err().error(), Errno::EBUSY);
    pool.transition_data_page(page, DataPageState::InFlight, DataPageState::NvDirty)
        .unwrap();
    pool.transition_data_page(page, DataPageState::NvDirty, DataPageState::InFlight)
        .unwrap();
    pool.transition_data_page(page, DataPageState::InFlight, DataPageState::Synced)
        .unwrap();
    pool.free_data_page(page).unwrap();
    assert_eq!(pool.data_page_state(page).unwrap(), DataPageState::Free);
}

#[ktest]
fn staging_data_page_copies_a_full_page_and_returns_a_staged_pin() {
    let pool = test_pool(4, 1, 3, None);
    let content = [0x4c; PAGE_SIZE];
    let page = pool.stage_data_page(&content).unwrap();

    assert_eq!(pool.data_page_state(page).unwrap(), DataPageState::Staged);
    let pin = pool.pin_data_page(page).unwrap();
    let mut read_back = [0; PAGE_SIZE];
    pin.read(&mut read_back).unwrap();
    assert_eq!(read_back, content);

    pin.write(&[0x9e; PAGE_SIZE]).unwrap();
    pin.read(&mut read_back).unwrap();
    assert_eq!(read_back, [0x9e; PAGE_SIZE]);
    drop(pin);
    pool.free_data_page(page).unwrap();
}

#[ktest]
fn staging_data_page_rejects_partial_content() {
    let pool = test_pool(4, 1, 3, None);
    let partial = [0; PAGE_SIZE - 1];

    assert_eq!(
        pool.stage_data_page(&partial).unwrap_err().error(),
        Errno::EINVAL
    );
    assert_eq!(pool.stats().free_pages, 4);
}

#[ktest]
fn committed_data_pages_are_resident_and_read_only() {
    let (_image, _config, pool) = restartable_pool(4);
    let page = pool.stage_data_page(&[0x73; PAGE_SIZE]).unwrap();
    pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
        .unwrap();
    pool.commit_data_page(page, 191, None).unwrap();

    assert_eq!(pool.resident_data_pages(), vec![(191, page)]);
    let pin = pool.pin_data_page(page).unwrap();
    let mut content = [0; PAGE_SIZE];
    pin.read(&mut content).unwrap();
    assert_eq!(content, [0x73; PAGE_SIZE]);
    assert_eq!(
        pin.write(&[0x82; PAGE_SIZE]).unwrap_err().error(),
        Errno::EBUSY
    );
}

#[ktest]
fn concurrent_alloc_pin_free_never_duplicates_an_id() {
    const THREADS: usize = 4;
    const ITERATIONS: usize = 128;
    let pool = test_pool(8, 8, 0, None);
    let active: Arc<SpinLock<BTreeSet<usize>>> = Arc::new(SpinLock::new(BTreeSet::new()));
    let mut workers = Vec::new();

    for _ in 0..THREADS {
        let pool = pool.clone();
        let active = active.clone();
        workers.push(
            ThreadOptions::new(move || {
                for _ in 0..ITERATIONS {
                    let frame = loop {
                        match pool.alloc_frame(PoolClass::Metadata) {
                            Ok(frame) => break frame,
                            Err(error) if error.error() == Errno::ENOSPC => Thread::yield_now(),
                            Err(error) => panic!("unexpected allocation error: {:?}", error),
                        }
                    };
                    assert!(active.lock().insert(frame.index()));
                    let pin = pool.pin_frame(frame).unwrap();
                    Thread::yield_now();
                    drop(pin);
                    assert!(active.lock().remove(&frame.index()));
                    pool.free_frame(frame).unwrap();
                }
            })
            .spawn(),
        );
    }

    for worker in workers {
        worker.join();
    }
    assert!(active.lock().is_empty());
    assert_eq!(pool.stats().free_pages, 8);
}

fn restartable_pool(total: usize) -> (DramNvImage, NvPoolConfig, Arc<NvPool>) {
    let config = NvPoolConfig::from_bytes(
        Some(total * PAGE_SIZE),
        Some(PAGE_SIZE),
        Some((total - 1) * PAGE_SIZE),
        Some(NvSimMode::Model),
    )
    .unwrap()
    .unwrap();
    let image = DramNvImage::allocate_model(config).unwrap();
    let pool = Arc::new(
        NvPool::format(Arc::new(image.attach()), config, None).expect("fresh image should format"),
    );
    (image, config, pool)
}

#[ktest]
fn committed_frame_is_rebuilt_from_durable_bytes_only() {
    let (image, config, pool) = restartable_pool(4);
    let frame = pool.alloc_frame(PoolClass::Metadata).unwrap();
    let pin = pool.pin_frame(frame).unwrap();
    pin.write(&[0x5a; PAGE_SIZE]).unwrap();
    pool.commit_frame(frame, 41, 0xffff, None).unwrap();
    drop(pin);
    drop(pool);

    image.simulate_crash();
    let recovered = Arc::new(NvPool::recover(Arc::new(image.attach()), config, None).unwrap());
    assert_eq!(recovered.stats().pinned_frames, 0);
    let recovered_frame = recovered.resident_frame(41).unwrap();
    let recovered_pin = recovered.pin_frame(recovered_frame).unwrap();
    let mut content = [0; PAGE_SIZE];
    recovered_pin.read(&mut content).unwrap();

    assert_eq!(content, [0x5a; PAGE_SIZE]);
    assert_eq!(recovered.stats().pinned_frames, 1);
    drop(recovered_pin);
    assert_eq!(recovered.stats().pinned_frames, 0);
}

#[ktest]
fn recovery_observes_only_fully_persisted_commits() {
    for (fault, committed) in [
        (FaultPoint::BeforeContentPersist, false),
        (FaultPoint::AfterContentPersist, false),
        (FaultPoint::AfterCommitStore, false),
        (FaultPoint::AfterCommitPersist, true),
        (FaultPoint::AfterCommitFence, true),
    ] {
        let (image, config, pool) = restartable_pool(4);
        let frame = pool.alloc_frame(PoolClass::Metadata).unwrap();
        pool.pin_frame(frame)
            .unwrap()
            .write(&[fault as u8; PAGE_SIZE])
            .unwrap();
        assert!(pool.commit_frame(frame, 73, 1, Some(fault)).is_err());
        drop(pool);

        image.simulate_crash();
        let recovered = NvPool::recover(Arc::new(image.attach()), config, None).unwrap();
        assert_eq!(recovered.resident_frame(73).is_some(), committed);
        assert_eq!(recovered.stats().pinned_frames, 0);
    }
}

#[ktest]
fn recovery_observes_only_fully_persisted_clears() {
    for (fault, remains_committed) in [
        (FaultPoint::BeforeClear, true),
        (FaultPoint::AfterClearStore, true),
        (FaultPoint::AfterClearPersist, false),
        (FaultPoint::AfterClearFence, false),
    ] {
        let (image, config, pool) = restartable_pool(4);
        let frame = pool.alloc_frame(PoolClass::Metadata).unwrap();
        pool.pin_frame(frame)
            .unwrap()
            .write(&[0x3c; PAGE_SIZE])
            .unwrap();
        pool.commit_frame(frame, 89, 1, None).unwrap();
        assert!(pool.clear_frame(frame, Some(fault)).is_err());
        drop(pool);

        image.simulate_crash();
        let recovered = NvPool::recover(Arc::new(image.attach()), config, None).unwrap();
        assert_eq!(recovered.resident_frame(89).is_some(), remains_committed);
    }
}

#[ktest]
fn recovery_rejects_duplicate_resident_home_blocks() {
    let (image, config, pool) = restartable_pool(4);
    let first = pool.alloc_frame(PoolClass::Metadata).unwrap();
    let second = pool.alloc_frame(PoolClass::Metadata).unwrap();
    pool.commit_frame(first, 101, 1, None).unwrap();
    PersistEngine::new(Arc::new(image.attach()), config.total_pages())
        .unwrap()
        .commit_frame(
            second.index(),
            PoolClass::Metadata,
            [0; 16],
            101,
            1,
            1,
            None,
        )
        .unwrap();
    drop(pool);

    image.simulate_crash();
    assert!(NvPool::recover(Arc::new(image.attach()), config, None).is_err());
}

#[ktest]
fn committed_frame_requires_clear_before_reuse() {
    let (_image, _config, pool) = restartable_pool(4);
    let first = pool.alloc_frame(PoolClass::Metadata).unwrap();
    let second = pool.alloc_frame(PoolClass::Metadata).unwrap();
    let pin = pool.pin_frame(first).unwrap();
    pool.commit_frame(first, 131, 1, None).unwrap();

    assert_eq!(
        pin.write(&[0xa5; PAGE_SIZE]).unwrap_err().error(),
        Errno::EBUSY
    );
    assert_eq!(
        pool.commit_frame(first, 132, 1, None).unwrap_err().error(),
        Errno::EBUSY
    );
    assert_eq!(
        pool.commit_frame(second, 131, 1, None).unwrap_err().error(),
        Errno::EBUSY
    );
    assert_eq!(pool.free_frame(first).unwrap_err().error(), Errno::EBUSY);
    drop(pin);
    pool.clear_frame(first, None).unwrap();
    pool.free_frame(first).unwrap();
}

#[ktest]
fn recovery_selects_latest_metadata_version_and_reclaims_older_version() {
    let (image, config, pool) = restartable_pool(6);
    let key = super::frame::MetaKey::new([0x23; 16], 131);
    let first = pool.alloc_frame(PoolClass::Metadata).unwrap();
    pool.pin_frame(first)
        .unwrap()
        .write(&[0x11; PAGE_SIZE])
        .unwrap();
    assert!(
        pool.replace_metadata_frame(first, key, 1, None)
            .unwrap()
            .is_none()
    );
    let second = pool.alloc_frame(PoolClass::Metadata).unwrap();
    pool.pin_frame(second)
        .unwrap()
        .write(&[0x22; PAGE_SIZE])
        .unwrap();
    assert_eq!(
        pool.replace_metadata_frame(second, key, 1, None).unwrap(),
        Some(first)
    );
    drop(pool);

    image.simulate_crash();
    let recovered = Arc::new(NvPool::recover(Arc::new(image.attach()), config, None).unwrap());
    let current = recovered.resident_meta_frame(key).unwrap();
    assert_eq!(current, second);
    let pin = recovered.pin_frame(current).unwrap();
    let mut content = [0; PAGE_SIZE];
    pin.read(&mut content).unwrap();
    assert_eq!(content, [0x22; PAGE_SIZE]);
    drop(pin);
    assert_eq!(recovered.stats().free_pages, 5);
}

#[ktest]
fn metadata_namespaces_isolate_identical_home_blocks() {
    let (_image, _config, pool) = restartable_pool(4);
    let first_key = super::frame::MetaKey::new([1; 16], 7);
    let second_key = super::frame::MetaKey::new([2; 16], 7);
    let first = pool.alloc_frame(PoolClass::Metadata).unwrap();
    let second = pool.alloc_frame(PoolClass::Metadata).unwrap();
    pool.replace_metadata_frame(first, first_key, 1, None)
        .unwrap();
    pool.replace_metadata_frame(second, second_key, 1, None)
        .unwrap();

    assert_eq!(pool.resident_meta_frame(first_key), Some(first));
    assert_eq!(pool.resident_meta_frame(second_key), Some(second));
}

#[ktest]
fn data_namespaces_isolate_identical_home_blocks() {
    let (_image, _config, pool) = restartable_pool(4);
    let first_key = super::frame::MetaKey::new([1; 16], 7);
    let second_key = super::frame::MetaKey::new([2; 16], 7);
    let first = pool.stage_data_page(&[0x11; PAGE_SIZE]).unwrap();
    let second = pool.stage_data_page(&[0x22; PAGE_SIZE]).unwrap();
    pool.transition_data_page(first, DataPageState::Staged, DataPageState::NvDirty)
        .unwrap();
    pool.transition_data_page(second, DataPageState::Staged, DataPageState::NvDirty)
        .unwrap();
    assert_eq!(
        pool.commit_namespaced_data_page(first, first_key, None)
            .unwrap(),
        None
    );
    assert_eq!(
        pool.commit_namespaced_data_page(second, second_key, None)
            .unwrap(),
        None
    );

    assert_eq!(pool.data_page_key(first).unwrap(), first_key);
    assert_eq!(pool.data_page_key(second).unwrap(), second_key);
}

#[ktest]
fn recovery_preserves_all_data_pages_for_the_same_target() {
    let (image, config, pool) = restartable_pool(4);
    let key = super::frame::MetaKey::new([0x31; 16], 7);
    let first = pool.stage_data_page(&[0x11; PAGE_SIZE]).unwrap();
    let second = pool.stage_data_page(&[0x22; PAGE_SIZE]).unwrap();
    for page in [first, second] {
        pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
            .unwrap();
        pool.commit_namespaced_data_page(page, key, None).unwrap();
    }
    drop(pool);

    image.simulate_crash();
    let recovered = NvPool::recover(Arc::new(image.attach()), config, None).unwrap();
    let pages: BTreeSet<_> = recovered
        .committed_data_pages(key.namespace())
        .into_iter()
        .map(|(_, page)| page)
        .collect();
    assert_eq!(pages, BTreeSet::from([first, second]));
}

#[ktest]
fn frame_eviction_requires_exclusive_pin_ownership() {
    let (_image, _config, pool) = restartable_pool(4);
    let frame = pool.alloc_frame(PoolClass::Metadata).unwrap();
    pool.pin_frame(frame)
        .unwrap()
        .write(&[0x6d; PAGE_SIZE])
        .unwrap();
    pool.commit_frame(frame, 149, 1, None).unwrap();

    let reader = pool.pin_frame(frame).unwrap();
    assert_eq!(
        pool.begin_frame_eviction(frame).unwrap_err().error(),
        Errno::EBUSY
    );
    drop(reader);

    let eviction = pool.begin_frame_eviction(frame).unwrap();
    let mut content = [0; PAGE_SIZE];
    eviction.read(&mut content).unwrap();
    assert_eq!(content, [0x6d; PAGE_SIZE]);
    eviction.finish(None).unwrap();
    assert!(pool.resident_frame(149).is_none());
    pool.free_frame(frame).unwrap();
}

#[ktest]
fn dropping_unfinished_frame_eviction_preserves_residency() {
    let (_image, _config, pool) = restartable_pool(4);
    let frame = pool.alloc_frame(PoolClass::Metadata).unwrap();
    pool.commit_frame(frame, 157, 1, None).unwrap();

    drop(pool.begin_frame_eviction(frame).unwrap());

    assert_eq!(pool.resident_frame(157), Some(frame));
    assert_eq!(pool.stats().pinned_frames, 0);
    pool.clear_frame(frame, None).unwrap();
    pool.free_frame(frame).unwrap();
}

#[ktest]
fn data_page_eviction_excludes_readers_and_releases_a_drained_page() {
    let (_image, _config, pool) = restartable_pool(4);
    let page = pool.stage_data_page(&[0x28; PAGE_SIZE]).unwrap();
    pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
        .unwrap();
    pool.commit_data_page(page, 193, None).unwrap();

    let reader = pool.pin_data_page(page).unwrap();
    assert_eq!(
        pool.begin_data_page_eviction(page).unwrap_err().error(),
        Errno::EBUSY
    );
    drop(reader);

    let eviction = pool.begin_data_page_eviction(page).unwrap();
    let mut content = [0; PAGE_SIZE];
    eviction.read(&mut content).unwrap();
    assert_eq!(content, [0x28; PAGE_SIZE]);
    eviction.finish(None).unwrap();

    assert_eq!(pool.resident_data_page(193), None);
    assert_eq!(pool.data_page_state(page).unwrap(), DataPageState::Synced);
    pool.free_data_page(page).unwrap();
}

#[ktest]
fn dropping_unfinished_data_page_eviction_preserves_residency() {
    let (_image, _config, pool) = restartable_pool(4);
    let page = pool.stage_data_page(&[0x4f; PAGE_SIZE]).unwrap();
    pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
        .unwrap();
    pool.commit_data_page(page, 197, None).unwrap();

    drop(pool.begin_data_page_eviction(page).unwrap());

    assert_eq!(pool.resident_data_page(197), Some(page));
    assert_eq!(pool.data_page_state(page).unwrap(), DataPageState::NvDirty);
    assert_eq!(pool.stats().pinned_frames, 0);
}

#[ktest]
fn committed_data_page_is_rebuilt_as_nv_dirty() {
    let (image, config, pool) = restartable_pool(4);
    let page = pool.alloc_data_page().unwrap();
    pool.transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
        .unwrap();
    pool.commit_data_page(page, 117, None).unwrap();
    drop(pool);

    image.simulate_crash();
    let recovered = NvPool::recover(Arc::new(image.attach()), config, None).unwrap();
    let recovered_page = recovered.resident_data_page(117).unwrap();
    assert_eq!(recovered_page, page);
    assert_eq!(
        recovered.data_page_state(recovered_page).unwrap(),
        DataPageState::NvDirty
    );
    assert_eq!(
        recovered
            .free_data_page(recovered_page)
            .unwrap_err()
            .error(),
        Errno::EBUSY
    );
    recovered.clear_data_page(recovered_page, None).unwrap();
    recovered.free_data_page(recovered_page).unwrap();
}

#[ktest]
fn recovery_rejects_an_unformatted_image() {
    let config = NvPoolConfig::from_bytes(
        Some(4 * PAGE_SIZE),
        Some(PAGE_SIZE),
        Some(3 * PAGE_SIZE),
        Some(NvSimMode::Model),
    )
    .unwrap()
    .unwrap();
    let image = DramNvImage::allocate_model(config).unwrap();
    assert!(NvPool::recover(Arc::new(image.attach()), config, None).is_err());
}

#[ktest]
fn simulated_crash_invalidates_old_backend_sessions() {
    let config = NvPoolConfig::from_bytes(
        Some(4 * PAGE_SIZE),
        Some(PAGE_SIZE),
        Some(3 * PAGE_SIZE),
        Some(NvSimMode::Model),
    )
    .unwrap()
    .unwrap();
    let image = DramNvImage::allocate_model(config).unwrap();
    let old_backend = image.attach();
    image.simulate_crash();

    let mut page = [0; PAGE_SIZE];
    assert!(
        old_backend
            .read_page(NvPageId::try_from(0).unwrap(), &mut page)
            .is_err()
    );
    assert!(
        image
            .attach()
            .read_page(NvPageId::try_from(0).unwrap(), &mut page)
            .is_ok()
    );
}
