// SPDX-License-Identifier: MPL-2.0

//! Extent-tree lookup: maps a logical block to its leaf extent.
//!
//! The tree root lives inline in the inode's 60-byte `i_block`. Interior nodes
//! hold index entries pointing to child blocks read from the device; leaf nodes
//! hold the extents that map logical blocks to physical runs.

#[cfg(ktest)]
use core::sync::atomic::{AtomicUsize, Ordering};

use super::node::{
    EXTENT_MAGIC, Extent, ExtentHeader, ExtentIdx, ExtentKind, MAX_WRITTEN_LEN, RawExtent,
    RawExtentHeader, RawExtentIdx, TierBlock,
};
use crate::fs::fs_impls::ext4::{
    extent_mutation_anchor::{ExtentMutationPhase, PendingExtentMutation},
    fs::{Ext4, ExtentMetadataReservation},
    inode::RAW_BLOCK_PTRS_LEN,
    prelude::*,
    utils,
    writeback::Ext4PageWritebackObserver,
};

/// Size of one extent-tree entry (header, index, or leaf), in bytes.
const ENTRY_SIZE: usize = 12;

pub(super) trait ExtentBlockIo {
    fn read_extent_block(&self, bid: Ext4Bid) -> Result<[u8; BLOCK_SIZE]>;

    fn write_extent_block(&self, bid: Ext4Bid, block: &[u8; BLOCK_SIZE]) -> Result<()>;
}

impl ExtentBlockIo for dyn BlockDevice + '_ {
    fn read_extent_block(&self, bid: Ext4Bid) -> Result<[u8; BLOCK_SIZE]> {
        Ok(self.read_val(utils::block_offset(bid, BLOCK_SIZE)?)?)
    }

    fn write_extent_block(&self, bid: Ext4Bid, block: &[u8; BLOCK_SIZE]) -> Result<()> {
        Ok(self.write_val(utils::block_offset(bid, BLOCK_SIZE)?, block)?)
    }
}

impl ExtentBlockIo for Ext4 {
    fn read_extent_block(&self, bid: Ext4Bid) -> Result<[u8; BLOCK_SIZE]> {
        self.read_metadata_block(bid)
    }

    fn write_extent_block(&self, bid: Ext4Bid, block: &[u8; BLOCK_SIZE]) -> Result<()> {
        self.write_metadata_block(bid, block)
    }
}

/// A validated extent tree rooted in an inode's inline `i_block` field.
pub(super) struct ExtentTree {
    root: [u32; RAW_BLOCK_PTRS_LEN],
    owner_ino: Option<Ext4Ino>,
    allocation_observer: Option<Arc<Ext4PageWritebackObserver>>,
    #[cfg(ktest)]
    full_scan_count: AtomicUsize,
}

#[derive(Clone, Copy)]
enum NodeLocation {
    Root,
    Block(Ext4Bid),
}

struct ExtentPathLevel {
    location: NodeLocation,
    depth: u16,
    indexes: Vec<ExtentIdx>,
    selected: usize,
}

struct ExtentPathLeaf {
    location: NodeLocation,
    capacity: usize,
    extents: Vec<Extent>,
    insert_at: usize,
}

struct ExtentPath {
    depth: u16,
    levels: Vec<ExtentPathLevel>,
    leaf: ExtentPathLeaf,
    covered: Option<Extent>,
    predecessor: Option<Extent>,
    successor: Option<Extent>,
}

impl ExtentPath {
    #[cfg(ktest)]
    fn depth(&self) -> u16 {
        self.depth
    }

    fn covered(&self) -> Option<Extent> {
        self.covered
    }

    fn predecessor(&self) -> Option<Extent> {
        self.predecessor
    }

    fn successor(&self) -> Option<Extent> {
        self.successor
    }

    fn hole_range(&self) -> Option<Range<Iblock>> {
        self.covered.is_none().then(|| {
            let start = self
                .predecessor
                .map(|extent| extent.block() + u32::from(extent.len()))
                .unwrap_or(0);
            let end = self
                .successor
                .map(|extent| extent.block())
                .unwrap_or(Iblock::MAX);
            start..end
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InPlaceInsert {
    Inserted,
    NeedsSplit,
}

enum RangeReplacement {
    Keep,
    Replace(Extent),
    Remove,
}

#[cfg(ktest)]
enum LegacyStep {
    Found(Extent),
    Hole,
    Descend(Ext4Bid),
}

impl ExtentTree {
    /// Parses and validates an inline extent-tree root.
    pub(super) fn try_from_root(root: [u32; RAW_BLOCK_PTRS_LEN]) -> Result<Self> {
        ExtentHeader::try_from(&RawExtentHeader::from_bytes(
            &root.as_bytes()[0..ENTRY_SIZE],
        ))?;
        Ok(Self {
            root,
            owner_ino: None,
            allocation_observer: None,
            #[cfg(ktest)]
            full_scan_count: AtomicUsize::new(0),
        })
    }

    pub(super) fn bind_allocation_observer(&mut self, observer: Arc<Ext4PageWritebackObserver>) {
        self.allocation_observer = Some(observer);
    }

    pub(super) fn bind_owner(&mut self, ino: Ext4Ino) {
        self.owner_ino = Some(ino);
    }

    pub(in crate::fs::fs_impls::ext4) fn recover_pending_mutation(
        fs: &Ext4,
        mutation: &PendingExtentMutation,
    ) -> Result<()> {
        if mutation.kind()?
            != crate::fs::fs_impls::ext4::extent_mutation_anchor::ExtentMutationKind::Split
        {
            return_errno_with_message!(Errno::EINVAL, "unsupported extent mutation recovery");
        }
        mutation.phase()?;
        let split_iblock = mutation.split_iblock()?;
        let parent_block = fs.read_metadata_block(mutation.parent())?;
        let parent_header = Self::node_header(&parent_block)?;
        if parent_header.is_leaf() {
            return_errno_with_message!(Errno::EUCLEAN, "split parent is an extent leaf");
        }
        let parent_published = Self::node_indexes(&parent_block, parent_header)?
            .into_iter()
            .any(|index| index.block() == split_iblock && index.leaf() == mutation.new_child());
        let nv_state = fs.nv_state().ok_or_else(|| Error::new(Errno::EIO))?;

        if !parent_published {
            nv_state.clear_transient_block(mutation.new_child())?;
            PendingExtentMutation::clear(nv_state, mutation.ino()?)?;
            fs.free_blocks(mutation.new_child(), 1)?;
            return Ok(());
        }

        let old_block = fs.read_metadata_block(mutation.old_child())?;
        let old_header = Self::node_header(&old_block)?;
        if !old_header.is_leaf() {
            return_errno_with_message!(Errno::EUCLEAN, "split child is not an extent leaf");
        }
        let old_extents = Self::leaf_extents(&old_block, old_header)?;
        let mut left = Vec::new();
        for extent in old_extents {
            let end = extent.block() + u32::from(extent.len());
            if extent.block() < split_iblock && end > split_iblock {
                return_errno_with_message!(Errno::EUCLEAN, "split key crosses an extent");
            }
            if extent.block() < split_iblock {
                left.push(extent);
            }
        }
        if left.is_empty() {
            return_errno_with_message!(Errno::EUCLEAN, "split leaves the old child empty");
        }
        Self::write_leaf_node(fs, mutation.old_child(), &left)?;
        PendingExtentMutation::clear(nv_state, mutation.ino()?)
    }

    /// Returns the serialized inline root for inode writeback.
    pub(super) const fn root(&self) -> [u32; RAW_BLOCK_PTRS_LEN] {
        self.root
    }

    /// Finds the extent covering `iblock`, or `None` when it is a hole.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn find(
        &self,
        io: &(impl ExtentBlockIo + ?Sized),
        iblock: Iblock,
    ) -> Result<Option<Extent>> {
        Ok(self.lookup_path(io, iblock)?.covered())
    }

    /// Finds the extent covering `iblock`, or the maximal known hole around it.
    ///
    /// Both covered mappings and hole boundaries come from one root-to-leaf
    /// `ExtentPath`; a gap at a leaf edge descends only the adjacent subtree.
    pub(super) fn find_or_hole(
        &self,
        io: &(impl ExtentBlockIo + ?Sized),
        iblock: Iblock,
    ) -> Result<(Option<Extent>, Range<Iblock>)> {
        let path = self.lookup_path(io, iblock)?;
        if let Some(extent) = path.covered() {
            let range = extent.block()..extent.block() + u32::from(extent.len());
            return Ok((Some(extent), range));
        }
        Ok((None, path.hole_range().unwrap()))
    }

    pub(super) fn next_extent_at_or_after(
        &self,
        io: &(impl ExtentBlockIo + ?Sized),
        iblock: Iblock,
    ) -> Result<Option<Extent>> {
        let path = self.lookup_path(io, iblock)?;
        Ok(path.covered().or_else(|| path.successor()))
    }

    /// Returns the preferred physical allocation goal immediately before a hole.
    pub(super) fn allocation_goal(
        &self,
        io: &(impl ExtentBlockIo + ?Sized),
        iblock: Iblock,
    ) -> Result<Ext4Bid> {
        let path = self.lookup_path(io, iblock)?;
        if path.covered().is_some() {
            return_errno_with_message!(Errno::EEXIST, "allocation target is already mapped");
        }
        Ok(path
            .predecessor()
            .map(|extent| extent.start() + Ext4Bid::from(extent.len()))
            .unwrap_or(0))
    }

    /// Returns a conservative upper bound for metadata blocks allocated by an insert.
    pub(super) fn metadata_blocks_needed_for_insert(
        &self,
        io: &(impl ExtentBlockIo + ?Sized),
        iblock: Iblock,
    ) -> Result<u32> {
        let path = self.lookup_path(io, iblock)?;
        let leaf_is_full = path.leaf.extents.len() >= path.leaf.capacity;
        let changes_deep_ancestor_key = path.depth > 1 && path.leaf.insert_at == 0;
        if !leaf_is_full && !changes_deep_ancestor_key {
            return Ok(0);
        }
        if matches!(path.leaf.location, NodeLocation::Root) {
            return Ok(1);
        }

        // A full external leaf may split into two COW leaves. A non-full leaf
        // whose first key changes still needs one COW replacement so that deep
        // ancestor keys can be updated. Full external parents can likewise
        // split into two replacement nodes.
        let mut replacements = if leaf_is_full { 2usize } else { 1usize };
        let mut needed = u32::try_from(replacements).unwrap();
        for level in path.levels.into_iter().rev() {
            let entry_count = level
                .indexes
                .len()
                .checked_sub(1)
                .and_then(|count| count.checked_add(replacements))
                .ok_or_else(|| {
                    Error::with_message(Errno::EUCLEAN, "invalid extent insertion path")
                })?;
            match level.location {
                NodeLocation::Root => {
                    if entry_count > INLINE_MAX {
                        if level.depth == 5 {
                            return_errno_with_message!(
                                Errno::EFBIG,
                                "extent tree reached maximum depth"
                            );
                        }
                        needed = needed.checked_add(1).ok_or_else(|| {
                            Error::with_message(
                                Errno::EOVERFLOW,
                                "extent metadata reservation overflow",
                            )
                        })?;
                    }
                    return Ok(needed);
                }
                NodeLocation::Block(_) => {
                    replacements = if entry_count <= LEAF_MAX { 1 } else { 2 };
                    needed = needed
                        .checked_add(u32::try_from(replacements).unwrap())
                        .ok_or_else(|| {
                            Error::with_message(
                                Errno::EOVERFLOW,
                                "extent metadata reservation overflow",
                            )
                        })?;
                }
            }
        }
        return_errno_with_message!(Errno::EUCLEAN, "external extent leaf has no root path");
    }

    fn lookup_path(
        &self,
        io: &(impl ExtentBlockIo + ?Sized),
        iblock: Iblock,
    ) -> Result<ExtentPath> {
        let mut bytes = [0u8; BLOCK_SIZE];
        bytes[..size_of::<[u32; RAW_BLOCK_PTRS_LEN]>()].copy_from_slice(self.root.as_bytes());
        let mut location = NodeLocation::Root;
        let mut node_len = size_of::<[u32; RAW_BLOCK_PTRS_LEN]>();
        let root_header = Self::node_header(&bytes[..node_len])?;
        let root_depth = root_header.depth();
        let mut expected_depth = root_depth;
        let mut levels = Vec::with_capacity(usize::from(root_depth));

        loop {
            let header = Self::node_header(&bytes[..node_len])?;
            if header.depth() != expected_depth {
                return_errno_with_message!(Errno::EUCLEAN, "extent path depth mismatch");
            }
            if header.is_leaf() {
                let extents = Self::leaf_extents(&bytes[..node_len], header)?;
                let insert_at = extents.partition_point(|extent| extent.block() <= iblock);
                let covered = insert_at
                    .checked_sub(1)
                    .and_then(|idx| extents.get(idx).copied())
                    .filter(|extent| extent.covers(iblock));
                let mut predecessor = insert_at
                    .checked_sub(1)
                    .and_then(|idx| extents.get(idx).copied());
                let mut successor = extents.get(insert_at).copied();
                if covered.is_none() && predecessor.is_none() {
                    predecessor = Self::neighbor_extent(io, &levels, false)?;
                }
                if covered.is_none() && successor.is_none() {
                    successor = Self::neighbor_extent(io, &levels, true)?;
                }
                return Ok(ExtentPath {
                    depth: root_depth,
                    levels,
                    leaf: ExtentPathLeaf {
                        location,
                        capacity: usize::from(header.max()),
                        extents,
                        insert_at,
                    },
                    covered,
                    predecessor,
                    successor,
                });
            }

            let indexes = Self::node_indexes(&bytes[..node_len], header)?;
            if indexes.is_empty() {
                return_errno_with_message!(Errno::EUCLEAN, "empty extent index node");
            }
            let after = indexes.partition_point(|idx| idx.block() <= iblock);
            let selected = after.saturating_sub(1);
            let next_bid = indexes[selected].leaf();
            levels.push(ExtentPathLevel {
                location,
                depth: header.depth(),
                indexes,
                selected,
            });
            bytes = io.read_extent_block(next_bid)?;
            location = NodeLocation::Block(next_bid);
            node_len = BLOCK_SIZE;
            expected_depth = expected_depth
                .checked_sub(1)
                .ok_or_else(|| Error::with_message(Errno::EUCLEAN, "invalid extent depth"))?;
        }
    }

    fn node_header(bytes: &[u8]) -> Result<ExtentHeader> {
        if bytes.len() < ENTRY_SIZE {
            return_errno_with_message!(Errno::EUCLEAN, "extent node header is truncated");
        }
        let header = ExtentHeader::try_from(&RawExtentHeader::from_bytes(&bytes[..ENTRY_SIZE]))?;
        if ENTRY_SIZE * (1 + usize::from(header.max())) > bytes.len() {
            return_errno_with_message!(Errno::EUCLEAN, "extent node capacity exceeds node size");
        }
        Ok(header)
    }

    fn leaf_extents(bytes: &[u8], header: ExtentHeader) -> Result<Vec<Extent>> {
        let nr_entries = usize::from(header.entries());
        if ENTRY_SIZE * (1 + nr_entries) > bytes.len() {
            return_errno_with_message!(Errno::EUCLEAN, "extent leaf entries overrun node");
        }
        (0..nr_entries)
            .map(|i| {
                let off = ENTRY_SIZE * (1 + i);
                Extent::try_from(&RawExtent::from_bytes(&bytes[off..off + ENTRY_SIZE]))
            })
            .collect()
    }

    fn node_indexes(bytes: &[u8], header: ExtentHeader) -> Result<Vec<ExtentIdx>> {
        let nr_entries = usize::from(header.entries());
        if ENTRY_SIZE * (1 + nr_entries) > bytes.len() {
            return_errno_with_message!(Errno::EUCLEAN, "extent index entries overrun node");
        }
        Ok((0..nr_entries)
            .map(|i| {
                let off = ENTRY_SIZE * (1 + i);
                ExtentIdx::from(&RawExtentIdx::from_bytes(&bytes[off..off + ENTRY_SIZE]))
            })
            .collect())
    }

    fn neighbor_extent(
        io: &(impl ExtentBlockIo + ?Sized),
        levels: &[ExtentPathLevel],
        successor: bool,
    ) -> Result<Option<Extent>> {
        for level in levels.iter().rev() {
            let sibling = if successor {
                level.indexes.get(level.selected + 1)
            } else {
                level
                    .selected
                    .checked_sub(1)
                    .and_then(|idx| level.indexes.get(idx))
            };
            if let Some(sibling) = sibling {
                return Self::edge_extent(io, sibling.leaf(), level.depth - 1, successor);
            }
        }
        Ok(None)
    }

    fn edge_extent(
        io: &(impl ExtentBlockIo + ?Sized),
        mut bid: Ext4Bid,
        mut expected_depth: u16,
        first: bool,
    ) -> Result<Option<Extent>> {
        loop {
            let bytes = io.read_extent_block(bid)?;
            let header = Self::node_header(&bytes)?;
            if header.depth() != expected_depth {
                return_errno_with_message!(Errno::EUCLEAN, "extent neighbor depth mismatch");
            }
            if header.is_leaf() {
                let extents = Self::leaf_extents(&bytes, header)?;
                return Ok(if first {
                    extents.first().copied()
                } else {
                    extents.last().copied()
                });
            }
            let indexes = Self::node_indexes(&bytes, header)?;
            let next = if first {
                indexes.first()
            } else {
                indexes.last()
            }
            .ok_or_else(|| Error::with_message(Errno::EUCLEAN, "empty extent index node"))?;
            bid = next.leaf();
            expected_depth = expected_depth
                .checked_sub(1)
                .ok_or_else(|| Error::with_message(Errno::EUCLEAN, "invalid extent depth"))?;
        }
    }

    fn try_insert_in_place(
        &mut self,
        fs: &Ext4,
        iblock: Iblock,
        pblock: Ext4Bid,
        len: u16,
        kind: ExtentKind,
    ) -> Result<InPlaceInsert> {
        if len == 0 {
            return_errno_with_message!(Errno::EINVAL, "cannot insert an empty extent");
        }
        let path = self.lookup_path(fs, iblock)?;
        let end = iblock as u64 + u64::from(len);
        if path.covered().is_some()
            || path
                .predecessor()
                .is_some_and(|extent| extent.block() as u64 + extent.len() as u64 > iblock as u64)
            || path
                .successor()
                .is_some_and(|extent| end > u64::from(extent.block()))
        {
            return_errno_with_message!(Errno::EINVAL, "extent insertion overlaps existing mapping");
        }

        let new_extent = Extent::new(iblock, len, pblock, kind);
        let old_first = path.leaf.extents.first().map(Extent::block);
        let mut extents = path.leaf.extents;
        extents.insert(path.leaf.insert_at, new_extent);
        Self::merge_extents(&mut extents);
        if extents.len() > path.leaf.capacity {
            return Ok(InPlaceInsert::NeedsSplit);
        }
        let new_first = extents.first().map(Extent::block);
        let first_changed = old_first != new_first;
        if first_changed && path.depth > 1 {
            return Ok(InPlaceInsert::NeedsSplit);
        }

        match path.leaf.location {
            NodeLocation::Root => self.write_inline_leaf_root(&extents)?,
            NodeLocation::Block(bid) => Self::write_leaf_node(fs, bid, &extents)?,
        }
        if first_changed && path.depth == 1 {
            let level = path
                .levels
                .first()
                .expect("a depth-1 extent path has one index level");
            debug_assert!(matches!(level.location, NodeLocation::Root));
            let selected_leaf = level.indexes[level.selected].leaf();
            let mut indexes: Vec<RawExtentIdx> = level
                .indexes
                .iter()
                .map(|idx| RawExtentIdx::new(idx.block(), idx.leaf()))
                .collect::<Result<_>>()?;
            indexes[level.selected] =
                RawExtentIdx::new(new_first.expect("inserted leaf is nonempty"), selected_leaf)?;
            self.write_inline_index_root(&indexes, 1);
        }
        Ok(InPlaceInsert::Inserted)
    }

    #[cfg(ktest)]
    fn legacy_find(
        &self,
        io: &(impl ExtentBlockIo + ?Sized),
        iblock: Iblock,
    ) -> Result<Option<Extent>> {
        let mut next_bid = match Self::legacy_search_node(self.root.as_bytes(), iblock)? {
            LegacyStep::Found(extent) => return Ok(Some(extent)),
            LegacyStep::Hole => return Ok(None),
            LegacyStep::Descend(bid) => bid,
        };
        for _ in 0..5 {
            let block = io.read_extent_block(next_bid)?;
            match Self::legacy_search_node(&block, iblock)? {
                LegacyStep::Found(extent) => return Ok(Some(extent)),
                LegacyStep::Hole => return Ok(None),
                LegacyStep::Descend(bid) => next_bid = bid,
            }
        }
        return_errno_with_message!(Errno::EUCLEAN, "extent tree deeper than maximum depth");
    }

    #[cfg(ktest)]
    fn legacy_search_node(bytes: &[u8], iblock: Iblock) -> Result<LegacyStep> {
        let header = Self::node_header(bytes)?;
        if header.is_leaf() {
            let extents = Self::leaf_extents(bytes, header)?;
            return Ok(extents
                .into_iter()
                .take_while(|extent| extent.block() <= iblock)
                .last()
                .filter(|extent| extent.covers(iblock))
                .map(LegacyStep::Found)
                .unwrap_or(LegacyStep::Hole));
        }
        Ok(Self::node_indexes(bytes, header)?
            .into_iter()
            .take_while(|idx| idx.block() <= iblock)
            .last()
            .map(|idx| LegacyStep::Descend(idx.leaf()))
            .unwrap_or(LegacyStep::Hole))
    }

    /// Inserts a mapping into a hole and updates the serialized tree.
    pub(super) fn insert(
        &mut self,
        fs: &Ext4,
        iblock: Iblock,
        pblock: Ext4Bid,
        len: u16,
        kind: ExtentKind,
    ) -> Result<TreeDelta> {
        if self.try_insert_in_place(fs, iblock, pblock, len, kind)? == InPlaceInsert::Inserted {
            return Ok(TreeDelta {
                meta_allocated: 0,
                meta_freed: 0,
                allocated: Vec::new(),
                deferred_frees: Vec::new(),
            });
        }

        self.insert_with_split(fs, iblock, pblock, len, kind, &mut None)
    }

    /// Inserts using blocks held by a short-lived extent metadata reservation.
    pub(super) fn insert_reserved(
        &mut self,
        fs: &Ext4,
        iblock: Iblock,
        pblock: Ext4Bid,
        len: u16,
        kind: ExtentKind,
        reservation: &mut ExtentMetadataReservation<'_>,
    ) -> Result<TreeDelta> {
        if self.try_insert_in_place(fs, iblock, pblock, len, kind)? == InPlaceInsert::Inserted {
            return Ok(TreeDelta {
                meta_allocated: 0,
                meta_freed: 0,
                allocated: Vec::new(),
                deferred_frees: Vec::new(),
            });
        }

        self.insert_with_split(fs, iblock, pblock, len, kind, &mut Some(reservation))
    }

    /// Inserts through a copy-on-write path and switches the in-memory inline root last.
    fn insert_with_split(
        &mut self,
        fs: &Ext4,
        iblock: Iblock,
        pblock: Ext4Bid,
        len: u16,
        kind: ExtentKind,
        metadata_reservation: &mut Option<&mut ExtentMetadataReservation<'_>>,
    ) -> Result<TreeDelta> {
        let path = self.lookup_path(fs, iblock)?;
        let end = u64::from(iblock) + u64::from(len);
        if path.covered().is_some()
            || path.predecessor().is_some_and(|extent| {
                u64::from(extent.block()) + u64::from(extent.len()) > u64::from(iblock)
            })
            || path
                .successor()
                .is_some_and(|extent| end > u64::from(extent.block()))
        {
            return_errno_with_message!(Errno::EINVAL, "extent insertion overlaps existing mapping");
        }

        let mut extents = path.leaf.extents.clone();
        extents.insert(path.leaf.insert_at, Extent::new(iblock, len, pblock, kind));
        Self::merge_extents(&mut extents);

        self.rewrite_leaf_path(fs, path, extents, pblock, metadata_reservation)
    }

    fn rewrite_leaf_path(
        &mut self,
        fs: &Ext4,
        path: ExtentPath,
        extents: Vec<Extent>,
        goal: Ext4Bid,
        metadata_reservation: &mut Option<&mut ExtentMetadataReservation<'_>>,
    ) -> Result<TreeDelta> {
        if let Some(delta) =
            self.try_split_nv_leaf_in_place(fs, &path, &extents, goal, metadata_reservation)?
        {
            return Ok(delta);
        }
        if matches!(path.leaf.location, NodeLocation::Root) {
            return self.grow_inline_leaf(fs, &extents, metadata_reservation);
        }

        let mut allocated = Vec::new();
        let mut obsolete = Vec::new();
        let result = (|| {
            let old_leaf = match path.leaf.location {
                NodeLocation::Block(bid) => bid,
                NodeLocation::Root => unreachable!(),
            };
            let mut replacements = Self::write_cow_leaves(
                fs,
                &extents,
                &mut allocated,
                metadata_reservation,
                self.allocation_observer.as_deref(),
            )?;
            obsolete.push(old_leaf);

            for level in path.levels.into_iter().rev() {
                let mut indexes = Self::raw_indexes(&level.indexes)?;
                indexes.splice(level.selected..=level.selected, replacements);

                match level.location {
                    NodeLocation::Root => {
                        if indexes.len() <= INLINE_MAX {
                            let meta_allocated = u32::try_from(allocated.len()).map_err(|_| {
                                Error::with_message(
                                    Errno::EOVERFLOW,
                                    "too many extent metadata blocks",
                                )
                            })?;
                            self.write_inline_index_root(&indexes, level.depth);
                            return Ok(meta_allocated);
                        } else {
                            if level.depth == 5 {
                                return_errno_with_message!(
                                    Errno::EFBIG,
                                    "extent tree reached maximum depth"
                                );
                            }
                            let root_child = Self::alloc_one(
                                fs,
                                goal,
                                &mut allocated,
                                metadata_reservation,
                                self.allocation_observer.as_deref(),
                            )?;
                            Self::write_index_node(fs, root_child, &indexes, level.depth)?;
                            let root_entry = RawExtentIdx::new(indexes[0].block, root_child)?;
                            let meta_allocated = u32::try_from(allocated.len()).map_err(|_| {
                                Error::with_message(
                                    Errno::EOVERFLOW,
                                    "too many extent metadata blocks",
                                )
                            })?;
                            self.write_inline_index_root(&[root_entry], level.depth + 1);
                            return Ok(meta_allocated);
                        }
                    }
                    NodeLocation::Block(old_bid) => {
                        replacements = Self::write_cow_indexes(
                            fs,
                            &indexes,
                            level.depth,
                            goal,
                            &mut allocated,
                            metadata_reservation,
                            self.allocation_observer.as_deref(),
                        )?;
                        obsolete.push(old_bid);
                    }
                }
            }
            return_errno_with_message!(Errno::EUCLEAN, "external extent leaf has no root path");
        })();

        let meta_allocated = match result {
            Ok(meta_allocated) => meta_allocated,
            Err(err) => {
                Self::rollback_allocations(fs, &allocated, metadata_reservation);
                return Err(err);
            }
        };

        let meta_freed = u32::try_from(obsolete.len())
            .map_err(|_| Error::with_message(Errno::EOVERFLOW, "too many obsolete extent nodes"))?;
        Ok(TreeDelta {
            meta_allocated,
            meta_freed,
            allocated,
            deferred_frees: obsolete,
        })
    }

    fn try_split_nv_leaf_in_place(
        &self,
        fs: &Ext4,
        path: &ExtentPath,
        extents: &[Extent],
        goal: Ext4Bid,
        metadata_reservation: &mut Option<&mut ExtentMetadataReservation<'_>>,
    ) -> Result<Option<TreeDelta>> {
        if extents.len() <= path.leaf.capacity || fs.nv_state().is_none() {
            return Ok(None);
        }
        let Some(ino) = self.owner_ino else {
            return Ok(None);
        };
        let NodeLocation::Block(old_child) = path.leaf.location else {
            return Ok(None);
        };
        let Some(parent_level) = path.levels.last() else {
            return Ok(None);
        };
        let NodeLocation::Block(parent) = parent_level.location else {
            return Ok(None);
        };
        if parent_level.indexes.len() >= LEAF_MAX {
            return Ok(None);
        }

        let split_at = extents.len().div_ceil(2);
        let (left, right) = extents.split_at(split_at);
        let split_iblock = right[0].block();
        let old_left_len = path
            .leaf
            .extents
            .partition_point(|extent| extent.block() < split_iblock);
        let old_left = &path.leaf.extents[..old_left_len];
        let left_is_unchanged_prefix = old_left.len() == left.len()
            && old_left.iter().zip(left).all(|(old, new)| {
                old.block() == new.block()
                    && old.len() == new.len()
                    && old.tier_start() == new.tier_start()
                    && old.kind() == new.kind()
            });
        if !left_is_unchanged_prefix {
            return Ok(None);
        }
        let mut allocated = Vec::new();
        let new_child = Self::alloc_one(
            fs,
            goal,
            &mut allocated,
            metadata_reservation,
            self.allocation_observer.as_deref(),
        )?;
        if let Err(error) = Self::write_leaf_node(fs, new_child, right) {
            Self::rollback_allocations(fs, &allocated, metadata_reservation);
            return Err(error);
        }

        let nv_state = fs.nv_state().unwrap();
        let mut mutation = PendingExtentMutation::split(
            ino,
            parent,
            old_child,
            new_child,
            split_iblock,
            u16::try_from(path.leaf.extents.len()).unwrap(),
        );
        if let Err(error) = mutation.persist(nv_state) {
            Self::rollback_allocations(fs, &allocated, metadata_reservation);
            return Err(error);
        }

        let mut indexes = Self::raw_indexes(&parent_level.indexes)?;
        indexes.insert(
            parent_level.selected + 1,
            RawExtentIdx::new(right[0].block(), new_child)?,
        );
        Self::write_index_node(fs, parent, &indexes, parent_level.depth)?;
        mutation.set_phase(ExtentMutationPhase::ParentPublished);
        mutation.persist(nv_state)?;
        Self::write_leaf_node(fs, old_child, left)?;
        PendingExtentMutation::clear(nv_state, ino)?;

        Ok(Some(TreeDelta {
            meta_allocated: 1,
            meta_freed: 0,
            allocated,
            deferred_frees: Vec::new(),
        }))
    }

    fn remove_empty_leaf_path(
        &mut self,
        fs: &Ext4,
        path: ExtentPath,
        goal: Ext4Bid,
    ) -> Result<TreeDelta> {
        if matches!(path.leaf.location, NodeLocation::Root) {
            self.write_inline_leaf_root(&[])?;
            return Ok(TreeDelta::empty());
        }

        let mut allocated = Vec::new();
        let mut obsolete = Vec::new();
        let result = (|| {
            let old_leaf = match path.leaf.location {
                NodeLocation::Block(bid) => bid,
                NodeLocation::Root => unreachable!(),
            };
            obsolete.push(old_leaf);
            let mut replacements = Vec::new();

            for level in path.levels.into_iter().rev() {
                let mut indexes = Self::raw_indexes(&level.indexes)?;
                indexes.splice(level.selected..=level.selected, replacements);
                match level.location {
                    NodeLocation::Root => {
                        if indexes.is_empty() {
                            self.write_inline_leaf_root(&[])?;
                        } else {
                            self.write_inline_index_root(&indexes, level.depth);
                        }
                        return u32::try_from(allocated.len()).map_err(|_| {
                            Error::with_message(Errno::EOVERFLOW, "too many extent metadata blocks")
                        });
                    }
                    NodeLocation::Block(old_bid) => {
                        obsolete.push(old_bid);
                        replacements = if indexes.is_empty() {
                            Vec::new()
                        } else {
                            Self::write_cow_indexes(
                                fs,
                                &indexes,
                                level.depth,
                                goal,
                                &mut allocated,
                                &mut None,
                                self.allocation_observer.as_deref(),
                            )?
                        };
                    }
                }
            }
            return_errno_with_message!(Errno::EUCLEAN, "external extent leaf has no root path");
        })();

        let meta_allocated = match result {
            Ok(meta_allocated) => meta_allocated,
            Err(error) => {
                Self::rollback_allocations(fs, &allocated, &mut None);
                return Err(error);
            }
        };
        Ok(TreeDelta {
            meta_allocated,
            meta_freed: u32::try_from(obsolete.len()).map_err(|_| Error::new(Errno::EOVERFLOW))?,
            allocated,
            deferred_frees: obsolete,
        })
    }

    fn replace_range(
        &mut self,
        fs: &Ext4,
        range: Range<Iblock>,
        mut replacement: impl FnMut(Extent, Iblock, Iblock) -> Result<RangeReplacement>,
    ) -> Result<TreeDelta> {
        let mut delta = TreeDelta::empty();
        let mut cursor = range.start;
        while cursor < range.end {
            let path = self.lookup_path(fs, cursor)?;
            let Some(covered) = path.covered() else {
                cursor = range.end.min(path.hole_range().unwrap().end);
                continue;
            };
            let extent_end = covered.block() + u32::from(covered.len());
            let overlap_start = cursor.max(covered.block());
            let overlap_end = range.end.min(extent_end);
            let middle = match replacement(covered, overlap_start, overlap_end)? {
                RangeReplacement::Keep => {
                    cursor = overlap_end;
                    continue;
                }
                RangeReplacement::Replace(middle) => Some(middle),
                RangeReplacement::Remove => None,
            };
            {
                let covered_idx = path
                    .leaf
                    .insert_at
                    .checked_sub(1)
                    .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
                let mut extents = path.leaf.extents.clone();
                extents.remove(covered_idx);
                let mut replacements = Vec::with_capacity(3);
                if overlap_start > covered.block() {
                    replacements.push(Extent::new_tiered(
                        covered.block(),
                        u16::try_from(overlap_start - covered.block()).unwrap(),
                        covered.tier_start(),
                        covered.kind(),
                    ));
                }
                replacements.extend(middle);
                if overlap_end < extent_end {
                    replacements.push(Extent::new_tiered(
                        overlap_end,
                        u16::try_from(extent_end - overlap_end).unwrap(),
                        covered.tier_start().offset(overlap_end - covered.block())?,
                        covered.kind(),
                    ));
                }
                extents.splice(covered_idx..covered_idx, replacements);
                Self::merge_extents(&mut extents);

                let first_unchanged = extents.first().map(Extent::block)
                    == path.leaf.extents.first().map(Extent::block);
                let goal = match covered.tier_start() {
                    TierBlock::Ssd(block) => block,
                    TierBlock::Nv(_) => 0,
                };
                let local_delta = if extents.is_empty() {
                    self.remove_empty_leaf_path(fs, path, goal)?
                } else if extents.len() <= path.leaf.capacity && first_unchanged {
                    match path.leaf.location {
                        NodeLocation::Root => self.write_inline_leaf_root(&extents)?,
                        NodeLocation::Block(bid) => Self::write_leaf_node(fs, bid, &extents)?,
                    }
                    TreeDelta::empty()
                } else {
                    let goal = extents
                        .iter()
                        .find_map(|extent| match extent.tier_start() {
                            TierBlock::Ssd(block) => Some(block),
                            TierBlock::Nv(_) => None,
                        })
                        .unwrap_or(0);
                    self.rewrite_leaf_path(fs, path, extents, goal, &mut None)?
                };
                delta.absorb(local_delta)?;
            }
            cursor = overlap_end;
        }
        Ok(delta)
    }

    pub(super) fn remove_range(
        &mut self,
        fs: &Ext4,
        range: Range<Iblock>,
    ) -> Result<(TreeDelta, Vec<Extent>)> {
        let mut removed = Vec::new();
        let delta = self.replace_range(fs, range, |extent, start, end| {
            removed.push(Extent::new_tiered(
                start,
                u16::try_from(end - start).unwrap(),
                extent.tier_start().offset(start - extent.block())?,
                extent.kind(),
            ));
            Ok(RangeReplacement::Remove)
        })?;
        Ok((delta, removed))
    }

    fn grow_inline_leaf(
        &mut self,
        fs: &Ext4,
        extents: &[Extent],
        metadata_reservation: &mut Option<&mut ExtentMetadataReservation<'_>>,
    ) -> Result<TreeDelta> {
        if extents.len() <= INLINE_MAX {
            self.write_inline_leaf_root(extents)?;
            return Ok(TreeDelta {
                meta_allocated: 0,
                meta_freed: 0,
                allocated: Vec::new(),
                deferred_frees: Vec::new(),
            });
        }
        if extents.len() > LEAF_MAX {
            return_errno_with_message!(Errno::EUCLEAN, "inline extent overflow exceeds one leaf");
        }

        let mut allocated = Vec::new();
        let result = (|| {
            let leaf_bid = Self::alloc_one(
                fs,
                extents[0].start(),
                &mut allocated,
                metadata_reservation,
                self.allocation_observer.as_deref(),
            )?;
            Self::write_leaf_node(fs, leaf_bid, extents)?;
            let root_entry = RawExtentIdx::new(extents[0].block(), leaf_bid)?;
            self.write_inline_index_root(&[root_entry], 1);
            Ok(())
        })();
        if let Err(err) = result {
            Self::rollback_allocations(fs, &allocated, metadata_reservation);
            return Err(err);
        }
        Ok(TreeDelta {
            meta_allocated: 1,
            meta_freed: 0,
            allocated,
            deferred_frees: Vec::new(),
        })
    }

    fn write_cow_leaves(
        fs: &Ext4,
        extents: &[Extent],
        allocated: &mut Vec<Ext4Bid>,
        metadata_reservation: &mut Option<&mut ExtentMetadataReservation<'_>>,
        observer: Option<&Ext4PageWritebackObserver>,
    ) -> Result<Vec<RawExtentIdx>> {
        let chunks = if extents.len() <= LEAF_MAX { 1 } else { 2 };
        let split_at = if chunks == 1 {
            extents.len()
        } else {
            extents.len().div_ceil(2)
        };
        let parts = [&extents[..split_at], &extents[split_at..]];
        let mut replacements = Vec::with_capacity(chunks);
        for part in parts.into_iter().take(chunks) {
            let bid = Self::alloc_one(
                fs,
                part[0].start(),
                allocated,
                metadata_reservation,
                observer,
            )?;
            Self::write_leaf_node(fs, bid, part)?;
            replacements.push(RawExtentIdx::new(part[0].block(), bid)?);
        }
        Ok(replacements)
    }

    fn write_cow_indexes(
        fs: &Ext4,
        indexes: &[RawExtentIdx],
        depth: u16,
        goal: Ext4Bid,
        allocated: &mut Vec<Ext4Bid>,
        metadata_reservation: &mut Option<&mut ExtentMetadataReservation<'_>>,
        observer: Option<&Ext4PageWritebackObserver>,
    ) -> Result<Vec<RawExtentIdx>> {
        let chunks = if indexes.len() <= LEAF_MAX { 1 } else { 2 };
        let split_at = if chunks == 1 {
            indexes.len()
        } else {
            indexes.len().div_ceil(2)
        };
        let parts = [&indexes[..split_at], &indexes[split_at..]];
        let mut replacements = Vec::with_capacity(chunks);
        for part in parts.into_iter().take(chunks) {
            let bid = Self::alloc_one(fs, goal, allocated, metadata_reservation, observer)?;
            Self::write_index_node(fs, bid, part, depth)?;
            replacements.push(RawExtentIdx::new(part[0].block, bid)?);
        }
        Ok(replacements)
    }

    fn raw_indexes(indexes: &[ExtentIdx]) -> Result<Vec<RawExtentIdx>> {
        indexes
            .iter()
            .map(|idx| RawExtentIdx::new(idx.block(), idx.leaf()))
            .collect()
    }

    fn alloc_one(
        fs: &Ext4,
        goal: Ext4Bid,
        allocated: &mut Vec<Ext4Bid>,
        metadata_reservation: &mut Option<&mut ExtentMetadataReservation<'_>>,
        observer: Option<&Ext4PageWritebackObserver>,
    ) -> Result<Ext4Bid> {
        let bid = match metadata_reservation.as_deref_mut() {
            Some(reservation) => reservation.alloc_block(goal)?,
            None => fs.alloc_blocks(1, goal)?.start,
        };
        if let Some(observer) = observer {
            observer.touch_metadata_range(fs, bid..bid + 1);
        }
        allocated.push(bid);
        Ok(bid)
    }

    fn rollback_allocations(
        fs: &Ext4,
        allocated: &[Ext4Bid],
        metadata_reservation: &mut Option<&mut ExtentMetadataReservation<'_>>,
    ) {
        for &bid in allocated {
            if fs.free_blocks(bid, 1).is_ok()
                && let Some(reservation) = metadata_reservation.as_deref_mut()
            {
                reservation.restore_block();
            }
        }
    }

    /// Returns every leaf extent sorted by logical block.
    #[cfg(ktest)]
    pub(super) fn extents(&self, io: &(impl ExtentBlockIo + ?Sized)) -> Result<Vec<Extent>> {
        let (mut extents, _external) = self.flatten(io)?;
        extents.sort_by_key(|extent| extent.block());
        Ok(extents)
    }

    #[cfg(ktest)]
    pub(super) fn full_scan_count(&self) -> usize {
        self.full_scan_count.load(Ordering::Relaxed)
    }

    /// Converts overlapping unwritten mappings to written mappings.
    pub(super) fn convert_unwritten(
        &mut self,
        fs: &Ext4,
        iblock: Iblock,
        len: u32,
    ) -> Result<TreeDelta> {
        let end = iblock
            .checked_add(len)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.replace_range(fs, iblock..end, |extent, start, end| {
            if !extent.is_unwritten() {
                return Ok(RangeReplacement::Keep);
            }
            Ok(RangeReplacement::Replace(Extent::new_tiered(
                start,
                u16::try_from(end - start).unwrap(),
                extent.tier_start().offset(start - extent.block())?,
                ExtentKind::Written,
            )))
        })
    }

    /// Replaces a written logical range with a tiered address while preserving
    /// the surrounding mappings and SSD allocation ownership.
    pub(super) fn publish_tiered(
        &mut self,
        fs: &Ext4,
        iblock: Iblock,
        len: u32,
        tier_start: TierBlock,
    ) -> Result<(TreeDelta, Vec<crate::fs::nv_pool::id::NvPageId>)> {
        let end = iblock
            .checked_add(len)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let mut old_nv = Vec::new();
        let delta = self.replace_range(fs, iblock..end, |extent, start, end| {
            for offset in start - extent.block()..end - extent.block() {
                if let TierBlock::Nv(page) = extent.tier_start().offset(offset)? {
                    old_nv.push(page);
                }
            }
            Ok(RangeReplacement::Replace(Extent::new_tiered(
                start,
                u16::try_from(end - start).unwrap(),
                tier_start.offset(start - iblock)?,
                ExtentKind::Written,
            )))
        })?;
        Ok((delta, old_nv))
    }

    /// Rebuilds the tree from the mappings retained by truncate.
    #[cfg(ktest)]
    pub(super) fn rebuild(
        &mut self,
        fs: &Ext4,
        extents: &[(Iblock, u16, Ext4Bid, ExtentKind)],
    ) -> Result<(u32, TreeDelta)> {
        let (_old, old_external) = self.flatten(fs)?;
        let extents: Vec<Extent> = extents
            .iter()
            .map(|&(block, len, start, kind)| Extent::new(block, len, start, kind))
            .collect();
        let delta = self.reserialize(fs, &extents, &old_external)?;

        let (_new, new_external) = self.flatten(fs)?;
        let count = u32::try_from(new_external.len())
            .map_err(|_| Error::with_message(Errno::EOVERFLOW, "too many extent leaf blocks"))?;
        Ok((count, delta))
    }
}

/// Maximum extents in the inline (depth-0) root: the 60-byte `i_block` holds a
/// 12-byte header plus four 12-byte entries.
const INLINE_MAX: usize = 4;

/// Maximum extents in one full-block external leaf node.
const LEAF_MAX: usize = (BLOCK_SIZE - ENTRY_SIZE) / ENTRY_SIZE;

/// The metadata (index/leaf) blocks a tree mutation allocated and freed, so the
/// caller can keep the inode's `i_blocks` accounting correct.
pub(super) struct TreeDelta {
    /// Number of extent metadata blocks allocated by the mutation.
    pub(super) meta_allocated: u32,
    /// Number of extent metadata blocks freed by the mutation.
    pub(super) meta_freed: u32,
    /// Newly allocated extent-node blocks whose bitmap groups must be synced.
    pub(super) allocated: Vec<Ext4Bid>,
    /// Obsolete extent-node blocks that become free only after the new inode root commits.
    pub(super) deferred_frees: Vec<Ext4Bid>,
}

impl TreeDelta {
    fn empty() -> Self {
        Self {
            meta_allocated: 0,
            meta_freed: 0,
            allocated: Vec::new(),
            deferred_frees: Vec::new(),
        }
    }

    fn absorb(&mut self, other: Self) -> Result<()> {
        self.meta_allocated = self
            .meta_allocated
            .checked_add(other.meta_allocated)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.meta_freed = self
            .meta_freed
            .checked_add(other.meta_freed)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.allocated.extend(other.allocated);
        self.deferred_frees.extend(other.deferred_frees);
        Ok(())
    }
}

impl ExtentTree {
    /// Parses all leaf extents and returns every external metadata block.
    #[cfg(ktest)]
    fn flatten(&self, io: &(impl ExtentBlockIo + ?Sized)) -> Result<(Vec<Extent>, Vec<Ext4Bid>)> {
        #[cfg(ktest)]
        self.full_scan_count.fetch_add(1, Ordering::Relaxed);
        let root_bytes = self.root.as_bytes();
        let header = Self::node_header(root_bytes)?;

        if header.is_leaf() {
            return Ok((Self::leaf_extents(root_bytes, header)?, Vec::new()));
        }

        let root_indexes = Self::node_indexes(root_bytes, header)?;
        let mut extents = Vec::new();
        let mut external = Vec::new();
        for index in root_indexes {
            Self::flatten_external(
                io,
                index.leaf(),
                header.depth() - 1,
                &mut extents,
                &mut external,
            )?;
        }
        Ok((extents, external))
    }

    #[cfg(ktest)]
    fn flatten_external(
        io: &(impl ExtentBlockIo + ?Sized),
        bid: Ext4Bid,
        expected_depth: u16,
        extents: &mut Vec<Extent>,
        external: &mut Vec<Ext4Bid>,
    ) -> Result<()> {
        let block = io.read_extent_block(bid)?;
        let header = Self::node_header(&block)?;
        if header.depth() != expected_depth {
            return_errno_with_message!(Errno::EUCLEAN, "extent tree depth mismatch");
        }
        external.push(bid);
        if header.is_leaf() {
            extents.extend(Self::leaf_extents(&block, header)?);
            return Ok(());
        }
        for index in Self::node_indexes(&block, header)? {
            Self::flatten_external(io, index.leaf(), expected_depth - 1, extents, external)?;
        }
        Ok(())
    }

    /// Sorts `extents` by logical block and coalesces runs that are logically and
    /// physically contiguous and share the same written/unwritten state.
    fn merge_extents(extents: &mut Vec<Extent>) {
        extents.sort_by_key(|e| e.block());
        let mut merged: Vec<Extent> = Vec::with_capacity(extents.len());
        for e in extents.iter() {
            if let Some(last) = merged.last() {
                // Unwritten extents cap one below the written limit: the length is
                // bias-encoded as `len + MAX_WRITTEN_LEN`, so an unwritten run of
                // `MAX_WRITTEN_LEN` would overflow the encoded `ee_len` value.
                let max_len = if last.is_unwritten() {
                    u32::from(MAX_WRITTEN_LEN) - 1
                } else {
                    u32::from(MAX_WRITTEN_LEN)
                };
                let contiguous = last.block() as u64 + last.len() as u64 == e.block() as u64
                    && last.start() + last.len() as u64 == e.start()
                    && last.is_unwritten() == e.is_unwritten()
                    && u32::from(last.len()) + u32::from(e.len()) <= max_len;
                if contiguous {
                    *merged.last_mut().unwrap() = Extent::new(
                        last.block(),
                        last.len() + e.len(),
                        last.start(),
                        last.kind(),
                    );
                    continue;
                }
            }
            merged.push(*e);
        }
        *extents = merged;
    }

    /// Test-only whole-tree serializer used as a structural oracle.
    #[cfg(ktest)]
    fn reserialize(
        &mut self,
        fs: &Ext4,
        extents: &[Extent],
        old_external: &[Ext4Bid],
    ) -> Result<TreeDelta> {
        if extents.len() <= INLINE_MAX {
            self.write_inline_leaf_root(extents)?;
            return Ok(TreeDelta {
                meta_allocated: 0,
                meta_freed: u32::try_from(old_external.len()).map_err(|_| {
                    Error::with_message(Errno::EOVERFLOW, "too many obsolete extent nodes")
                })?,
                allocated: Vec::new(),
                deferred_frees: old_external.to_vec(),
            });
        }

        let mut newly_allocated: Vec<Ext4Bid> = Vec::new();
        let goal = extents.first().map(|e| e.start()).unwrap_or(0);
        let result = (|| {
            let mut entries = Vec::with_capacity(extents.len().div_ceil(LEAF_MAX));
            for chunk in extents.chunks(LEAF_MAX) {
                let bid = Self::alloc_one(
                    fs,
                    goal,
                    &mut newly_allocated,
                    &mut None,
                    self.allocation_observer.as_deref(),
                )?;
                Self::write_leaf_node(fs, bid, chunk)?;
                entries.push(RawExtentIdx::new(chunk[0].block(), bid)?);
            }

            let mut depth = 1;
            while entries.len() > INLINE_MAX {
                if depth == 5 {
                    return_errno_with_message!(Errno::EFBIG, "extent tree reached maximum depth");
                }
                let mut parents = Vec::with_capacity(entries.len().div_ceil(LEAF_MAX));
                for chunk in entries.chunks(LEAF_MAX) {
                    let bid = Self::alloc_one(
                        fs,
                        goal,
                        &mut newly_allocated,
                        &mut None,
                        self.allocation_observer.as_deref(),
                    )?;
                    Self::write_index_node(fs, bid, chunk, depth)?;
                    parents.push(RawExtentIdx::new(chunk[0].block, bid)?);
                }
                entries = parents;
                depth += 1;
            }
            let meta_allocated = u32::try_from(newly_allocated.len()).map_err(|_| {
                Error::with_message(Errno::EOVERFLOW, "too many extent metadata blocks")
            })?;
            self.write_inline_index_root(&entries, depth);
            Ok(meta_allocated)
        })();
        let meta_allocated = match result {
            Ok(meta_allocated) => meta_allocated,
            Err(err) => {
                Self::rollback_allocations(fs, &newly_allocated, &mut None);
                return Err(err);
            }
        };

        Ok(TreeDelta {
            meta_allocated,
            meta_freed: u32::try_from(old_external.len()).map_err(|_| {
                Error::with_message(Errno::EOVERFLOW, "too many obsolete extent nodes")
            })?,
            allocated: newly_allocated,
            deferred_frees: old_external.to_vec(),
        })
    }

    /// Serializes `extents` into a full-block external leaf node at `bid`.
    fn write_leaf_node(
        io: &(impl ExtentBlockIo + ?Sized),
        bid: Ext4Bid,
        extents: &[Extent],
    ) -> Result<()> {
        if extents.is_empty() || extents.len() > LEAF_MAX {
            return_errno_with_message!(Errno::EINVAL, "invalid external extent leaf size");
        }
        let mut block = [0u8; BLOCK_SIZE];
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: u16::try_from(extents.len()).expect("leaf extent count fits u16"),
            max: u16::try_from(LEAF_MAX).expect("leaf capacity fits u16"),
            depth: 0,
            generation: 0,
        };
        block[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        for (i, ext) in extents.iter().enumerate() {
            let off = ENTRY_SIZE * (1 + i);
            block[off..off + ENTRY_SIZE].copy_from_slice(RawExtent::try_from(ext)?.as_bytes());
        }
        io.write_extent_block(bid, &block)?;
        Ok(())
    }

    /// Writes a depth-0 inline leaf root (header + up to [`INLINE_MAX`] extents)
    /// into the inode's 60-byte `i_block`.
    fn write_inline_leaf_root(&mut self, extents: &[Extent]) -> Result<()> {
        let bytes = self.root.as_mut_bytes();
        bytes.fill(0);
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: u16::try_from(extents.len()).expect("inline extent count fits u16"),
            max: u16::try_from(INLINE_MAX).expect("inline capacity fits u16"),
            depth: 0,
            generation: 0,
        };
        bytes[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        for (i, ext) in extents.iter().enumerate() {
            let off = ENTRY_SIZE * (1 + i);
            bytes[off..off + ENTRY_SIZE].copy_from_slice(RawExtent::try_from(ext)?.as_bytes());
        }
        Ok(())
    }

    fn write_index_node(
        io: &(impl ExtentBlockIo + ?Sized),
        bid: Ext4Bid,
        entries: &[RawExtentIdx],
        depth: u16,
    ) -> Result<()> {
        if entries.is_empty() || entries.len() > LEAF_MAX || depth == 0 {
            return_errno_with_message!(Errno::EINVAL, "invalid external extent index size");
        }
        let mut block = [0u8; BLOCK_SIZE];
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: u16::try_from(entries.len()).expect("index count fits u16"),
            max: u16::try_from(LEAF_MAX).expect("index capacity fits u16"),
            depth,
            generation: 0,
        };
        block[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        for (i, idx) in entries.iter().enumerate() {
            let off = ENTRY_SIZE * (1 + i);
            block[off..off + ENTRY_SIZE].copy_from_slice(idx.as_bytes());
        }
        io.write_extent_block(bid, &block)?;
        Ok(())
    }

    /// Writes an index root into the inode's 60-byte `i_block`.
    fn write_inline_index_root(&mut self, entries: &[RawExtentIdx], depth: u16) {
        debug_assert!(!entries.is_empty() && entries.len() <= INLINE_MAX && depth > 0);
        let bytes = self.root.as_mut_bytes();
        bytes.fill(0);
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: u16::try_from(entries.len()).expect("inline index count fits u16"),
            max: u16::try_from(INLINE_MAX).expect("inline capacity fits u16"),
            depth,
            generation: 0,
        };
        bytes[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        for (i, idx) in entries.iter().enumerate() {
            let off = ENTRY_SIZE * (1 + i);
            bytes[off..off + ENTRY_SIZE].copy_from_slice(idx.as_bytes());
        }
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;
    use crate::fs::{
        fs_impls::ext4::test_utils::Ext4FixtureBuilder,
        nv_pool::{
            config::{NvPoolConfig, NvSimMode},
            dram::DramNvBackend,
            frame::NvPool,
            id::NvPageId,
        },
    };

    fn nv_pool() -> Arc<NvPool> {
        let config = NvPoolConfig::from_bytes(
            Some(64 * PAGE_SIZE),
            Some(64 * PAGE_SIZE),
            Some(0),
            Some(NvSimMode::Fast),
        )
        .unwrap()
        .unwrap();
        let backend = Arc::new(DramNvBackend::allocate(config).unwrap());
        Arc::new(NvPool::format(backend, config, None).unwrap())
    }

    /// Writes a depth-0 extent root (header + extents) into a 60-byte `i_block`.
    fn inline_root(extents: &[RawExtent]) -> [u32; RAW_BLOCK_PTRS_LEN] {
        let mut block = [0u32; RAW_BLOCK_PTRS_LEN];
        let bytes = block.as_mut_bytes();
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: extents.len() as u16,
            max: 4,
            depth: 0,
            generation: 0,
        };
        bytes[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        for (i, extent) in extents.iter().enumerate() {
            let off = ENTRY_SIZE * (1 + i);
            bytes[off..off + ENTRY_SIZE].copy_from_slice(extent.as_bytes());
        }
        block
    }

    #[ktest]
    fn inline_single_extent_lookup() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let device = f.ext4.block_device().as_ref();
        // One extent mapping logical 0..4 to physical 100..104.
        let tree = ExtentTree::try_from_root(inline_root(&[RawExtent {
            block: 0,
            len: 4,
            start_hi: 1,
            start_lo: 100,
        }]))
        .unwrap();

        let mapped = tree.find(device, 2).unwrap().unwrap();
        assert_eq!(mapped.start(), (1 << 32) | 100);
        assert_eq!(mapped.block(), 0);

        // Block 4 is beyond the extent: a hole.
        assert!(tree.find(device, 4).unwrap().is_none());
    }

    #[ktest]
    fn unwritten_conversion_is_path_local() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let mut tree = ExtentTree::try_from_root(inline_root(&[RawExtent {
            block: 0,
            len: MAX_WRITTEN_LEN + 8,
            start_hi: 0,
            start_lo: 500,
        }]))
        .unwrap();

        tree.convert_unwritten(&f.ext4, 2, 3).unwrap();

        assert_eq!(tree.full_scan_count(), 0);
        let head = tree.find(&*f.ext4, 0).unwrap().unwrap();
        let middle = tree.find(&*f.ext4, 2).unwrap().unwrap();
        let tail = tree.find(&*f.ext4, 5).unwrap().unwrap();
        assert_eq!(
            (head.block(), head.len(), head.kind()),
            (0, 2, ExtentKind::Unwritten)
        );
        assert_eq!(
            (middle.block(), middle.len(), middle.start(), middle.kind()),
            (2, 3, 502, ExtentKind::Written)
        );
        assert_eq!(
            (tail.block(), tail.len(), tail.start(), tail.kind()),
            (5, 3, 505, ExtentKind::Unwritten)
        );
    }

    #[ktest]
    fn tier_publish_is_path_local() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let mut tree = ExtentTree::try_from_root(inline_root(&[RawExtent {
            block: 0,
            len: 8,
            start_hi: 0,
            start_lo: 500,
        }]))
        .unwrap();
        let page = NvPageId::try_from(7).unwrap();

        let (_, old_nv) = tree
            .publish_tiered(&f.ext4, 2, 3, TierBlock::Nv(page))
            .unwrap();
        assert!(old_nv.is_empty());
        assert_eq!(tree.full_scan_count(), 0);
        assert_eq!(
            tree.find(&*f.ext4, 2).unwrap().unwrap().tier_start(),
            TierBlock::Nv(page)
        );

        let (_, old_nv) = tree
            .publish_tiered(&f.ext4, 2, 3, TierBlock::Ssd(502))
            .unwrap();
        assert_eq!(old_nv.len(), 3);
        assert_eq!(tree.full_scan_count(), 0);
        let merged = tree.find(&*f.ext4, 0).unwrap().unwrap();
        assert_eq!((merged.block(), merged.len(), merged.start()), (0, 8, 500));
    }

    #[ktest]
    fn tier_publish_splits_full_external_leaf_with_ssd_allocation_goal() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let leaf_bid = f.ext4.alloc_blocks(1, 0).unwrap().start;
        let extents: Vec<_> = (0..LEAF_MAX as u32)
            .map(|entry| RawExtent {
                block: entry * 4,
                len: 3,
                start_hi: 0,
                start_lo: 1000 + entry * 4,
            })
            .collect();
        f.write_data_block(leaf_bid as u32, &leaf_node(&extents));
        let mut tree = ExtentTree::try_from_root(index_root_for(&[(0, leaf_bid as u32)])).unwrap();
        let page = NvPageId::try_from(7).unwrap();

        let (delta, old_nv) = tree
            .publish_tiered(&f.ext4, 1, 1, TierBlock::Nv(page))
            .unwrap();

        assert!(old_nv.is_empty());
        assert_eq!(delta.meta_allocated, 2);
        assert_eq!(delta.meta_freed, 1);
        assert_eq!(tree.full_scan_count(), 0);
        assert_eq!(
            tree.find(&*f.ext4, 1).unwrap().unwrap().tier_start(),
            TierBlock::Nv(page)
        );
        assert_eq!(tree.find(&*f.ext4, 4).unwrap().unwrap().start(), 1004);
    }

    #[ktest]
    fn nv_deep_left_leaf_mutation_falls_back_to_cow() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .with_nv_pool(nv_pool())
            .build()
            .unwrap();
        let leaf_bid = f.ext4.alloc_blocks(1, 0).unwrap().start;
        let parent_bid = f.ext4.alloc_blocks(1, 0).unwrap().start;
        let extents: Vec<_> = (0..LEAF_MAX as u32)
            .map(|entry| RawExtent {
                block: entry * 4,
                len: 3,
                start_hi: 0,
                start_lo: 1000 + entry * 4,
            })
            .collect();
        f.write_data_block(leaf_bid as u32, &leaf_node(&extents));
        ExtentTree::write_index_node(
            f.ext4.block_device().as_ref(),
            parent_bid,
            &[RawExtentIdx::new(0, leaf_bid).unwrap()],
            1,
        )
        .unwrap();
        let mut tree = ExtentTree::try_from_root(index_root_at_depth(
            &[RawExtentIdx::new(0, parent_bid).unwrap()],
            2,
        ))
        .unwrap();
        tree.bind_owner(17);
        let page = NvPageId::try_from(7).unwrap();

        let (delta, old_nv) = tree
            .publish_tiered(&f.ext4, 1, 1, TierBlock::Nv(page))
            .unwrap();

        assert!(old_nv.is_empty());
        assert_eq!(delta.meta_allocated, 2);
        assert_eq!(delta.meta_freed, 1);
        assert_eq!(delta.deferred_frees, vec![leaf_bid]);
        assert!(
            PendingExtentMutation::load(f.ext4.nv_state().unwrap(), 17)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            tree.find(&*f.ext4, 1).unwrap().unwrap().tier_start(),
            TierBlock::Nv(page)
        );
        assert_eq!(
            tree.find(&*f.ext4, (LEAF_MAX as u32 - 1) * 4)
                .unwrap()
                .unwrap()
                .start(),
            1000 + (LEAF_MAX as u64 - 1) * 4
        );
    }

    #[ktest]
    fn nv_deep_right_leaf_mutation_uses_parent_publish_protocol() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .with_nv_pool(nv_pool())
            .build()
            .unwrap();
        let leaf_bid = f.ext4.alloc_blocks(1, 0).unwrap().start;
        let parent_bid = f.ext4.alloc_blocks(1, 0).unwrap().start;
        let extents: Vec<_> = (0..LEAF_MAX as u32)
            .map(|entry| RawExtent {
                block: entry * 4,
                len: 3,
                start_hi: 0,
                start_lo: 1000 + entry * 4,
            })
            .collect();
        f.write_data_block(leaf_bid as u32, &leaf_node(&extents));
        ExtentTree::write_index_node(
            f.ext4.block_device().as_ref(),
            parent_bid,
            &[RawExtentIdx::new(0, leaf_bid).unwrap()],
            1,
        )
        .unwrap();
        let mut tree = ExtentTree::try_from_root(index_root_at_depth(
            &[RawExtentIdx::new(0, parent_bid).unwrap()],
            2,
        ))
        .unwrap();
        tree.bind_owner(17);
        let page = NvPageId::try_from(7).unwrap();
        let iblock = (LEAF_MAX as u32 - 1) * 4 + 1;

        let (delta, old_nv) = tree
            .publish_tiered(&f.ext4, iblock, 1, TierBlock::Nv(page))
            .unwrap();

        assert!(old_nv.is_empty());
        assert_eq!(delta.meta_allocated, 1);
        assert_eq!(delta.meta_freed, 0);
        assert!(delta.deferred_frees.is_empty());
        assert!(
            PendingExtentMutation::load(f.ext4.nv_state().unwrap(), 17)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            tree.find(&*f.ext4, iblock).unwrap().unwrap().tier_start(),
            TierBlock::Nv(page)
        );
    }

    #[ktest]
    fn recovers_parent_published_leaf_split_idempotently() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .with_nv_pool(nv_pool())
            .build()
            .unwrap();
        let old_child = f.ext4.alloc_blocks(1, 0).unwrap().start;
        let new_child = f.ext4.alloc_blocks(1, 0).unwrap().start;
        let parent = f.ext4.alloc_blocks(1, 0).unwrap().start;
        let old_extents = [
            RawExtent {
                block: 0,
                len: 2,
                start_hi: 0,
                start_lo: 300,
            },
            RawExtent {
                block: 4,
                len: 2,
                start_hi: 0,
                start_lo: 400,
            },
        ];
        f.write_data_block(old_child as u32, &leaf_node(&old_extents));
        f.write_data_block(new_child as u32, &leaf_node(&old_extents[1..]));
        ExtentTree::write_index_node(
            &*f.ext4,
            parent,
            &[
                RawExtentIdx::new(0, old_child).unwrap(),
                RawExtentIdx::new(4, new_child).unwrap(),
            ],
            1,
        )
        .unwrap();
        // Simulate power loss after the parent commit but before the marker's
        // phase update. Recovery must inspect the authoritative parent rather
        // than trust the marker phase alone.
        let mutation = PendingExtentMutation::split(17, parent, old_child, new_child, 4, 2);
        mutation.persist(f.ext4.nv_state().unwrap()).unwrap();

        ExtentTree::recover_pending_mutation(&f.ext4, &mutation).unwrap();
        ExtentTree::recover_pending_mutation(&f.ext4, &mutation).unwrap();

        assert!(
            PendingExtentMutation::load(f.ext4.nv_state().unwrap(), 17)
                .unwrap()
                .is_none()
        );
        let tree = ExtentTree::try_from_root(index_root_at_depth(
            &[RawExtentIdx::new(0, parent).unwrap()],
            2,
        ))
        .unwrap();
        assert_eq!(tree.find(&*f.ext4, 0).unwrap().unwrap().start(), 300);
        assert_eq!(tree.find(&*f.ext4, 4).unwrap().unwrap().start(), 400);
        let old = f.ext4.read_metadata_block(old_child).unwrap();
        let header = RawExtentHeader::from_bytes(&old[..ENTRY_SIZE]);
        assert_eq!((header.depth, header.entries), (0, 1));
    }

    #[ktest]
    fn recovers_prepared_leaf_split_by_restoring_old_tree() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .with_nv_pool(nv_pool())
            .build()
            .unwrap();
        let old_child = f.ext4.alloc_blocks(1, 0).unwrap().start;
        let new_child = f.ext4.alloc_blocks(1, 0).unwrap().start;
        let parent = f.ext4.alloc_blocks(1, 0).unwrap().start;
        let old_extents = [
            Extent::new(0, 2, 300, ExtentKind::Written),
            Extent::new(4, 2, 400, ExtentKind::Written),
        ];
        ExtentTree::write_leaf_node(&*f.ext4, old_child, &old_extents).unwrap();
        ExtentTree::write_leaf_node(&*f.ext4, new_child, &old_extents[1..]).unwrap();
        ExtentTree::write_index_node(
            &*f.ext4,
            parent,
            &[RawExtentIdx::new(0, old_child).unwrap()],
            1,
        )
        .unwrap();
        let mutation = PendingExtentMutation::split(17, parent, old_child, new_child, 4, 2);
        mutation.persist(f.ext4.nv_state().unwrap()).unwrap();
        let free_before_recovery = f.ext4.super_block().free_blocks_count();

        ExtentTree::recover_pending_mutation(&f.ext4, &mutation).unwrap();

        assert!(
            PendingExtentMutation::load(f.ext4.nv_state().unwrap(), 17)
                .unwrap()
                .is_none()
        );
        assert!(
            f.ext4
                .nv_state()
                .unwrap()
                .resident_frame(new_child)
                .is_none()
        );
        assert_eq!(
            f.ext4.super_block().free_blocks_count(),
            free_before_recovery + 1
        );
        let tree = ExtentTree::try_from_root(index_root_at_depth(
            &[RawExtentIdx::new(0, parent).unwrap()],
            2,
        ))
        .unwrap();
        assert_eq!(tree.find(&*f.ext4, 0).unwrap().unwrap().start(), 300);
        assert_eq!(tree.find(&*f.ext4, 4).unwrap().unwrap().start(), 400);
    }

    #[ktest]
    fn range_removal_unlinks_an_empty_external_leaf() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        for (bid, block, start) in [(200, 0, 300), (201, 4, 400), (202, 8, 500)] {
            f.write_data_block(
                bid,
                &leaf_node(&[RawExtent {
                    block,
                    len: 2,
                    start_hi: 0,
                    start_lo: start,
                }]),
            );
        }
        let mut tree =
            ExtentTree::try_from_root(index_root_for(&[(0, 200), (4, 201), (8, 202)])).unwrap();

        let (delta, removed) = tree.remove_range(&f.ext4, 4..6).unwrap();

        assert_eq!(removed.len(), 1);
        assert_eq!((removed[0].block(), removed[0].len()), (4, 2));
        assert_eq!(delta.meta_allocated, 0);
        assert_eq!(delta.meta_freed, 1);
        assert_eq!(delta.deferred_frees, vec![201]);
        assert_eq!(root_header(&tree.root()), (1, 2));
        assert!(tree.find(&*f.ext4, 4).unwrap().is_none());
        assert_eq!(tree.find(&*f.ext4, 8).unwrap().unwrap().start(), 500);
        assert_eq!(tree.full_scan_count(), 0);
    }

    /// Writes a depth-1 index root into a 60-byte `i_block`, pointing at a single
    /// external leaf node at physical block `leaf_block`.
    fn index_root(leaf_block: u32) -> [u32; RAW_BLOCK_PTRS_LEN] {
        index_root_for(&[(0, leaf_block)])
    }

    fn index_root_for(entries: &[(Iblock, u32)]) -> [u32; RAW_BLOCK_PTRS_LEN] {
        let mut block = [0u32; RAW_BLOCK_PTRS_LEN];
        let bytes = block.as_mut_bytes();
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: entries.len() as u16,
            max: 4,
            depth: 1,
            generation: 0,
        };
        bytes[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        for (i, &(iblock, leaf_block)) in entries.iter().enumerate() {
            let idx = RawExtentIdx {
                block: iblock,
                leaf_lo: leaf_block,
                leaf_hi: 0,
                unused: 0,
            };
            let offset = ENTRY_SIZE * (i + 1);
            bytes[offset..offset + ENTRY_SIZE].copy_from_slice(idx.as_bytes());
        }
        block
    }

    /// Builds a full-block external leaf node (depth 0) from `extents`.
    fn leaf_node(extents: &[RawExtent]) -> [u8; BLOCK_SIZE] {
        let mut block = [0u8; BLOCK_SIZE];
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: extents.len() as u16,
            max: ((BLOCK_SIZE / ENTRY_SIZE) - 1) as u16,
            depth: 0,
            generation: 0,
        };
        block[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        for (i, extent) in extents.iter().enumerate() {
            let off = ENTRY_SIZE * (1 + i);
            block[off..off + ENTRY_SIZE].copy_from_slice(extent.as_bytes());
        }
        block
    }

    /// A depth-1 tree (index root → external leaf read from the device) must be
    /// descended into. This exercises the interior-node read path that inline
    /// (depth-0) roots never reach — the real-image counterpart is a fragmented
    /// file whose extents overflow the inline root.
    #[ktest]
    fn descends_into_external_leaf() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let device = f.ext4.block_device().as_ref();

        let leaf_block = 200u32;
        let leaf = leaf_node(&[
            RawExtent {
                block: 0,
                len: 2,
                start_hi: 0,
                start_lo: 300,
            },
            RawExtent {
                block: 5,
                len: 3,
                start_hi: 0,
                start_lo: 400,
            },
        ]);
        f.write_data_block(leaf_block, &leaf);
        let tree = ExtentTree::try_from_root(index_root(leaf_block)).unwrap();

        // Logical 1 → descend to the leaf → first extent (0..2) → physical 301.
        let m0 = tree.find(device, 1).unwrap().unwrap();
        assert_eq!(m0.start() + (1 - m0.block()) as u64, 301);
        // Logical 6 → second extent (5..8) → physical 401.
        let m1 = tree.find(device, 6).unwrap().unwrap();
        assert_eq!(m1.start() + (6 - m1.block()) as u64, 401);
        // Logical 3 → gap between the leaf's extents → hole.
        assert!(tree.find(device, 3).unwrap().is_none());
        // Logical 100 → beyond all extents → hole.
        assert!(tree.find(device, 100).unwrap().is_none());
    }

    #[ktest]
    fn external_leaf_update_is_visible_from_nv_before_ssd_eviction() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_nv_pool(nv_pool())
            .build()
            .unwrap();
        let leaf_block = 200u32;
        let old_leaf = leaf_node(&[RawExtent {
            block: 0,
            len: 1,
            start_hi: 0,
            start_lo: 300,
        }]);
        f.write_data_block(leaf_block, &old_leaf);

        let new_extent = Extent::try_from(&RawExtent {
            block: 0,
            len: 2,
            start_hi: 0,
            start_lo: 400,
        })
        .unwrap();
        ExtentTree::write_leaf_node(f.ext4.as_ref(), leaf_block.into(), &[new_extent]).unwrap();

        let mut ssd = [0; BLOCK_SIZE];
        f.disk
            .segment()
            .read_bytes(leaf_block as usize * BLOCK_SIZE, &mut ssd)
            .unwrap();
        assert_eq!(ssd, old_leaf);

        let tree = ExtentTree::try_from_root(index_root(leaf_block)).unwrap();
        let mapped = tree.find(f.ext4.as_ref(), 1).unwrap().unwrap();
        assert_eq!(mapped.start() + 1, 401);
    }

    #[ktest]
    fn extent_path_reports_covered_and_gap_neighbors() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let device = f.ext4.block_device().as_ref();
        let tree = ExtentTree::try_from_root(inline_root(&[
            RawExtent {
                block: 2,
                len: 1,
                start_hi: 0,
                start_lo: 100,
            },
            RawExtent {
                block: 7,
                len: 2,
                start_hi: 0,
                start_lo: 200,
            },
        ]))
        .unwrap();

        let covered = tree.lookup_path(device, 8).unwrap();
        assert_eq!(covered.depth(), 0);
        assert_eq!(covered.covered().unwrap().block(), 7);

        let gap = tree.lookup_path(device, 5).unwrap();
        assert_eq!(gap.predecessor().unwrap().block(), 2);
        assert_eq!(gap.successor().unwrap().block(), 7);
        assert_eq!(gap.hole_range(), Some(3..7));
    }

    #[ktest]
    fn extent_path_finds_neighbors_across_leaf_boundary() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        f.write_data_block(
            200,
            &leaf_node(&[RawExtent {
                block: 0,
                len: 3,
                start_hi: 0,
                start_lo: 100,
            }]),
        );
        f.write_data_block(
            201,
            &leaf_node(&[RawExtent {
                block: 8,
                len: 2,
                start_hi: 0,
                start_lo: 300,
            }]),
        );
        let tree = ExtentTree::try_from_root(index_root_for(&[(0, 200), (8, 201)])).unwrap();

        let gap = tree.lookup_path(f.ext4.block_device().as_ref(), 5).unwrap();
        assert_eq!(gap.predecessor().unwrap().block(), 0);
        assert_eq!(gap.successor().unwrap().block(), 8);
        assert_eq!(gap.hole_range(), Some(3..8));
    }

    #[ktest]
    fn next_extent_streams_across_leaf_boundary_without_full_scan() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        f.write_data_block(
            200,
            &leaf_node(&[RawExtent {
                block: 0,
                len: 3,
                start_hi: 0,
                start_lo: 100,
            }]),
        );
        f.write_data_block(
            201,
            &leaf_node(&[RawExtent {
                block: 8,
                len: 2,
                start_hi: 0,
                start_lo: 300,
            }]),
        );
        let tree = ExtentTree::try_from_root(index_root_for(&[(0, 200), (8, 201)])).unwrap();

        let next = tree.next_extent_at_or_after(&*f.ext4, 3).unwrap().unwrap();

        assert_eq!((next.block(), next.len(), next.start()), (8, 2, 300));
        assert_eq!(tree.full_scan_count(), 0);
        assert!(
            tree.next_extent_at_or_after(&*f.ext4, 10)
                .unwrap()
                .is_none()
        );
    }

    #[ktest]
    fn extent_path_matches_legacy_lookup_on_depth1_tree() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        f.write_data_block(
            200,
            &leaf_node(&[RawExtent {
                block: 0,
                len: 3,
                start_hi: 0,
                start_lo: 100,
            }]),
        );
        f.write_data_block(
            201,
            &leaf_node(&[RawExtent {
                block: 8,
                len: 2,
                start_hi: 0,
                start_lo: 300,
            }]),
        );
        let tree = ExtentTree::try_from_root(index_root_for(&[(0, 200), (8, 201)])).unwrap();
        let device = f.ext4.block_device().as_ref();

        for iblock in 0..12 {
            let path = tree
                .find(device, iblock)
                .unwrap()
                .map(|extent| (extent.block(), extent.len(), extent.start(), extent.kind()));
            let legacy = tree
                .legacy_find(device, iblock)
                .unwrap()
                .map(|extent| (extent.block(), extent.len(), extent.start(), extent.kind()));
            assert_eq!(path, legacy, "lookup mismatch at logical block {iblock}");
        }
    }

    #[ktest]
    fn inserts_into_inline_leaf_without_split() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let mut tree = ExtentTree::try_from_root(inline_root(&[
            RawExtent {
                block: 0,
                len: 1,
                start_hi: 0,
                start_lo: 100,
            },
            RawExtent {
                block: 4,
                len: 1,
                start_hi: 0,
                start_lo: 400,
            },
        ]))
        .unwrap();

        assert_eq!(
            tree.try_insert_in_place(&f.ext4, 2, 250, 1, ExtentKind::Written)
                .unwrap(),
            InPlaceInsert::Inserted
        );
        assert_eq!(root_header(&tree.root()), (0, 3));
        assert_eq!(
            tree.find(f.ext4.block_device().as_ref(), 2)
                .unwrap()
                .unwrap()
                .start(),
            250
        );
    }

    #[ktest]
    fn in_place_insert_merges_both_adjacent_extents() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let mut tree = ExtentTree::try_from_root(inline_root(&[
            RawExtent {
                block: 0,
                len: 1,
                start_hi: 0,
                start_lo: 100,
            },
            RawExtent {
                block: 2,
                len: 1,
                start_hi: 0,
                start_lo: 102,
            },
        ]))
        .unwrap();

        assert_eq!(
            tree.try_insert_in_place(&f.ext4, 1, 101, 1, ExtentKind::Written)
                .unwrap(),
            InPlaceInsert::Inserted
        );
        assert_eq!(root_header(&tree.root()), (0, 1));
        let merged = tree
            .find(f.ext4.block_device().as_ref(), 2)
            .unwrap()
            .unwrap();
        assert_eq!(merged.block(), 0);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged.start(), 100);
    }

    #[ktest]
    fn external_leaf_insert_updates_first_ancestor_key() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let leaf_block = 200u32;
        f.write_data_block(
            leaf_block,
            &leaf_node(&[RawExtent {
                block: 10,
                len: 1,
                start_hi: 0,
                start_lo: 300,
            }]),
        );
        let mut root = index_root(leaf_block);
        root.as_mut_bytes()[ENTRY_SIZE..ENTRY_SIZE + size_of::<u32>()]
            .copy_from_slice(10u32.as_bytes());
        let mut tree = ExtentTree::try_from_root(root).unwrap();

        assert_eq!(
            tree.try_insert_in_place(&f.ext4, 5, 250, 1, ExtentKind::Written)
                .unwrap(),
            InPlaceInsert::Inserted
        );
        let root_idx = ExtentIdx::from(&RawExtentIdx::from_bytes(
            &tree.root().as_bytes()[ENTRY_SIZE..2 * ENTRY_SIZE],
        ));
        assert_eq!(root_idx.block(), 5);
        assert_eq!(
            tree.find(f.ext4.block_device().as_ref(), 10)
                .unwrap()
                .unwrap()
                .start(),
            300
        );
    }

    #[ktest]
    fn deep_first_key_update_reserves_cow_path() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let leaf_bid = f.ext4.alloc_blocks(1, 0).unwrap().start;
        f.write_data_block(
            leaf_bid as u32,
            &leaf_node(&[RawExtent {
                block: 10,
                len: 1,
                start_hi: 0,
                start_lo: 300,
            }]),
        );
        let parent_bid = f.ext4.alloc_blocks(1, 0).unwrap().start;
        ExtentTree::write_index_node(
            f.ext4.block_device().as_ref(),
            parent_bid,
            &[RawExtentIdx::new(10, leaf_bid).unwrap()],
            1,
        )
        .unwrap();
        let root_entry = RawExtentIdx::new(10, parent_bid).unwrap();
        let mut tree = ExtentTree::try_from_root(index_root_at_depth(&[root_entry], 2)).unwrap();

        let metadata_needed = tree
            .metadata_blocks_needed_for_insert(f.ext4.block_device().as_ref(), 5)
            .unwrap();
        assert_eq!(metadata_needed, 2);
        let delta = {
            let mut reservation = f.ext4.reserve_extent_metadata(metadata_needed).unwrap();
            tree.insert_reserved(&f.ext4, 5, 250, 1, ExtentKind::Written, &mut reservation)
                .unwrap()
        };

        assert_eq!(delta.meta_allocated, 2);
        assert_eq!(delta.meta_freed, 2);
        assert_eq!(f.ext4.reserved_metadata_blocks(), 0);
        assert_eq!(
            tree.find(f.ext4.block_device().as_ref(), 5)
                .unwrap()
                .unwrap()
                .start(),
            250
        );
        let root_idx = ExtentIdx::from(&RawExtentIdx::from_bytes(
            &tree.root().as_bytes()[ENTRY_SIZE..2 * ENTRY_SIZE],
        ));
        assert_eq!(root_idx.block(), 5);
    }

    #[ktest]
    fn full_leaf_requires_split_without_modification() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let extents: Vec<_> = (0..INLINE_MAX as u32)
            .map(|i| RawExtent {
                block: i * 2,
                len: 1,
                start_hi: 0,
                start_lo: 100 + i * 10,
            })
            .collect();
        let root = inline_root(&extents);
        let mut tree = ExtentTree::try_from_root(root).unwrap();

        assert_eq!(
            tree.try_insert_in_place(&f.ext4, 9, 900, 1, ExtentKind::Written)
                .unwrap(),
            InPlaceInsert::NeedsSplit
        );
        assert_eq!(tree.root(), root);
    }

    #[ktest]
    fn full_root_propagates_leaf_split_by_deepening_tree() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut indexes = Vec::new();
        for leaf_nr in 0..INLINE_MAX as u32 {
            let leaf_bid = f.ext4.alloc_blocks(1, 0).unwrap().start;
            let logical_base = leaf_nr * LEAF_MAX as u32 * 2;
            let extents: Vec<_> = (0..LEAF_MAX as u32)
                .map(|entry| RawExtent {
                    block: logical_base + entry * 2,
                    len: 1,
                    start_hi: 0,
                    start_lo: 2000 + logical_base + entry * 3,
                })
                .collect();
            f.write_data_block(leaf_bid as u32, &leaf_node(&extents));
            indexes.push((logical_base, leaf_bid as u32));
        }
        let mut tree = ExtentTree::try_from_root(index_root_for(&indexes)).unwrap();
        let inserted_iblock = INLINE_MAX as u32 * LEAF_MAX as u32 * 2 + 1;

        let metadata_needed = tree
            .metadata_blocks_needed_for_insert(f.ext4.block_device().as_ref(), inserted_iblock)
            .unwrap();
        assert_eq!(metadata_needed, 3);
        let delta = {
            let mut reservation = f.ext4.reserve_extent_metadata(metadata_needed).unwrap();
            tree.insert_reserved(
                &f.ext4,
                inserted_iblock,
                7000,
                1,
                ExtentKind::Written,
                &mut reservation,
            )
            .unwrap()
        };
        assert_eq!(f.ext4.reserved_metadata_blocks(), 0);

        assert_eq!(root_header(&tree.root()), (2, 1));
        assert_eq!(delta.meta_allocated, 3);
        assert_eq!(delta.meta_freed, 1);
        assert_eq!(
            tree.find(f.ext4.block_device().as_ref(), inserted_iblock)
                .unwrap()
                .unwrap()
                .start(),
            7000
        );
        assert_eq!(
            tree.find(f.ext4.block_device().as_ref(), 0)
                .unwrap()
                .unwrap()
                .start(),
            2000
        );

        let rebuilt_extents: Vec<_> = tree
            .extents(f.ext4.block_device().as_ref())
            .unwrap()
            .into_iter()
            .map(|extent| (extent.block(), extent.len(), extent.start(), extent.kind()))
            .collect();
        assert_eq!(tree.rebuild(&f.ext4, &rebuilt_extents).unwrap().0, 6);
        assert_eq!(root_header(&tree.root()), (2, 1));
        let remounted = ExtentTree::try_from_root(tree.root()).unwrap();
        assert_eq!(
            remounted
                .find(f.ext4.block_device().as_ref(), inserted_iblock)
                .unwrap()
                .unwrap()
                .start(),
            7000
        );
    }

    #[ktest]
    fn full_external_parent_splits_toward_root() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut parent_entries = Vec::with_capacity(LEAF_MAX);
        let mut last_leaf = 0;
        let mut last_base = 0;
        for child_nr in 0..LEAF_MAX as u32 {
            let leaf_bid = f.ext4.alloc_blocks(1, 0).unwrap().start;
            let logical_base = child_nr * (LEAF_MAX as u32 * 2 + 2);
            let extents: Vec<_> = if child_nr + 1 == LEAF_MAX as u32 {
                (0..LEAF_MAX as u32)
                    .map(|entry| RawExtent {
                        block: logical_base + entry * 2,
                        len: 1,
                        start_hi: 0,
                        start_lo: 4000 + entry * 3,
                    })
                    .collect()
            } else {
                vec![RawExtent {
                    block: logical_base,
                    len: 1,
                    start_hi: 0,
                    start_lo: 3000 + child_nr,
                }]
            };
            f.write_data_block(leaf_bid as u32, &leaf_node(&extents));
            parent_entries.push(RawExtentIdx::new(logical_base, leaf_bid).unwrap());
            last_leaf = leaf_bid;
            last_base = logical_base;
        }
        let parent_bid = f.ext4.alloc_blocks(1, 0).unwrap().start;
        ExtentTree::write_index_node(
            f.ext4.block_device().as_ref(),
            parent_bid,
            &parent_entries,
            1,
        )
        .unwrap();
        let root_entry = RawExtentIdx::new(0, parent_bid).unwrap();
        let mut tree = ExtentTree::try_from_root(index_root_at_depth(&[root_entry], 2)).unwrap();
        let inserted_iblock = last_base + LEAF_MAX as u32 * 2 + 1;

        let metadata_needed = tree
            .metadata_blocks_needed_for_insert(f.ext4.block_device().as_ref(), inserted_iblock)
            .unwrap();
        assert_eq!(metadata_needed, 4);
        let delta = {
            let mut reservation = f.ext4.reserve_extent_metadata(metadata_needed).unwrap();
            tree.insert_reserved(
                &f.ext4,
                inserted_iblock,
                7000,
                1,
                ExtentKind::Written,
                &mut reservation,
            )
            .unwrap()
        };
        assert_eq!(f.ext4.reserved_metadata_blocks(), 0);

        assert_eq!(root_header(&tree.root()), (2, 2));
        assert_eq!(delta.meta_allocated, 4);
        assert_eq!(delta.meta_freed, 2);
        assert_eq!(
            tree.find(f.ext4.block_device().as_ref(), inserted_iblock)
                .unwrap()
                .unwrap()
                .start(),
            7000
        );
        assert_ne!(last_leaf, 0);
    }

    #[ktest]
    fn failed_leaf_split_rolls_back_new_metadata_and_root() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_free_blocks(1)
            .build()
            .unwrap();
        let leaf_bid = 200;
        let extents: Vec<_> = (0..LEAF_MAX as u32)
            .map(|entry| RawExtent {
                block: entry * 2,
                len: 1,
                start_hi: 0,
                start_lo: 500 + entry,
            })
            .collect();
        f.write_data_block(leaf_bid, &leaf_node(&extents));
        let root = index_root(leaf_bid);
        let mut tree = ExtentTree::try_from_root(root).unwrap();
        let free_before = f.ext4.super_block().free_blocks_count();

        assert!(
            tree.insert(
                &f.ext4,
                LEAF_MAX as u32 * 2 + 1,
                1800,
                1,
                ExtentKind::Written,
            )
            .is_err()
        );

        assert_eq!(tree.root(), root);
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before);
        assert_eq!(
            tree.find(f.ext4.block_device().as_ref(), 0)
                .unwrap()
                .unwrap()
                .start(),
            500
        );
    }

    #[ktest]
    fn failed_metadata_reservation_leaves_tree_unchanged() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_free_blocks(1)
            .build()
            .unwrap();
        let leaf_bid = 200;
        let extents: Vec<_> = (0..LEAF_MAX as u32)
            .map(|entry| RawExtent {
                block: entry * 2,
                len: 1,
                start_hi: 0,
                start_lo: 500 + entry,
            })
            .collect();
        f.write_data_block(leaf_bid, &leaf_node(&extents));
        let root = index_root(leaf_bid);
        let tree = ExtentTree::try_from_root(root).unwrap();
        let inserted_iblock = LEAF_MAX as u32 * 2 + 1;
        let free_before = f.ext4.super_block().free_blocks_count();

        let metadata_needed = tree
            .metadata_blocks_needed_for_insert(f.ext4.block_device().as_ref(), inserted_iblock)
            .unwrap();
        assert_eq!(metadata_needed, 2);
        let err = match f.ext4.reserve_extent_metadata(metadata_needed) {
            Ok(_) => panic!("metadata reservation unexpectedly succeeded"),
            Err(err) => err,
        };
        assert_eq!(err.error(), Errno::ENOSPC);

        assert_eq!(tree.root(), root);
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before);
        assert_eq!(f.ext4.reserved_metadata_blocks(), 0);
    }

    #[ktest]
    fn reserved_metadata_write_failure_restores_reservation_and_root() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let extents: Vec<_> = (0..INLINE_MAX as u32)
            .map(|entry| RawExtent {
                block: entry * 2,
                len: 1,
                start_hi: 0,
                start_lo: 100 + entry * 10,
            })
            .collect();
        let root = inline_root(&extents);
        let mut tree = ExtentTree::try_from_root(root).unwrap();
        let free_before = f.ext4.super_block().free_blocks_count();
        let mut reservation = f.ext4.reserve_extent_metadata(1).unwrap();

        f.disk.set_fail_writes(true);
        assert!(
            tree.insert_reserved(&f.ext4, 9, 900, 1, ExtentKind::Written, &mut reservation,)
                .is_err()
        );
        f.disk.set_fail_writes(false);

        assert_eq!(tree.root(), root);
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before);
        assert_eq!(f.ext4.reserved_metadata_blocks(), 1);
        drop(reservation);
        assert_eq!(f.ext4.reserved_metadata_blocks(), 0);
    }

    /// Returns the depth and entry count of the inline root header.
    fn root_header(root: &[u32; RAW_BLOCK_PTRS_LEN]) -> (u16, u16) {
        let hdr = ExtentHeader::try_from(&RawExtentHeader::from_bytes(
            &root.as_bytes()[0..ENTRY_SIZE],
        ))
        .unwrap();
        (hdr.depth(), hdr.entries())
    }

    fn index_root_at_depth(entries: &[RawExtentIdx], depth: u16) -> [u32; RAW_BLOCK_PTRS_LEN] {
        let mut root = [0u32; RAW_BLOCK_PTRS_LEN];
        let bytes = root.as_mut_bytes();
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: entries.len() as u16,
            max: INLINE_MAX as u16,
            depth,
            generation: 0,
        };
        bytes[..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        for (i, entry) in entries.iter().enumerate() {
            let off = ENTRY_SIZE * (i + 1);
            bytes[off..off + ENTRY_SIZE].copy_from_slice(entry.as_bytes());
        }
        root
    }

    #[ktest]
    fn inline_overflow_grows_to_depth1() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::try_from_root(inline_root(&[])).unwrap();

        // Five non-contiguous extents overflow the 4-entry inline root.
        let mut total_allocated = 0;
        for k in 0..5u32 {
            let d = tree
                .insert(&f.ext4, k * 2, 100 + k as u64 * 10, 1, ExtentKind::Written)
                .unwrap();
            total_allocated += d.meta_allocated;
        }

        // The root is now a depth-1 index with one external leaf.
        assert_eq!(root_header(&tree.root()), (1, 1));
        assert_eq!(total_allocated, 1); // exactly one leaf block allocated

        // All five mappings are still reachable through the external leaf.
        let device = f.ext4.block_device().as_ref();
        for k in 0..5u32 {
            let m = tree.find(device, k * 2).unwrap().unwrap();
            assert_eq!(m.start(), 100 + k as u64 * 10);
        }

        // The allocated leaf block is marked in the block bitmap (e2fsck-clean).
        let leaf_bid = ExtentIdx::from(&RawExtentIdx::from_bytes(
            &tree.root().as_bytes()[ENTRY_SIZE..2 * ENTRY_SIZE],
        ))
        .leaf();
        let group = f.ext4.block_group(0);
        let metadata = group.metadata();
        assert!(
            metadata
                .block_bitmap
                .is_allocated((leaf_bid - group.first_block()) as u16)
        );
    }
}
