// SPDX-License-Identifier: MPL-2.0

//! Allocation, pinning, and object state management for NVPool pages.

use core::fmt;

use super::{
    backend::NvMemoryBackend,
    capacity::{CapacityState, NvPoolStats, PoolClass},
    config::NvPoolConfig,
    data_page::DataPageState,
    fault::FaultPoint,
    id::{NvFrameId, NvPageId},
    persist::PersistEngine,
    recover,
};
use crate::prelude::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObjectKind {
    Free,
    Metadata,
    Data(DataPageState),
}

#[derive(Clone, Copy, Debug)]
struct ObjectRecord {
    kind: ObjectKind,
    pins: u32,
    persisting: bool,
    committed: bool,
    key: Option<MetaKey>,
    version: u64,
}

impl ObjectRecord {
    fn free() -> Self {
        Self {
            kind: ObjectKind::Free,
            pins: 0,
            persisting: false,
            committed: false,
            key: None,
            version: 0,
        }
    }
}

struct PoolState {
    records: Vec<ObjectRecord>,
    free_ids: Vec<usize>,
    resident: BTreeMap<MetaKey, ResidentRecord>,
    capacity: CapacityState,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct MetaKey {
    namespace: [u8; 16],
    home_block: u64,
}

impl MetaKey {
    pub(crate) const fn new(namespace: [u8; 16], home_block: u64) -> Self {
        Self {
            namespace,
            home_block,
        }
    }

    const fn legacy(home_block: u64) -> Self {
        Self::new([0; 16], home_block)
    }

    pub(crate) const fn namespace(self) -> [u8; 16] {
        self.namespace
    }

    pub(crate) const fn home_block(self) -> u64 {
        self.home_block
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ResidentRecord {
    pub(super) index: usize,
    pub(super) version: u64,
}

pub(crate) struct NvPool {
    backend: Arc<dyn NvMemoryBackend>,
    persist: PersistEngine,
    state: SpinLock<PoolState>,
    high_water_wake: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl NvPool {
    pub(crate) fn new(
        backend: Arc<dyn NvMemoryBackend>,
        config: NvPoolConfig,
        high_water_wake: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Self> {
        if backend.capacity_pages() != config.total_pages() {
            return_errno_with_message!(
                Errno::EINVAL,
                "NVPool backend and configured capacities differ"
            );
        }

        let total_pages = config.total_pages();
        let mut records = Vec::new();
        records.try_reserve_exact(total_pages).map_err(|_| {
            Error::with_message(Errno::ENOMEM, "NVPool object table allocation failed")
        })?;
        records.resize(total_pages, ObjectRecord::free());
        let mut free_ids = Vec::new();
        free_ids.try_reserve_exact(total_pages).map_err(|_| {
            Error::with_message(Errno::ENOMEM, "NVPool free-list allocation failed")
        })?;
        free_ids.extend((0..total_pages).rev());
        let persist = PersistEngine::new(backend.clone(), total_pages)?;
        Ok(Self {
            backend,
            persist,
            state: SpinLock::new(PoolState {
                records,
                free_ids,
                resident: BTreeMap::new(),
                capacity: CapacityState::new(config),
            }),
            high_water_wake,
        })
    }

    pub(crate) fn format(
        backend: Arc<dyn NvMemoryBackend>,
        config: NvPoolConfig,
        high_water_wake: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Self> {
        let pool = Self::new(backend, config, high_water_wake)?;
        pool.persist.format()?;
        Ok(pool)
    }

    pub(crate) fn recover(
        backend: Arc<dyn NvMemoryBackend>,
        config: NvPoolConfig,
        high_water_wake: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Self> {
        let persist = PersistEngine::new(backend.clone(), config.total_pages())?;
        let (entries, resident) = recover::scan(&persist, config.total_pages())?;
        let mut pool = Self::new(backend, config, high_water_wake)?;
        {
            let state = pool.state.get_mut();
            state.resident = resident;
            for entry in entries {
                state.records[entry.index].kind = match entry.class {
                    PoolClass::Metadata => ObjectKind::Metadata,
                    PoolClass::Data => ObjectKind::Data(DataPageState::NvDirty),
                };
                state.records[entry.index].committed = true;
                state.records[entry.index].key = Some(entry.key);
                state.records[entry.index].version = entry.version;
                state.capacity.account_recovered(entry.class)?;
            }
            state
                .free_ids
                .retain(|index| state.records[*index].kind == ObjectKind::Free);
        }
        pool.persist = persist;
        Ok(pool)
    }

    pub(crate) fn alloc_frame(&self, class: PoolClass) -> Result<NvFrameId> {
        if class != PoolClass::Metadata {
            return_errno_with_message!(Errno::EINVAL, "data pages require alloc_data_page");
        }
        let (index, wake) = self.allocate_index(PoolClass::Metadata)?;
        if wake {
            self.wake_high_water();
        }
        NvFrameId::try_from(index).map_err(|_| Error::new(Errno::EOVERFLOW))
    }

    pub(crate) fn alloc_data_page(&self) -> Result<NvPageId> {
        let (index, wake) = self.allocate_index(PoolClass::Data)?;
        if wake {
            self.wake_high_water();
        }
        NvPageId::try_from(index).map_err(|_| Error::new(Errno::EOVERFLOW))
    }

    /// Allocates a staged data page initialized with one full page of content.
    pub(crate) fn stage_data_page(&self, input: &[u8]) -> Result<NvPageId> {
        if input.len() != PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "NVPool data page content is not page-sized");
        }
        let id = self.alloc_data_page()?;
        if let Err(error) = self.backend.write_page(id, input) {
            self.free_data_page(id)
                .expect("freshly allocated data page must be freeable");
            return Err(error);
        }
        Ok(id)
    }

    pub(crate) fn pin_frame(self: &Arc<Self>, id: NvFrameId) -> Result<PinnedFrame> {
        let mut state = self.state.lock();
        let record = state
            .records
            .get(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if record.kind != ObjectKind::Metadata {
            return_errno_with_message!(Errno::EINVAL, "NVPool frame is not allocated metadata");
        }
        if record.persisting {
            return_errno_with_message!(Errno::EBUSY, "NVPool frame persistence is in progress");
        }
        let pins = record
            .pins
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        state.capacity.account_pin()?;
        state.records[id.index()].pins = pins;
        Ok(PinnedFrame {
            pool: self.clone(),
            id,
        })
    }

    /// Pins an allocated data page so its bytes remain stable until dropped.
    pub(crate) fn pin_data_page(self: &Arc<Self>, id: NvPageId) -> Result<PinnedDataPage> {
        self.pin_data_page_in_namespace(id, None)
    }

    pub(crate) fn pin_namespaced_data_page(
        self: &Arc<Self>,
        id: NvPageId,
        namespace: [u8; 16],
    ) -> Result<PinnedDataPage> {
        self.pin_data_page_in_namespace(id, Some(namespace))
    }

    fn pin_data_page_in_namespace(
        self: &Arc<Self>,
        id: NvPageId,
        namespace: Option<[u8; 16]>,
    ) -> Result<PinnedDataPage> {
        let mut state = self.state.lock();
        let record = state
            .records
            .get(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if !matches!(record.kind, ObjectKind::Data(_)) {
            return_errno_with_message!(Errno::EINVAL, "NVPool page is not allocated data");
        }
        if namespace
            .is_some_and(|namespace| record.key.is_none_or(|key| key.namespace() != namespace))
        {
            return_errno_with_message!(Errno::EINVAL, "NVPool data page namespace mismatch");
        }
        if record.persisting {
            return_errno_with_message!(Errno::EBUSY, "NVPool data page persistence is in progress");
        }
        let pins = record
            .pins
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        state.capacity.account_pin()?;
        state.records[id.index()].pins = pins;
        Ok(PinnedDataPage {
            pool: self.clone(),
            id,
        })
    }

    pub(crate) fn free_frame(&self, id: NvFrameId) -> Result<()> {
        self.free_index(id.index(), PoolClass::Metadata)
    }

    pub(crate) fn free_data_page(&self, id: NvPageId) -> Result<()> {
        let mut state = self.state.lock();
        if state.records[id.index()].committed {
            return_errno_with_message!(Errno::EBUSY, "NVPool data page is still committed");
        }
        let record = state
            .records
            .get_mut(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let ObjectKind::Data(data_state) = record.kind else {
            return_errno_with_message!(Errno::EINVAL, "NVPool data page is not allocated");
        };
        if record.pins != 0 || !data_state.can_free() {
            return_errno_with_message!(Errno::EBUSY, "NVPool data page cannot be freed");
        }
        record.kind = ObjectKind::Free;
        state.free_ids.push(id.index());
        state.capacity.account_free(PoolClass::Data);
        Ok(())
    }

    pub(crate) fn abort_data_page(&self, id: NvPageId) -> Result<()> {
        let mut state = self.state.lock();
        let record = state
            .records
            .get(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if record.committed || record.pins != 0 || record.persisting {
            return_errno_with_message!(Errno::EBUSY, "NVPool data page cannot be aborted");
        }
        if !matches!(
            record.kind,
            ObjectKind::Data(DataPageState::Staged | DataPageState::NvDirty)
        ) {
            return_errno_with_message!(Errno::EINVAL, "NVPool data page is not abortable");
        }
        state.records[id.index()] = ObjectRecord::free();
        state.free_ids.push(id.index());
        state.capacity.account_free(PoolClass::Data);
        Ok(())
    }

    pub(crate) fn data_page_state(&self, id: NvPageId) -> Result<DataPageState> {
        let state = self.state.lock();
        let record = state
            .records
            .get(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        match record.kind {
            ObjectKind::Free => Ok(DataPageState::Free),
            ObjectKind::Data(data_state) => Ok(data_state),
            ObjectKind::Metadata => {
                return_errno_with_message!(Errno::EINVAL, "NVPool object is metadata")
            }
        }
    }

    pub(crate) fn transition_data_page(
        &self,
        id: NvPageId,
        expected: DataPageState,
        next: DataPageState,
    ) -> Result<()> {
        if !expected.can_transition_to(next) {
            return_errno_with_message!(Errno::EINVAL, "invalid NVPool data page transition");
        }

        let mut state = self.state.lock();
        let record = state
            .records
            .get_mut(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if record.kind != ObjectKind::Data(expected) {
            return_errno_with_message!(Errno::EBUSY, "NVPool data page state changed");
        }
        record.kind = ObjectKind::Data(next);
        Ok(())
    }

    pub(crate) fn stats(&self) -> NvPoolStats {
        self.state.lock().capacity.stats()
    }

    pub(crate) fn resident_frame(&self, home_block: u64) -> Option<NvFrameId> {
        self.resident_meta_frame(MetaKey::legacy(home_block))
    }

    pub(crate) fn resident_meta_frame(&self, key: MetaKey) -> Option<NvFrameId> {
        let state = self.state.lock();
        let index = state.resident.get(&key)?.index;
        (state.records[index].kind == ObjectKind::Metadata)
            .then(|| NvFrameId::try_from(index).ok())
            .flatten()
    }

    pub(crate) fn resident_metadata(&self, namespace: [u8; 16]) -> Vec<(u64, NvFrameId)> {
        self.state
            .lock()
            .resident
            .iter()
            .filter_map(|(key, record)| {
                (key.namespace() == namespace).then(|| {
                    (
                        key.home_block(),
                        NvFrameId::try_from(record.index)
                            .expect("resident metadata index must fit NvFrameId"),
                    )
                })
            })
            .collect()
    }

    pub(crate) fn resident_data_page(&self, home_block: u64) -> Option<NvPageId> {
        let state = self.state.lock();
        state
            .records
            .iter()
            .enumerate()
            .find_map(|(index, record)| {
                (record.committed
                    && matches!(record.kind, ObjectKind::Data(_))
                    && record.key == Some(MetaKey::legacy(home_block)))
                .then(|| NvPageId::try_from(index).ok())
                .flatten()
            })
    }

    pub(crate) fn resident_data_pages(&self) -> Vec<(u64, NvPageId)> {
        let state = self.state.lock();
        state
            .records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                (record.committed && matches!(record.kind, ObjectKind::Data(_))).then(|| {
                    (
                        record
                            .key
                            .expect("committed data page has a key")
                            .home_block(),
                        NvPageId::try_from(index).expect("data page index must fit NvPageId"),
                    )
                })
            })
            .collect()
    }

    pub(crate) fn committed_data_pages(&self, namespace: [u8; 16]) -> Vec<(u64, NvPageId)> {
        let state = self.state.lock();
        state
            .records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                let key = record.key?;
                (record.committed
                    && matches!(record.kind, ObjectKind::Data(_))
                    && key.namespace() == namespace)
                    .then(|| {
                        (
                            key.home_block(),
                            NvPageId::try_from(index).expect("data page index must fit NvPageId"),
                        )
                    })
            })
            .collect()
    }

    pub(crate) fn data_page_home(&self, id: NvPageId) -> Result<u64> {
        Ok(self.data_page_key(id)?.home_block())
    }

    pub(crate) fn data_page_key(&self, id: NvPageId) -> Result<MetaKey> {
        let state = self.state.lock();
        let record = state
            .records
            .get(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if !matches!(record.kind, ObjectKind::Data(_)) || !record.committed {
            return_errno_with_message!(Errno::EINVAL, "NVPool data page is not committed");
        }
        record.key.ok_or_else(|| Error::new(Errno::EIO))
    }

    pub(crate) fn commit_frame(
        &self,
        id: NvFrameId,
        home_block: u64,
        slot_bitmap: u64,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        self.commit_metadata_frame(id, MetaKey::legacy(home_block), slot_bitmap, fault, false)
            .map(|_| ())
    }

    pub(crate) fn replace_metadata_frame(
        &self,
        id: NvFrameId,
        key: MetaKey,
        slot_bitmap: u64,
        fault: Option<FaultPoint>,
    ) -> Result<Option<NvFrameId>> {
        self.commit_metadata_frame(id, key, slot_bitmap, fault, true)
    }

    fn commit_metadata_frame(
        &self,
        id: NvFrameId,
        key: MetaKey,
        slot_bitmap: u64,
        fault: Option<FaultPoint>,
        replace: bool,
    ) -> Result<Option<NvFrameId>> {
        let (version, old) = self.begin_commit(id.index(), ObjectKind::Metadata, key, replace)?;
        let result = self.persist.commit_frame(
            id.index(),
            PoolClass::Metadata,
            key.namespace,
            key.home_block,
            version,
            slot_bitmap,
            fault,
        );
        let mut state = self.state.lock();
        state.records[id.index()].pins -= 1;
        state.records[id.index()].persisting = false;
        state.capacity.account_unpin();
        if result.is_ok() {
            state.records[id.index()].committed = true;
            state.records[id.index()].key = Some(key);
            state.records[id.index()].version = version;
            state.resident.insert(
                key,
                ResidentRecord {
                    index: id.index(),
                    version,
                },
            );
        }
        result.map(|_| old.and_then(|index| NvFrameId::try_from(index).ok()))
    }

    pub(crate) fn commit_data_page(
        &self,
        id: NvPageId,
        home_block: u64,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        self.commit_data_page_inner(id, home_block, fault, false)
            .map(|_| ())
    }

    pub(crate) fn replace_data_page(
        &self,
        id: NvPageId,
        home_block: u64,
        fault: Option<FaultPoint>,
    ) -> Result<Option<NvPageId>> {
        self.commit_data_page_inner(id, home_block, fault, true)
    }

    pub(crate) fn commit_namespaced_data_page(
        &self,
        id: NvPageId,
        key: MetaKey,
        fault: Option<FaultPoint>,
    ) -> Result<Option<NvPageId>> {
        self.commit_unindexed_data_page(id, key, fault)
    }

    fn commit_unindexed_data_page(
        &self,
        id: NvPageId,
        key: MetaKey,
        fault: Option<FaultPoint>,
    ) -> Result<Option<NvPageId>> {
        {
            let mut state = self.state.lock();
            Self::pin_persistent_update(
                &mut state,
                id.index(),
                ObjectKind::Data(DataPageState::NvDirty),
            )?;
        }
        let result = self.persist.commit_frame(
            id.index(),
            PoolClass::Data,
            key.namespace,
            key.home_block,
            1,
            1,
            fault,
        );
        let mut state = self.state.lock();
        state.records[id.index()].pins -= 1;
        state.records[id.index()].persisting = false;
        state.capacity.account_unpin();
        if result.is_ok() {
            state.records[id.index()].committed = true;
            state.records[id.index()].key = Some(key);
            state.records[id.index()].version = 1;
        }
        result.map(|_| None)
    }

    fn commit_data_page_inner(
        &self,
        id: NvPageId,
        home_block: u64,
        fault: Option<FaultPoint>,
        replace: bool,
    ) -> Result<Option<NvPageId>> {
        self.commit_data_page_with_key(id, MetaKey::legacy(home_block), fault, replace)
    }

    fn commit_data_page_with_key(
        &self,
        id: NvPageId,
        key: MetaKey,
        fault: Option<FaultPoint>,
        replace: bool,
    ) -> Result<Option<NvPageId>> {
        let (version, old) = self.begin_commit(
            id.index(),
            ObjectKind::Data(DataPageState::NvDirty),
            key,
            replace,
        )?;
        let result = self.persist.commit_frame(
            id.index(),
            PoolClass::Data,
            key.namespace,
            key.home_block,
            version,
            1,
            fault,
        );
        let mut state = self.state.lock();
        state.records[id.index()].pins -= 1;
        state.records[id.index()].persisting = false;
        state.capacity.account_unpin();
        if result.is_ok() {
            state.records[id.index()].committed = true;
            state.records[id.index()].key = Some(key);
            state.records[id.index()].version = version;
            state.resident.insert(
                key,
                ResidentRecord {
                    index: id.index(),
                    version,
                },
            );
        }
        result.map(|_| old.and_then(|index| NvPageId::try_from(index).ok()))
    }

    pub(crate) fn clear_frame(&self, id: NvFrameId, fault: Option<FaultPoint>) -> Result<()> {
        self.clear_committed(id.index(), ObjectKind::Metadata, None, fault)
    }

    /// Acquires the only pin on a committed metadata frame before SSD eviction.
    ///
    /// The returned guard keeps new readers out until the caller either clears
    /// the durable commit word with [`EvictingFrame::finish`] or drops the guard
    /// to abort the eviction.
    pub(crate) fn begin_frame_eviction(self: &Arc<Self>, id: NvFrameId) -> Result<EvictingFrame> {
        let mut state = self.state.lock();
        let record = state
            .records
            .get(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if record.kind != ObjectKind::Metadata {
            return_errno_with_message!(Errno::EINVAL, "NVPool frame is not allocated metadata");
        }
        if !record.committed {
            return_errno_with_message!(Errno::EINVAL, "NVPool frame is not committed");
        }
        if record.pins != 0 || record.persisting {
            return_errno_with_message!(Errno::EBUSY, "NVPool frame is pinned");
        }
        state.capacity.account_pin()?;
        state.records[id.index()].pins = 1;
        state.records[id.index()].persisting = true;
        Ok(EvictingFrame {
            pool: self.clone(),
            id,
            active: true,
        })
    }

    /// Acquires the only pin on a committed dirty data page before SSD drain.
    ///
    /// The caller may read the page while the guard is active, then clear its
    /// durable residency with [`EvictingDataPage::finish`]. Dropping the guard
    /// leaves the committed data page resident and dirty.
    pub(crate) fn begin_data_page_eviction(
        self: &Arc<Self>,
        id: NvPageId,
    ) -> Result<EvictingDataPage> {
        let mut state = self.state.lock();
        let record = state
            .records
            .get(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if record.kind != ObjectKind::Data(DataPageState::NvDirty) {
            return_errno_with_message!(Errno::EINVAL, "NVPool data page is not dirty");
        }
        if !record.committed {
            return_errno_with_message!(Errno::EINVAL, "NVPool data page is not committed");
        }
        if record.pins != 0 || record.persisting {
            return_errno_with_message!(Errno::EBUSY, "NVPool data page is pinned");
        }
        state.capacity.account_pin()?;
        let record = &mut state.records[id.index()];
        record.kind = ObjectKind::Data(DataPageState::InFlight);
        record.pins = 1;
        record.persisting = true;
        Ok(EvictingDataPage {
            pool: self.clone(),
            id,
            active: true,
        })
    }

    pub(crate) fn clear_data_page(&self, id: NvPageId, fault: Option<FaultPoint>) -> Result<()> {
        self.clear_committed(
            id.index(),
            ObjectKind::Data(DataPageState::NvDirty),
            Some(ObjectKind::Data(DataPageState::Synced)),
            fault,
        )
    }

    fn clear_committed(
        &self,
        index: usize,
        expected: ObjectKind,
        next: Option<ObjectKind>,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        self.begin_clear(index, expected)?;
        let result = self.persist.clear_frame(index, fault);
        let mut state = self.state.lock();
        state.records[index].pins -= 1;
        state.records[index].persisting = false;
        state.capacity.account_unpin();
        if result.is_ok() {
            if let Some(key) = state.records[index].key
                && state.resident.get(&key).map(|entry| entry.index) == Some(index)
            {
                state.resident.remove(&key);
            }
            state.records[index].committed = false;
            state.records[index].key = None;
            state.records[index].version = 0;
            if let Some(next) = next {
                state.records[index].kind = next;
            }
        }
        result
    }

    fn allocate_index(&self, class: PoolClass) -> Result<(usize, bool)> {
        let mut state = self.state.lock();
        state.capacity.before_allocate(class)?;
        let index = state
            .free_ids
            .pop()
            .ok_or_else(|| Error::new(Errno::ENOSPC))?;
        let record = &mut state.records[index];
        debug_assert_eq!(record.kind, ObjectKind::Free);
        record.kind = match class {
            PoolClass::Metadata => ObjectKind::Metadata,
            PoolClass::Data => ObjectKind::Data(DataPageState::Staged),
        };
        record.persisting = false;
        record.committed = false;
        record.key = None;
        record.version = 0;
        Ok((index, state.capacity.account_allocate(class)))
    }

    fn free_index(&self, index: usize, class: PoolClass) -> Result<()> {
        let mut state = self.state.lock();
        if state.records[index].committed {
            return_errno_with_message!(Errno::EBUSY, "NVPool frame is still committed");
        }
        let record = state
            .records
            .get_mut(index)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let expected = match class {
            PoolClass::Metadata => ObjectKind::Metadata,
            PoolClass::Data => unreachable!(),
        };
        if record.kind != expected {
            return_errno_with_message!(Errno::EINVAL, "NVPool frame is not allocated");
        }
        if record.pins != 0 || record.persisting {
            return_errno_with_message!(Errno::EBUSY, "NVPool frame is pinned");
        }
        record.kind = ObjectKind::Free;
        state.free_ids.push(index);
        state.capacity.account_free(class);
        Ok(())
    }

    fn begin_commit(
        &self,
        index: usize,
        expected: ObjectKind,
        key: MetaKey,
        replace: bool,
    ) -> Result<(u64, Option<usize>)> {
        let mut state = self.state.lock();
        if state.records[index].committed {
            return_errno_with_message!(Errno::EBUSY, "NVPool object is already committed");
        }
        let old = state.resident.get(&key).copied();
        if old.is_some() && !replace {
            return_errno_with_message!(Errno::EBUSY, "NVPool home already has a committed version");
        }
        let version = old
            .map(|entry| {
                entry
                    .version
                    .checked_add(1)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))
            })
            .transpose()?
            .unwrap_or(1);
        Self::pin_persistent_update(&mut state, index, expected)?;
        Ok((version, old.map(|entry| entry.index)))
    }

    fn begin_clear(&self, index: usize, expected: ObjectKind) -> Result<()> {
        let mut state = self.state.lock();
        if !state.records[index].committed {
            return_errno_with_message!(Errno::EINVAL, "NVPool object is not committed");
        }
        Self::pin_persistent_update(&mut state, index, expected)
    }

    fn pin_persistent_update(
        state: &mut PoolState,
        index: usize,
        expected: ObjectKind,
    ) -> Result<()> {
        let record = state
            .records
            .get(index)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if record.kind != expected {
            return_errno_with_message!(Errno::EINVAL, "NVPool object has the wrong class");
        }
        if record.persisting {
            return_errno_with_message!(
                Errno::EBUSY,
                "NVPool object persistence is already in progress"
            );
        }
        let pins = record
            .pins
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        state.capacity.account_pin()?;
        state.records[index].pins = pins;
        state.records[index].persisting = true;
        Ok(())
    }

    fn write_pinned_frame(&self, id: NvFrameId, input: &[u8]) -> Result<()> {
        let state = self.state.lock();
        let record = state
            .records
            .get(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if record.kind != ObjectKind::Metadata
            || record.pins == 0
            || record.persisting
            || state.records[id.index()].committed
        {
            return_errno_with_message!(Errno::EBUSY, "NVPool frame is not writable");
        }
        self.backend.write_page(self.page_id(id), input)
    }

    fn write_pinned_data_page(&self, id: NvPageId, input: &[u8]) -> Result<()> {
        let state = self.state.lock();
        let record = state
            .records
            .get(id.index())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if record.kind != ObjectKind::Data(DataPageState::Staged)
            || record.pins == 0
            || record.persisting
            || record.committed
        {
            return_errno_with_message!(Errno::EBUSY, "NVPool data page is not writable");
        }
        self.backend.write_page(id, input)
    }

    fn page_id(&self, id: NvFrameId) -> NvPageId {
        NvPageId::try_from(id.index()).unwrap()
    }

    fn unpin_frame(&self, id: NvFrameId) {
        let mut state = self.state.lock();
        let record = &mut state.records[id.index()];
        debug_assert_eq!(record.kind, ObjectKind::Metadata);
        debug_assert!(record.pins > 0);
        record.pins -= 1;
        state.capacity.account_unpin();
    }

    fn unpin_data_page(&self, id: NvPageId) {
        let mut state = self.state.lock();
        let record = &mut state.records[id.index()];
        debug_assert!(matches!(record.kind, ObjectKind::Data(_)));
        debug_assert!(record.pins > 0);
        record.pins -= 1;
        state.capacity.account_unpin();
    }

    fn abort_frame_eviction(&self, id: NvFrameId) {
        let mut state = self.state.lock();
        let record = &mut state.records[id.index()];
        debug_assert_eq!(record.kind, ObjectKind::Metadata);
        debug_assert_eq!(record.pins, 1);
        debug_assert!(record.persisting);
        record.pins = 0;
        record.persisting = false;
        state.capacity.account_unpin();
    }

    fn abort_data_page_eviction(&self, id: NvPageId) {
        let mut state = self.state.lock();
        let record = &mut state.records[id.index()];
        debug_assert_eq!(record.kind, ObjectKind::Data(DataPageState::InFlight));
        debug_assert_eq!(record.pins, 1);
        debug_assert!(record.persisting);
        record.kind = ObjectKind::Data(DataPageState::NvDirty);
        record.pins = 0;
        record.persisting = false;
        state.capacity.account_unpin();
    }

    fn wake_high_water(&self) {
        if let Some(callback) = &self.high_water_wake {
            callback();
        }
    }
}

pub(crate) struct PinnedFrame {
    pool: Arc<NvPool>,
    id: NvFrameId,
}

impl PinnedFrame {
    pub(crate) fn id(&self) -> NvFrameId {
        self.id
    }

    pub(crate) fn read(&self, output: &mut [u8]) -> Result<()> {
        self.pool.backend.read_page(self.page_id(), output)
    }

    pub(crate) fn write(&self, input: &[u8]) -> Result<()> {
        self.pool.write_pinned_frame(self.id, input)
    }

    fn page_id(&self) -> NvPageId {
        NvPageId::try_from(self.id.index()).unwrap()
    }
}

impl Drop for PinnedFrame {
    fn drop(&mut self) {
        self.pool.unpin_frame(self.id);
    }
}

/// A pinned data page whose bytes remain stable for the lifetime of the
/// handle.
pub(crate) struct PinnedDataPage {
    pool: Arc<NvPool>,
    id: NvPageId,
}

impl PinnedDataPage {
    pub(crate) fn id(&self) -> NvPageId {
        self.id
    }

    pub(crate) fn read(&self, output: &mut [u8]) -> Result<()> {
        self.pool.backend.read_page(self.id, output)
    }

    /// Writes a staged page before it is made NV-dirty or committed.
    pub(crate) fn write(&self, input: &[u8]) -> Result<()> {
        self.pool.write_pinned_data_page(self.id, input)
    }
}

impl Drop for PinnedDataPage {
    fn drop(&mut self) {
        self.pool.unpin_data_page(self.id);
    }
}

pub(crate) struct EvictingFrame {
    pool: Arc<NvPool>,
    id: NvFrameId,
    active: bool,
}

impl EvictingFrame {
    pub(crate) fn read(&self, output: &mut [u8]) -> Result<()> {
        self.pool
            .backend
            .read_page(self.pool.page_id(self.id), output)
    }

    pub(crate) fn finish(mut self, fault: Option<FaultPoint>) -> Result<()> {
        self.pool.persist.clear_frame(self.id.index(), fault)?;
        let mut state = self.pool.state.lock();
        let record = &mut state.records[self.id.index()];
        debug_assert_eq!(record.kind, ObjectKind::Metadata);
        debug_assert_eq!(record.pins, 1);
        debug_assert!(record.persisting);
        record.pins = 0;
        record.persisting = false;
        state.capacity.account_unpin();
        if let Some(key) = state.records[self.id.index()].key
            && state.resident.get(&key).map(|entry| entry.index) == Some(self.id.index())
        {
            state.resident.remove(&key);
        }
        state.records[self.id.index()].committed = false;
        state.records[self.id.index()].key = None;
        state.records[self.id.index()].version = 0;
        self.active = false;
        Ok(())
    }
}

impl Debug for EvictingFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EvictingFrame")
            .field("id", &self.id)
            .finish()
    }
}

impl Drop for EvictingFrame {
    fn drop(&mut self) {
        if self.active {
            self.pool.abort_frame_eviction(self.id);
        }
    }
}

pub(crate) struct EvictingDataPage {
    pool: Arc<NvPool>,
    id: NvPageId,
    active: bool,
}

impl EvictingDataPage {
    pub(crate) fn read(&self, output: &mut [u8]) -> Result<()> {
        self.pool.backend.read_page(self.id, output)
    }

    pub(crate) fn finish(mut self, fault: Option<FaultPoint>) -> Result<()> {
        self.pool.persist.clear_frame(self.id.index(), fault)?;
        let mut state = self.pool.state.lock();
        let record = &mut state.records[self.id.index()];
        debug_assert_eq!(record.kind, ObjectKind::Data(DataPageState::InFlight));
        debug_assert_eq!(record.pins, 1);
        debug_assert!(record.persisting);
        record.kind = ObjectKind::Data(DataPageState::Synced);
        record.pins = 0;
        record.persisting = false;
        state.capacity.account_unpin();
        if let Some(key) = state.records[self.id.index()].key
            && state.resident.get(&key).map(|entry| entry.index) == Some(self.id.index())
        {
            state.resident.remove(&key);
        }
        state.records[self.id.index()].committed = false;
        state.records[self.id.index()].key = None;
        state.records[self.id.index()].version = 0;
        self.active = false;
        Ok(())
    }
}

impl Debug for EvictingDataPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EvictingDataPage")
            .field("id", &self.id)
            .finish()
    }
}

impl Drop for EvictingDataPage {
    fn drop(&mut self) {
        if self.active {
            self.pool.abort_data_page_eviction(self.id);
        }
    }
}
