// SPDX-License-Identifier: MPL-2.0

//! Epoch-local state for authority-aware background reclamation.

use core::ops::Range;

use crate::prelude::*;

/// Allocation-domain object tracked by one reclaim epoch.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum ObjectId {
    Block(u64),
    Inode(u32),
}

struct TouchedObjects(SpinLock<BTreeSet<ObjectId>>);

impl Default for TouchedObjects {
    fn default() -> Self {
        Self(SpinLock::new(BTreeSet::new()))
    }
}

/// Connects foreground allocation/free paths to the currently active epoch.
pub(crate) struct ReclaimTracker {
    active: SpinLock<Weak<TouchedObjects>>,
    mutation_gate: Arc<Mutex<()>>,
}

impl ReclaimTracker {
    pub(crate) fn new() -> Self {
        Self {
            active: SpinLock::new(Weak::new()),
            mutation_gate: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn begin(&self, allocated: BTreeSet<ObjectId>) -> Result<ReclaimEpoch> {
        let mut active = self.active.lock();
        if active.upgrade().is_some() {
            return_errno_with_message!(Errno::EBUSY, "a reclaim epoch is already active");
        }
        let touched = Arc::new(TouchedObjects::default());
        *active = Arc::downgrade(&touched);
        Ok(ReclaimEpoch {
            allocated,
            visited: BTreeSet::new(),
            touched,
            mutation_gate: self.mutation_gate.clone(),
        })
    }

    /// Serializes one allocator mutation with sweep candidate claiming.
    pub(crate) fn lock_mutation(&self) -> MutexGuard<'_, ()> {
        self.mutation_gate.lock()
    }

    pub(crate) fn touch(&self, object: ObjectId) {
        if let Some(active) = self.active.lock().upgrade() {
            active.0.lock().insert(object);
        }
    }

    pub(crate) fn touch_blocks(&self, blocks: Range<u64>) {
        if let Some(active) = self.active.lock().upgrade() {
            active.0.lock().extend(blocks.map(ObjectId::Block));
        }
    }
}

/// Stable allocation snapshot plus mark and foreground-interference state.
pub(crate) struct ReclaimEpoch {
    allocated: BTreeSet<ObjectId>,
    visited: BTreeSet<ObjectId>,
    touched: Arc<TouchedObjects>,
    mutation_gate: Arc<Mutex<()>>,
}

impl ReclaimEpoch {
    pub(crate) fn visit(&mut self, object: ObjectId) -> bool {
        self.visited.insert(object)
    }

    pub(crate) fn sweep_candidates(&self, pinned: &BTreeSet<ObjectId>) -> Vec<ObjectId> {
        self.allocated
            .iter()
            .filter(|object| !self.visited.contains(object))
            .filter(|object| !pinned.contains(object))
            .copied()
            .collect()
    }

    /// Revalidates one candidate and performs its bitmap transition while
    /// foreground allocation/free is excluded.
    pub(crate) fn reclaim_if_untouched(
        &self,
        object: ObjectId,
        reclaim: impl FnOnce() -> Result<bool>,
    ) -> Result<bool> {
        let _mutation = self.mutation_gate.lock();
        let mut touched = self.touched.0.lock();
        if touched.contains(&object) {
            return Ok(false);
        }
        touched.insert(object);
        reclaim()
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn touched_object_is_not_reclaimed_after_aba_reallocation() {
        let tracker = ReclaimTracker::new();
        let epoch = tracker
            .begin([ObjectId::Block(7)].into_iter().collect())
            .unwrap();

        tracker.touch(ObjectId::Block(7));

        let candidate = epoch.sweep_candidates(&BTreeSet::new())[0];
        assert!(!epoch.reclaim_if_untouched(candidate, || Ok(true)).unwrap());

        drop(epoch);
        let epoch = tracker
            .begin([ObjectId::Block(7)].into_iter().collect())
            .unwrap();
        let candidate = epoch.sweep_candidates(&BTreeSet::new())[0];
        assert!(epoch.reclaim_if_untouched(candidate, || Ok(true)).unwrap());
    }

    #[ktest]
    fn touch_after_candidate_collection_prevents_bitmap_transition() {
        let tracker = ReclaimTracker::new();
        let epoch = tracker
            .begin([ObjectId::Block(7)].into_iter().collect())
            .unwrap();
        let candidate = epoch.sweep_candidates(&BTreeSet::new())[0];
        {
            let _mutation = tracker.lock_mutation();
            tracker.touch(candidate);
        }
        let mut reclaimed = false;

        assert!(
            !epoch
                .reclaim_if_untouched(candidate, || {
                    reclaimed = true;
                    Ok(true)
                })
                .unwrap()
        );
        assert!(!reclaimed);
    }

    #[ktest]
    fn visited_and_pinned_objects_are_excluded_from_sweep() {
        let tracker = ReclaimTracker::new();
        let mut epoch = tracker
            .begin(
                [ObjectId::Block(3), ObjectId::Block(4), ObjectId::Inode(11)]
                    .into_iter()
                    .collect(),
            )
            .unwrap();
        epoch.visit(ObjectId::Block(3));
        let pinned = [ObjectId::Block(4)].into_iter().collect();

        assert_eq!(epoch.sweep_candidates(&pinned), vec![ObjectId::Inode(11)]);
    }

    #[ktest]
    fn mutation_gate_allows_sleeping_reclaim_work() {
        let tracker = ReclaimTracker::new();
        let _mutation = tracker.lock_mutation();

        ostd::task::atomic_mode::might_sleep();
    }
}
