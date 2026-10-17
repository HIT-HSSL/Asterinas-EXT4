// SPDX-License-Identifier: MPL-2.0

//! In-memory extent status index for one inode.

use alloc::{collections::BTreeMap, vec::Vec};
use core::{
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

use ostd::sync::{RwMutex, SpinLock};

use super::super::{Ext4Bid, Iblock};

/// The in-memory state of a contiguous logical-block range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ExtentStatus {
    Written { pblock: Ext4Bid },
    Unwritten { pblock: Ext4Bid },
    Delayed,
    Hole,
}

impl ExtentStatus {
    fn shifted(self, blocks: Iblock) -> Self {
        match self {
            Self::Written { pblock } => Self::Written {
                pblock: pblock + Ext4Bid::from(blocks),
            },
            Self::Unwritten { pblock } => Self::Unwritten {
                pblock: pblock + Ext4Bid::from(blocks),
            },
            Self::Delayed => Self::Delayed,
            Self::Hole => Self::Hole,
        }
    }

    fn can_follow(self, previous: Self, previous_len: Iblock) -> bool {
        match (previous, self) {
            (Self::Written { pblock: left }, Self::Written { pblock: right })
            | (Self::Unwritten { pblock: left }, Self::Unwritten { pblock: right }) => {
                left + Ext4Bid::from(previous_len) == right
            }
            (Self::Delayed, Self::Delayed) | (Self::Hole, Self::Hole) => true,
            _ => false,
        }
    }

    fn is_reclaimable(self) -> bool {
        !matches!(self, Self::Delayed)
    }
}

/// A status extent keyed by its first logical block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ExtentStatusEntry {
    start: Iblock,
    end: Iblock,
    status: ExtentStatus,
    referenced: bool,
}

impl ExtentStatusEntry {
    fn new(range: Range<Iblock>, status: ExtentStatus) -> Self {
        debug_assert!(range.start < range.end);
        Self {
            start: range.start,
            end: range.end,
            status,
            referenced: false,
        }
    }

    pub(super) fn range(self) -> Range<Iblock> {
        self.start..self.end
    }

    pub(super) fn status_at(self, iblock: Iblock) -> ExtentStatus {
        debug_assert!(self.start <= iblock && iblock < self.end);
        self.status.shifted(iblock - self.start)
    }

    fn fragment(self, range: Range<Iblock>) -> Self {
        debug_assert!(self.start <= range.start && range.end <= self.end);
        Self::new(range.clone(), self.status.shifted(range.start - self.start))
    }
}

#[derive(Default)]
struct ExtentStatusTreeInner {
    entries: BTreeMap<Iblock, ExtentStatusEntry>,
}

/// A volatile per-inode index of mapped, delayed, and known-hole ranges.
pub(super) struct ExtentStatusTree {
    inner: RwMutex<ExtentStatusTreeInner>,
    recent: SpinLock<Option<ExtentStatusEntry>>,
    version: AtomicU64,
}

impl ExtentStatusTree {
    pub(super) fn new() -> Self {
        Self {
            inner: RwMutex::new(ExtentStatusTreeInner::default()),
            recent: SpinLock::new(None),
            version: AtomicU64::new(0),
        }
    }

    pub(super) fn lookup(&self, iblock: Iblock) -> Option<ExtentStatusEntry> {
        let inner = self.inner.read();
        if let Some(entry) = *self.recent.lock()
            && entry.start <= iblock
            && iblock < entry.end
        {
            return Some(entry);
        }
        let found = inner
            .entries
            .range(..=iblock)
            .next_back()
            .map(|(_, entry)| *entry)
            .filter(|entry| iblock < entry.end);
        let observed_version = self.version.load(Ordering::Acquire);
        drop(inner);

        let Some(found) = found else {
            *self.recent.lock() = None;
            return None;
        };
        let mut inner = self.inner.write();
        if self.version.load(Ordering::Acquire) == observed_version
            && let Some(entry) = inner.entries.get_mut(&found.start)
            && entry.end == found.end
            && entry.status == found.status
        {
            entry.referenced = true;
            let found = *entry;
            *self.recent.lock() = Some(found);
            return Some(found);
        }
        Some(found)
    }

    pub(super) fn insert(&self, range: Range<Iblock>, status: ExtentStatus) {
        if range.start >= range.end {
            return;
        }

        let mut inner = self.inner.write();
        Self::insert_locked(&mut inner, range, status);
        self.bump_version();
    }

    /// Inserts a disk-lookup result only if the tree did not change while the
    /// caller performed that lookup.
    pub(super) fn insert_if_version(
        &self,
        expected_version: u64,
        range: Range<Iblock>,
        status: ExtentStatus,
    ) -> bool {
        if range.start >= range.end {
            return true;
        }

        let mut inner = self.inner.write();
        if self.version.load(Ordering::Acquire) != expected_version {
            return false;
        }
        Self::insert_locked(&mut inner, range, status);
        self.bump_version();
        true
    }

    /// Takes a versioned snapshot of known holes. Holding the read lock while
    /// reading the version makes the ranges and version one consistent view.
    pub(super) fn known_holes_snapshot(&self, range: Range<Iblock>) -> (u64, Vec<Range<Iblock>>) {
        let inner = self.inner.read();
        let holes = Self::ranges_with_status_locked(&inner, range, ExtentStatus::Hole);
        let version = self.version.load(Ordering::Acquire);
        (version, holes)
    }

    /// Converts exactly the snapshotted holes to delayed extents if no ES
    /// mutation occurred since the snapshot was taken.
    pub(super) fn mark_holes_delayed_if_version(
        &self,
        expected_version: u64,
        holes: &[Range<Iblock>],
    ) -> bool {
        if holes.is_empty() {
            return true;
        }

        let mut inner = self.inner.write();
        if self.version.load(Ordering::Acquire) != expected_version {
            return false;
        }
        for hole in holes {
            Self::insert_locked(&mut inner, hole.clone(), ExtentStatus::Delayed);
        }
        self.bump_version();
        true
    }

    pub(super) fn delayed_ranges(&self, range: Range<Iblock>) -> Vec<Range<Iblock>> {
        self.ranges_with_status(range, ExtentStatus::Delayed)
    }

    /// Intersects sorted page runs with delayed extents. Dense batches scan the
    /// enclosing ES range once; sparse batches seek directly to each page run.
    pub(super) fn delayed_intersections(&self, page_runs: &[Range<Iblock>]) -> Vec<Range<Iblock>> {
        let (Some(first), Some(last)) = (page_runs.first(), page_runs.last()) else {
            return Vec::new();
        };
        let inner = self.inner.read();
        if !Self::use_envelope_scan(inner.entries.len(), page_runs.len()) {
            let mut intersections = Vec::new();
            for run in page_runs {
                intersections.extend(Self::ranges_with_status_locked(
                    &inner,
                    run.clone(),
                    ExtentStatus::Delayed,
                ));
            }
            return intersections;
        }

        let delayed =
            Self::ranges_with_status_locked(&inner, first.start..last.end, ExtentStatus::Delayed);
        let mut intersections = Vec::new();
        let mut delayed_idx = 0;
        for run in page_runs {
            while delayed_idx < delayed.len() && delayed[delayed_idx].end <= run.start {
                delayed_idx += 1;
            }
            let mut idx = delayed_idx;
            while idx < delayed.len() && delayed[idx].start < run.end {
                let start = run.start.max(delayed[idx].start);
                let end = run.end.min(delayed[idx].end);
                if start < end {
                    intersections.push(start..end);
                }
                if delayed[idx].end > run.end {
                    break;
                }
                idx += 1;
            }
            delayed_idx = idx;
        }
        intersections
    }

    const fn use_envelope_scan(entry_count: usize, run_count: usize) -> bool {
        entry_count <= run_count
    }

    /// Finds the first delayed extent intersecting `range`.
    pub(super) fn next_delayed_range(&self, range: Range<Iblock>) -> Option<Range<Iblock>> {
        if range.start >= range.end {
            return None;
        }
        let inner = self.inner.read();
        if let Some((_, entry)) = inner.entries.range(..=range.start).next_back()
            && entry.status == ExtentStatus::Delayed
            && entry.end > range.start
        {
            return Some(range.start..entry.end.min(range.end));
        }
        inner
            .entries
            .range(range.start..range.end)
            .find_map(|(_, entry)| {
                (entry.status == ExtentStatus::Delayed)
                    .then_some(entry.start..entry.end.min(range.end))
            })
    }

    /// Replaces a delayed subrange with its allocated mapping.
    ///
    /// The caller uses the returned block count to verify that reservation and
    /// allocation state still agree before exposing the new mapping.
    pub(super) fn materialize_delayed(&self, range: Range<Iblock>, status: ExtentStatus) -> Iblock {
        debug_assert!(matches!(
            status,
            ExtentStatus::Written { .. } | ExtentStatus::Unwritten { .. }
        ));
        if range.start >= range.end {
            return 0;
        }

        let mut inner = self.inner.write();
        let delayed = Self::ranges_with_status_locked(&inner, range.clone(), ExtentStatus::Delayed);
        let materialized = delayed.iter().map(|part| part.end - part.start).sum();
        for part in delayed {
            Self::insert_locked(
                &mut inner,
                part.clone(),
                status.shifted(part.start - range.start),
            );
        }
        if materialized != 0 {
            self.bump_version();
        }
        materialized
    }

    fn ranges_with_status(
        &self,
        range: Range<Iblock>,
        expected: ExtentStatus,
    ) -> Vec<Range<Iblock>> {
        if range.start >= range.end {
            return Vec::new();
        }
        let inner = self.inner.read();
        Self::ranges_with_status_locked(&inner, range, expected)
    }

    fn ranges_with_status_locked(
        inner: &ExtentStatusTreeInner,
        range: Range<Iblock>,
        expected: ExtentStatus,
    ) -> Vec<Range<Iblock>> {
        let mut ranges = Vec::new();
        if let Some((_, entry)) = inner.entries.range(..range.start).next_back()
            && entry.status == expected
            && entry.end > range.start
        {
            ranges.push(range.start..entry.end.min(range.end));
        }
        ranges.extend(
            inner
                .entries
                .range(range.start..range.end)
                .filter_map(|(_, entry)| {
                    if entry.status != expected {
                        return None;
                    }
                    let end = entry.end.min(range.end);
                    (entry.start < end).then_some(entry.start..end)
                }),
        );
        ranges
    }

    pub(super) fn remove(&self, range: Range<Iblock>) {
        if range.start >= range.end {
            return;
        }

        let mut inner = self.inner.write();
        if !Self::remove_overlaps(&mut inner, &range) {
            return;
        }
        self.bump_version();
    }

    /// Invalidates cached facts before a mapping mutation.
    ///
    /// This always advances the version, even when the range was not cached,
    /// so an in-flight disk lookup cannot publish a stale result afterward.
    pub(super) fn invalidate(&self, range: Range<Iblock>) {
        if range.start >= range.end {
            return;
        }

        let mut inner = self.inner.write();
        Self::remove_overlaps(&mut inner, &range);
        self.bump_version();
    }

    /// Reclaims at most `limit` cache entries while preserving delayed state.
    pub(super) fn shrink(&self, limit: usize) -> usize {
        if limit == 0 {
            return 0;
        }

        let mut inner = self.inner.write();
        let mut victims = Vec::new();
        for (start, entry) in inner.entries.iter_mut() {
            if !entry.status.is_reclaimable() {
                continue;
            }
            if entry.referenced {
                entry.referenced = false;
                continue;
            }
            victims.push(*start);
            if victims.len() == limit {
                break;
            }
        }
        *self.recent.lock() = None;
        for start in &victims {
            inner.entries.remove(start);
        }
        if !victims.is_empty() {
            self.bump_version();
        }
        victims.len()
    }

    pub(super) fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    pub(super) fn node_count(&self) -> usize {
        self.inner.read().entries.len()
    }

    fn bump_version(&self) {
        *self.recent.lock() = None;
        self.version.fetch_add(1, Ordering::Release);
    }

    #[cfg(ktest)]
    fn recent_entry(&self) -> Option<ExtentStatusEntry> {
        *self.recent.lock()
    }

    fn insert_locked(
        inner: &mut ExtentStatusTreeInner,
        range: Range<Iblock>,
        status: ExtentStatus,
    ) {
        Self::remove_overlaps(inner, &range);
        inner
            .entries
            .insert(range.start, ExtentStatusEntry::new(range.clone(), status));
        Self::merge_around(inner, range.start);
    }

    fn remove_overlaps(inner: &mut ExtentStatusTreeInner, range: &Range<Iblock>) -> bool {
        let mut overlaps = Vec::new();
        if let Some((start, entry)) = inner.entries.range(..range.start).next_back()
            && entry.end > range.start
        {
            overlaps.push(*start);
        }
        overlaps.extend(
            inner
                .entries
                .range(range.start..range.end)
                .map(|(start, _)| *start),
        );

        if overlaps.is_empty() {
            return false;
        }

        for start in overlaps {
            let entry = inner.entries.remove(&start).unwrap();
            if entry.start < range.start {
                let left = entry.fragment(entry.start..range.start);
                inner.entries.insert(left.start, left);
            }
            if entry.end > range.end {
                let right = entry.fragment(range.end..entry.end);
                inner.entries.insert(right.start, right);
            }
        }
        true
    }

    fn merge_around(inner: &mut ExtentStatusTreeInner, inserted_start: Iblock) {
        let mut current = *inner.entries.get(&inserted_start).unwrap();

        if let Some((previous_start, previous)) = inner.entries.range(..current.start).next_back()
            && previous.end == current.start
            && current
                .status
                .can_follow(previous.status, previous.end - previous.start)
        {
            let previous_start = *previous_start;
            let previous = *previous;
            inner.entries.remove(&current.start);
            inner.entries.remove(&previous_start);
            current = ExtentStatusEntry::new(previous.start..current.end, previous.status);
            inner.entries.insert(current.start, current);
        }

        if let Some(next) = inner.entries.get(&current.end).copied()
            && next
                .status
                .can_follow(current.status, current.end - current.start)
        {
            inner.entries.remove(&current.start);
            inner.entries.remove(&next.start);
            current.end = next.end;
            inner.entries.insert(current.start, current);
        }
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    fn written(pblock: Ext4Bid) -> ExtentStatus {
        ExtentStatus::Written { pblock }
    }

    fn status_at(tree: &ExtentStatusTree, iblock: Iblock) -> Option<ExtentStatus> {
        tree.lookup(iblock).map(|entry| entry.status_at(iblock))
    }

    #[ktest]
    fn conditional_insert_rejects_stale_disk_lookup() {
        let tree = ExtentStatusTree::new();
        let observed_version = tree.version();

        tree.insert(8..12, ExtentStatus::Delayed);

        assert!(!tree.insert_if_version(observed_version, 8..12, ExtentStatus::Hole));
        assert_eq!(status_at(&tree, 9), Some(ExtentStatus::Delayed));
    }

    #[ktest]
    fn invalidating_unknown_range_rejects_stale_disk_lookup() {
        let tree = ExtentStatusTree::new();
        let observed_version = tree.version();

        tree.invalidate(20..24);

        assert!(!tree.insert_if_version(observed_version, 20..24, ExtentStatus::Hole));
        assert_eq!(tree.lookup(21), None);
    }

    #[ktest]
    fn mark_delayed_only_counts_new_hole_blocks() {
        let tree = ExtentStatusTree::new();
        tree.insert(0..4, written(100));
        tree.insert(4..8, ExtentStatus::Hole);
        tree.insert(8..12, ExtentStatus::Delayed);
        tree.insert(12..16, ExtentStatus::Unwritten { pblock: 200 });

        let (version, holes) = tree.known_holes_snapshot(2..14);
        let newly_delayed: Iblock = holes.iter().map(|range| range.end - range.start).sum();
        assert!(tree.mark_holes_delayed_if_version(version, &holes));

        assert_eq!(newly_delayed, 4);
        assert_eq!(status_at(&tree, 3), Some(written(103)));
        assert_eq!(status_at(&tree, 5), Some(ExtentStatus::Delayed));
        assert_eq!(status_at(&tree, 10), Some(ExtentStatus::Delayed));
        assert_eq!(
            status_at(&tree, 13),
            Some(ExtentStatus::Unwritten { pblock: 201 })
        );
    }

    #[ktest]
    fn conditional_delayed_mark_rejects_stale_hole_snapshot_without_mutation() {
        let tree = ExtentStatusTree::new();
        tree.insert(4..8, ExtentStatus::Hole);
        let (version, holes) = tree.known_holes_snapshot(4..8);

        tree.insert(4..8, written(100));

        assert!(!tree.mark_holes_delayed_if_version(version, &holes));
        assert_eq!(status_at(&tree, 5), Some(written(101)));
    }

    #[ktest]
    fn partial_materialization_preserves_the_delayed_suffix() {
        let tree = ExtentStatusTree::new();
        tree.insert(4..12, ExtentStatus::Delayed);

        assert_eq!(
            tree.materialize_delayed(4..7, ExtentStatus::Unwritten { pblock: 100 }),
            3
        );

        assert_eq!(
            status_at(&tree, 6),
            Some(ExtentStatus::Unwritten { pblock: 102 })
        );
        assert_eq!(status_at(&tree, 7), Some(ExtentStatus::Delayed));
        assert_eq!(tree.lookup(9).unwrap().range(), 7..12);
    }

    #[ktest]
    fn next_delayed_range_skips_other_statuses_and_clips_to_query() {
        let tree = ExtentStatusTree::new();
        tree.insert(0..4, written(100));
        tree.insert(4..8, ExtentStatus::Delayed);
        tree.insert(8..12, ExtentStatus::Hole);
        tree.insert(12..20, ExtentStatus::Delayed);

        assert_eq!(tree.next_delayed_range(6..16), Some(6..8));
        assert_eq!(tree.next_delayed_range(8..16), Some(12..16));
        assert_eq!(tree.next_delayed_range(20..24), None);
    }

    #[ktest]
    fn delayed_intersections_scan_only_selected_page_runs() {
        let tree = ExtentStatusTree::new();
        tree.insert(0..4, ExtentStatus::Delayed);
        tree.insert(4..8, written(100));
        tree.insert(8..16, ExtentStatus::Delayed);

        assert_eq!(
            tree.delayed_intersections(&[2..6, 10..12, 14..18]),
            [2..4, 10..12, 14..16]
        );
        assert!(
            tree.delayed_intersections(core::slice::from_ref(&(4..8)))
                .is_empty()
        );
    }

    #[ktest]
    fn sparse_page_runs_choose_local_es_queries() {
        assert!(!ExtentStatusTree::use_envelope_scan(1_000, 2));
        assert!(ExtentStatusTree::use_envelope_scan(2, 1_000));
        assert!(ExtentStatusTree::use_envelope_scan(2, 2));
    }

    #[ktest]
    fn overwrite_splits_mapped_extent_and_preserves_physical_offsets() {
        let tree = ExtentStatusTree::new();
        tree.insert(10..20, written(100));

        tree.insert(13..17, ExtentStatus::Delayed);

        assert_eq!(status_at(&tree, 12), Some(written(102)));
        assert_eq!(status_at(&tree, 13), Some(ExtentStatus::Delayed));
        assert_eq!(status_at(&tree, 16), Some(ExtentStatus::Delayed));
        assert_eq!(status_at(&tree, 17), Some(written(107)));
        assert_eq!(tree.node_count(), 3);
    }

    #[ktest]
    fn insertion_merges_only_compatible_adjacent_extents() {
        let tree = ExtentStatusTree::new();
        tree.insert(0..4, written(100));
        tree.insert(4..8, written(104));
        assert_eq!(tree.node_count(), 1);
        assert_eq!(tree.lookup(6).unwrap().range(), 0..8);

        tree.insert(8..10, written(200));
        tree.insert(10..12, ExtentStatus::Unwritten { pblock: 202 });
        assert_eq!(tree.node_count(), 3);

        tree.insert(20..22, ExtentStatus::Hole);
        tree.insert(22..24, ExtentStatus::Hole);
        assert_eq!(tree.lookup(23).unwrap().range(), 20..24);
    }

    #[ktest]
    fn recent_extent_tracks_lookup_and_is_replaced_by_mutation() {
        let tree = ExtentStatusTree::new();
        tree.insert(0..4, written(100));
        tree.insert(8..12, written(200));

        assert_eq!(tree.lookup(1).unwrap().range(), 0..4);
        assert_eq!(tree.recent_entry().unwrap().range(), 0..4);

        tree.insert(1..3, ExtentStatus::Delayed);
        assert_eq!(tree.lookup(1).unwrap().status_at(1), ExtentStatus::Delayed);
        assert_eq!(tree.recent_entry().unwrap().range(), 1..3);
    }

    #[ktest]
    fn remove_range_splits_an_extent() {
        let tree = ExtentStatusTree::new();
        tree.insert(5..15, ExtentStatus::Unwritten { pblock: 50 });

        tree.remove(8..12);

        assert_eq!(
            status_at(&tree, 7),
            Some(ExtentStatus::Unwritten { pblock: 52 })
        );
        assert_eq!(status_at(&tree, 8), None);
        assert_eq!(status_at(&tree, 11), None);
        assert_eq!(
            status_at(&tree, 12),
            Some(ExtentStatus::Unwritten { pblock: 57 })
        );
        assert_eq!(tree.node_count(), 2);
    }

    #[ktest]
    fn shrink_reclaims_cache_entries_but_keeps_delayed_state() {
        let tree = ExtentStatusTree::new();
        tree.insert(0..2, written(10));
        tree.insert(4..6, ExtentStatus::Delayed);
        tree.insert(8..10, ExtentStatus::Hole);
        tree.insert(12..14, ExtentStatus::Unwritten { pblock: 30 });

        assert_eq!(tree.shrink(2), 2);
        assert_eq!(status_at(&tree, 4), Some(ExtentStatus::Delayed));
        assert_eq!(tree.node_count(), 2);
        assert_eq!(tree.shrink(usize::MAX), 1);
        assert_eq!(tree.node_count(), 1);
        assert_eq!(status_at(&tree, 4), Some(ExtentStatus::Delayed));
    }

    #[ktest]
    fn shrink_gives_referenced_extent_a_second_chance() {
        let tree = ExtentStatusTree::new();
        tree.insert(0..2, written(10));
        assert_eq!(tree.lookup(1).unwrap().range(), 0..2);

        assert_eq!(tree.shrink(1), 0);
        assert_eq!(tree.node_count(), 1);
        assert_eq!(tree.shrink(1), 1);
        assert_eq!(tree.node_count(), 0);
    }

    #[ktest]
    fn mapping_version_changes_after_mutations() {
        let tree = ExtentStatusTree::new();
        let initial = tree.version();
        tree.insert(1..3, ExtentStatus::Delayed);
        let after_insert = tree.version();
        tree.remove(1..2);
        let after_remove = tree.version();
        tree.shrink(usize::MAX);

        assert!(after_insert > initial);
        assert!(after_remove > after_insert);
        assert_eq!(tree.version(), after_remove);
    }

    #[ktest]
    fn deterministic_operations_match_block_model() {
        const BLOCKS: usize = 64;
        let tree = ExtentStatusTree::new();
        let mut model = [None; BLOCKS];
        let mut state = 0x4d59_5df4_d0f3_3173u64;

        for step in 0..2_000u64 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let start = ((state >> 32) as usize) % BLOCKS;
            let len = ((state >> 24) as usize % 8) + 1;
            let end = (start + len).min(BLOCKS);

            if state & 3 == 0 {
                tree.remove(start as Iblock..end as Iblock);
                model[start..end].fill(None);
            } else {
                let status = match (state >> 8) & 3 {
                    0 => ExtentStatus::Delayed,
                    1 => ExtentStatus::Hole,
                    2 => written(step * 128),
                    _ => ExtentStatus::Unwritten { pblock: step * 128 },
                };
                tree.insert(start as Iblock..end as Iblock, status);
                for (offset, expected) in model[start..end].iter_mut().enumerate() {
                    *expected = Some(status.shifted(offset as Iblock));
                }
            }

            for (iblock, expected) in model.iter().copied().enumerate() {
                assert_eq!(status_at(&tree, iblock as Iblock), expected);
            }
        }
    }
}
