// SPDX-License-Identifier: MPL-2.0

//! In-memory free-run index for one ext4 block bitmap.
//!
//! The on-disk bitmap remains authoritative. This cache stores summaries over
//! 64-bit leaves in a binary tree so arbitrary-length contiguous runs can be
//! found without scanning the whole bitmap.

use smallvec::SmallVec;

use super::prelude::*;

const LEAF_BITS: usize = 64;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct FreeRunSummary {
    len: u16,
    prefix_free: u16,
    suffix_free: u16,
    max_free: u16,
}

type CoveredNode = (Option<usize>, usize, usize, FreeRunSummary);

impl FreeRunSummary {
    fn combine(left: Self, right: Self) -> Self {
        let len = left.len + right.len;
        let prefix_free = if left.prefix_free == left.len {
            left.len + right.prefix_free
        } else {
            left.prefix_free
        };
        let suffix_free = if right.suffix_free == right.len {
            right.len + left.suffix_free
        } else {
            right.suffix_free
        };
        let max_free = left
            .max_free
            .max(right.max_free)
            .max(left.suffix_free + right.prefix_free);
        Self {
            len,
            prefix_free,
            suffix_free,
            max_free,
        }
    }
}

/// A compact hierarchical index of free runs in one block-group bitmap.
pub(super) struct BuddyCache {
    len: u16,
    leaf_base: usize,
    tree: Vec<FreeRunSummary>,
}

impl BuddyCache {
    pub(super) fn from_bitmap(bitmap: &IdBitmap) -> Self {
        let leaf_count = usize::from(bitmap.len()).div_ceil(LEAF_BITS).max(1);
        let leaf_base = leaf_count.next_power_of_two();
        let mut cache = Self {
            len: bitmap.len(),
            leaf_base,
            tree: vec![FreeRunSummary::default(); leaf_base * 2],
        };
        for leaf in 0..leaf_count {
            cache.rebuild_leaf(bitmap, leaf);
        }
        for node in (1..leaf_base).rev() {
            cache.rebuild_node(node);
        }
        cache
    }

    pub(super) fn max_free(&self) -> u16 {
        self.tree[1].max_free
    }

    /// Finds the first `count`-block run at or after `goal`, then wraps once.
    pub(super) fn find_run(&self, bitmap: &IdBitmap, count: u16, goal: u16) -> Option<Range<u16>> {
        if count == 0 || count > self.max_free() {
            return None;
        }
        let goal = goal.min(self.len);
        self.find_run_in(bitmap, count, goal..self.len)
            .or_else(|| self.find_run_in(bitmap, count, 0..self.len))
    }

    /// Rebuilds summaries covering `range` after the authoritative bitmap was
    /// changed while holding the block-group metadata lock.
    pub(super) fn refresh_range(&mut self, bitmap: &IdBitmap, range: Range<u16>) {
        if range.is_empty() {
            return;
        }
        debug_assert!(range.end <= self.len);

        let first_leaf = usize::from(range.start) / LEAF_BITS;
        let last_leaf = (usize::from(range.end) - 1) / LEAF_BITS;
        for leaf in first_leaf..=last_leaf {
            self.rebuild_leaf(bitmap, leaf);
        }

        let mut first_node = (self.leaf_base + first_leaf) / 2;
        let mut last_node = (self.leaf_base + last_leaf) / 2;
        loop {
            for node in first_node..=last_node {
                self.rebuild_node(node);
            }
            if first_node == 1 {
                break;
            }
            first_node /= 2;
            last_node /= 2;
        }
    }

    fn rebuild_leaf(&mut self, bitmap: &IdBitmap, leaf: usize) {
        let start = leaf * LEAF_BITS;
        let end = (start + LEAF_BITS).min(usize::from(self.len));
        self.tree[self.leaf_base + leaf] = Self::summarize_bitmap(bitmap, start..end);
    }

    fn rebuild_node(&mut self, node: usize) {
        self.tree[node] = FreeRunSummary::combine(self.tree[node * 2], self.tree[node * 2 + 1]);
    }

    fn find_run_in(&self, bitmap: &IdBitmap, count: u16, query: Range<u16>) -> Option<Range<u16>> {
        if query.is_empty() || query.len() < usize::from(count) {
            return None;
        }

        let query = usize::from(query.start)..usize::from(query.end);
        let first_leaf = query.start / LEAF_BITS;
        let last_leaf = (query.end - 1) / LEAF_BITS;
        if first_leaf == last_leaf {
            return Self::find_first_in_leaf_range(bitmap, query, count);
        }

        let mut nodes = SmallVec::<[CoveredNode; 24]>::new();
        let first_full_leaf = if query.start % LEAF_BITS == 0 {
            first_leaf
        } else {
            let end = (first_leaf + 1) * LEAF_BITS;
            nodes.push(Self::partial_leaf(bitmap, query.start..end));
            first_leaf + 1
        };
        let last_full_leaf = if query.end % LEAF_BITS == 0 {
            last_leaf + 1
        } else {
            last_leaf
        };
        if first_full_leaf < last_full_leaf {
            self.collect_covering_nodes(
                1,
                0,
                self.leaf_base,
                first_full_leaf..last_full_leaf,
                &mut nodes,
            );
        }
        if query.end % LEAF_BITS != 0 {
            nodes.push(Self::partial_leaf(bitmap, last_leaf * LEAF_BITS..query.end));
        }

        let mut trailing_free = 0u16;
        for (node, start, end, summary) in nodes {
            if trailing_free + summary.prefix_free >= count {
                let run_start = start as u16 - trailing_free;
                return Some(run_start..run_start + count);
            }
            if summary.max_free >= count {
                return Some(match node {
                    Some(node) => self.find_first_in_node(bitmap, node, start, count),
                    None => Self::find_first_in_leaf_range(bitmap, start..end, count)
                        .expect("partial summary guarantees a matching run"),
                });
            }
            trailing_free = if summary.prefix_free == summary.len {
                trailing_free + summary.len
            } else {
                summary.suffix_free
            };
        }
        None
    }

    fn collect_covering_nodes(
        &self,
        node: usize,
        start: usize,
        end: usize,
        query: Range<usize>,
        nodes: &mut SmallVec<[CoveredNode; 24]>,
    ) {
        if query.start <= start && end <= query.end {
            nodes.push((
                Some(node),
                start * LEAF_BITS,
                end * LEAF_BITS,
                self.tree[node],
            ));
            return;
        }
        let middle = start + (end - start) / 2;
        if query.start < middle {
            self.collect_covering_nodes(node * 2, start, middle, query.clone(), nodes);
        }
        if middle < query.end {
            self.collect_covering_nodes(node * 2 + 1, middle, end, query, nodes);
        }
    }

    fn find_first_in_node(
        &self,
        bitmap: &IdBitmap,
        node: usize,
        start: usize,
        count: u16,
    ) -> Range<u16> {
        if node >= self.leaf_base {
            return self.find_first_in_leaf(bitmap, start, count);
        }

        let left = self.tree[node * 2];
        let right = self.tree[node * 2 + 1];
        let middle = start + usize::from(left.len);
        if left.max_free >= count {
            return self.find_first_in_node(bitmap, node * 2, start, count);
        }
        if left.suffix_free + right.prefix_free >= count {
            let run_start = middle as u16 - left.suffix_free;
            return run_start..run_start + count;
        }
        self.find_first_in_node(bitmap, node * 2 + 1, middle, count)
    }

    fn find_first_in_leaf(&self, bitmap: &IdBitmap, start: usize, count: u16) -> Range<u16> {
        let end = (start + LEAF_BITS).min(usize::from(self.len));
        Self::find_first_in_leaf_range(bitmap, start..end, count)
            .expect("leaf summary guarantees a matching run")
    }

    fn partial_leaf(bitmap: &IdBitmap, range: Range<usize>) -> CoveredNode {
        (
            None,
            range.start,
            range.end,
            Self::summarize_bitmap(bitmap, range.clone()),
        )
    }

    fn find_first_in_leaf_range(
        bitmap: &IdBitmap,
        range: Range<usize>,
        count: u16,
    ) -> Option<Range<u16>> {
        let len = range.len();
        if usize::from(count) > len {
            return None;
        }
        for shift in 0..=len - usize::from(count) {
            let start = range.start + shift;
            if (start..start + usize::from(count)).all(|bit| !bitmap.is_allocated(bit as u16)) {
                let start = (range.start + shift) as u16;
                return Some(start..start + count);
            }
        }
        None
    }

    fn summarize_bitmap(bitmap: &IdBitmap, range: Range<usize>) -> FreeRunSummary {
        let len = range.len();
        let mut prefix_free = 0usize;
        while prefix_free < len && !bitmap.is_allocated((range.start + prefix_free) as u16) {
            prefix_free += 1;
        }
        let mut suffix_free = 0usize;
        while suffix_free < len && !bitmap.is_allocated((range.end - suffix_free - 1) as u16) {
            suffix_free += 1;
        }
        let mut max_free = 0usize;
        let mut current = 0usize;
        for bit in range {
            if !bitmap.is_allocated(bit as u16) {
                current += 1;
                max_free = max_free.max(current);
            } else {
                current = 0;
            }
        }
        FreeRunSummary {
            len: len as u16,
            prefix_free: prefix_free as u16,
            suffix_free: suffix_free as u16,
            max_free: max_free as u16,
        }
    }
}

#[cfg(ktest)]
mod tests {
    use alloc::{vec, vec::Vec};

    use aster_block::BLOCK_SIZE;
    use ostd::prelude::ktest;

    use super::BuddyCache;
    use crate::fs::utils::IdBitmap;

    fn bitmap(len: u16, allocated: &[u16]) -> IdBitmap {
        let mut bytes = vec![0; BLOCK_SIZE].into_boxed_slice();
        for &bit in allocated {
            bytes[usize::from(bit / 8)] |= 1 << (bit % 8);
        }
        IdBitmap::from_buf(bytes, len)
    }

    #[ktest]
    fn finds_unaligned_arbitrary_length_run() {
        let bitmap = bitmap(143, &[0, 1, 2, 10, 11, 75, 76, 77, 142]);
        let cache = BuddyCache::from_bitmap(&bitmap);

        assert_eq!(cache.max_free(), 64);
        assert_eq!(cache.find_run(&bitmap, 7, 8), Some(12..19));
        assert_eq!(cache.find_run(&bitmap, 13, 70), Some(78..91));
    }

    #[ktest]
    fn searches_from_goal_then_wraps_to_head() {
        let mut allocated = (8..96).collect::<Vec<u16>>();
        allocated.extend(104..120);
        let bitmap = bitmap(128, &allocated);
        let cache = BuddyCache::from_bitmap(&bitmap);

        assert_eq!(cache.find_run(&bitmap, 8, 96), Some(96..104));
        assert_eq!(cache.find_run(&bitmap, 8, 105), Some(120..128));
        assert_eq!(cache.find_run(&bitmap, 8, 121), Some(0..8));
    }

    #[ktest]
    fn incremental_refresh_matches_bitmap_reference() {
        let mut bitmap = bitmap(257, &[0, 64, 128, 192, 256]);
        let mut cache = BuddyCache::from_bitmap(&bitmap);
        let operations = [
            (3..20, true),
            (66..97, true),
            (130..191, true),
            (8..15, false),
            (70..81, false),
            (193..225, true),
        ];

        for (range, allocate) in operations {
            if allocate {
                assert!(bitmap.alloc_exact(range.clone()));
            } else {
                bitmap.free_consecutive(range.clone());
            }
            cache.refresh_range(&bitmap, range);

            for count in 1..=65 {
                for goal in [0, 1, 63, 64, 127, 128, 200, 256] {
                    assert_eq!(
                        cache.find_run(&bitmap, count, goal),
                        naive_find_run(&bitmap, count, goal)
                    );
                }
            }
        }
    }

    #[ktest]
    fn deterministic_random_updates_match_bitmap_reference() {
        let mut bitmap = bitmap(1025, &[]);
        let mut cache = BuddyCache::from_bitmap(&bitmap);
        let mut state = 0x9e37_79b9u32;

        for _ in 0..512 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let bit = (state % u32::from(bitmap.len())) as u16;
            if bitmap.is_allocated(bit) {
                bitmap.free(bit);
            } else {
                assert!(bitmap.alloc_exact(bit..bit + 1));
            }
            cache.refresh_range(&bitmap, bit..bit + 1);

            for count in [1, 2, 3, 7, 31, 64, 65, 129] {
                for goal in [0, 1, 63, 64, 511, 512, 900, 1024] {
                    assert_eq!(
                        cache.find_run(&bitmap, count, goal),
                        naive_find_run(&bitmap, count, goal)
                    );
                }
            }
        }
    }

    fn naive_find_run(bitmap: &IdBitmap, count: u16, goal: u16) -> Option<core::ops::Range<u16>> {
        fn search(
            bitmap: &IdBitmap,
            count: u16,
            range: core::ops::Range<u16>,
        ) -> Option<core::ops::Range<u16>> {
            let mut start = range.start;
            while start.checked_add(count)? <= range.end {
                if (start..start + count).all(|bit| !bitmap.is_allocated(bit)) {
                    return Some(start..start + count);
                }
                start += 1;
            }
            None
        }

        if count == 0 || count > bitmap.len() {
            return None;
        }
        let goal = goal.min(bitmap.len());
        search(bitmap, count, goal..bitmap.len()).or_else(|| search(bitmap, count, 0..bitmap.len()))
    }
}
