// SPDX-License-Identifier: MPL-2.0

//! Ext4 linear directory read operations: `lookup` and `readdir`.
//!
//! A directory inode is page-cache-backed like a regular file; these methods
//! read its data blocks through the page cache and parse linear directory
//! entries. Htree-indexed directories are not supported.

#![short_vis_path::add(ext4)]

mod dir_entry;
mod linear;

use ostd::sync::RwMutexWriteGuard;

use self::dir_entry::DirEntryFileType;
use super::{
    super::{
        fs::Ext4,
        prelude::*,
        rename_anchor::{RenameAnchor, UNKNOWN_RENAME_SLOT},
        utils,
    },
    FileFlags, FilePerm, Inode, InodeInner, MAX_LINK_COUNT,
};

/// A candidate slot found by [`InodeInner::find_dir_slot`] or freshly created
/// by [`InodeInner::grow_dir_block`], where a new entry can be written.
#[derive(Clone, Copy, Debug)]
struct DirSlotInfo {
    /// Byte offset of the slot within the directory
    /// (`block_idx * BLOCK_SIZE + offset_in_block`).
    dir_offset: usize,
    /// Current `rec_len` of the candidate slot.
    slot_rec_len: usize,
    /// Minimal occupied length of the existing entry head (0 if the slot is a
    /// free entry or a freshly grown empty block).
    used_rec_len: usize,
}

/// A located, live directory entry returned by [`InodeInner::find_entry_info`],
/// describing where it sits so it can be deleted.
#[derive(Clone, Copy, Debug)]
struct DirEntryInfo {
    /// Inode number of the located entry. Read by the unlink/rmdir path to
    /// fetch the child inode; the delete primitive itself only needs the offset
    /// and record length.
    ino: Ext4Ino,
    /// Byte offset of the entry within the directory.
    dir_offset: usize,
    /// `rec_len` of the entry.
    entry_rec_len: usize,
}

struct ReclaimInodeCollector(Vec<Ext4Ino>);

/// Converges an interrupted NV-backed rename before the filesystem is exposed.
pub(in crate::fs::fs_impls::ext4) fn recover_rename(
    fs: &Arc<Ext4>,
    anchor: RenameAnchor,
) -> Result<()> {
    if anchor.dst_slot == UNKNOWN_RENAME_SLOT {
        return_errno_with_message!(Errno::EIO, "rename anchor has no destination slot");
    }
    let src_slot = usize::try_from(anchor.src_slot).map_err(|_| Error::new(Errno::EOVERFLOW))?;
    let dst_slot = usize::try_from(anchor.dst_slot).map_err(|_| Error::new(Errno::EOVERFLOW))?;
    let src = fs.read_inode(anchor.src_dir)?;
    let dst = fs.read_inode(anchor.dst_dir)?;
    let moved = fs.read_inode(anchor.ino)?;
    let replaced = (anchor.replaced_ino != 0)
        .then(|| fs.read_inode(anchor.replaced_ino))
        .transpose()?;
    let mut guards = MultiInodeInnerGuards::lock(&[
        src.as_ref(),
        dst.as_ref(),
        moved.as_ref(),
        replaced.as_deref().unwrap_or(moved.as_ref()),
    ]);

    let source_is_old = guards
        .inner(anchor.src_dir)
        .entry_info_at(src_slot)?
        .is_some_and(|entry| entry.ino == anchor.ino);
    let destination = guards.inner(anchor.dst_dir).entry_info_at(dst_slot)?;
    let destination_is_new = destination.is_some_and(|entry| entry.ino == anchor.ino);

    if source_is_old {
        // The old name is still authoritative. Undo a possibly published
        // destination and restore exact pre-rename link counts.
        if destination_is_new {
            let destination = destination.expect("checked above");
            if let Some(replaced) = &replaced {
                guards.inner_mut(anchor.dst_dir).set_entry_target(
                    &destination,
                    replaced.ino(),
                    DirEntryFileType::from(replaced.inode_type()),
                )?;
            } else {
                guards
                    .inner_mut(anchor.dst_dir)
                    .delete_entry(&destination)?;
            }
        }
        guards
            .inner_mut(anchor.src_dir)
            .set_link_count(anchor.src_links_before);
        guards
            .inner_mut(anchor.dst_dir)
            .set_link_count(anchor.dst_links_before);
        guards
            .inner_mut(anchor.ino)
            .set_link_count(anchor.ino_links_before);
        if let Some(replaced) = &replaced {
            guards
                .inner_mut(replaced.ino())
                .set_link_count(anchor.replaced_links_before);
        }
        if moved.inode_type() == InodeType::Dir && anchor.src_dir != anchor.dst_dir {
            let dotdot = guards.inner(anchor.ino).find_entry_info("..")?;
            guards.inner_mut(anchor.ino).set_entry_target(
                &dotdot,
                anchor.src_dir,
                DirEntryFileType::Dir,
            )?;
        }
    } else {
        // Source deletion is the commit point. The destination must already be
        // durable; finish all dependent metadata idempotently.
        if !destination_is_new {
            return_errno_with_message!(
                Errno::EIO,
                "interrupted rename has neither old nor new namespace entry"
            );
        }
        guards
            .inner_mut(anchor.src_dir)
            .set_link_count(anchor.src_links_after);
        guards
            .inner_mut(anchor.dst_dir)
            .set_link_count(anchor.dst_links_after);
        guards
            .inner_mut(anchor.ino)
            .set_link_count(anchor.ino_links_after);
        if let Some(replaced) = &replaced {
            guards
                .inner_mut(replaced.ino())
                .set_link_count(anchor.replaced_links_after);
        }
        if moved.inode_type() == InodeType::Dir && anchor.src_dir != anchor.dst_dir {
            let dotdot = guards.inner(anchor.ino).find_entry_info("..")?;
            guards.inner_mut(anchor.ino).set_entry_target(
                &dotdot,
                anchor.dst_dir,
                DirEntryFileType::Dir,
            )?;
        }
    }

    guards.inner_mut(anchor.src_dir).sync_data_pages()?;
    if anchor.src_dir != anchor.dst_dir {
        guards.inner_mut(anchor.dst_dir).sync_data_pages()?;
    }
    if moved.inode_type() == InodeType::Dir
        && anchor.ino != anchor.src_dir
        && anchor.ino != anchor.dst_dir
    {
        guards.inner_mut(anchor.ino).sync_data_pages()?;
    }
    fs.sync_metadata()?;
    let mut inos = vec![anchor.src_dir, anchor.dst_dir, anchor.ino];
    if anchor.replaced_ino != 0 {
        inos.push(anchor.replaced_ino);
    }
    inos.sort_unstable();
    inos.dedup();
    for ino in inos {
        guards.inner_mut(ino).write_back_inode_desc(fs, ino)?;
    }
    fs.clear_rename_anchor()
}

impl DirentVisitor for ReclaimInodeCollector {
    fn visit(&mut self, name: &str, ino: u64, _type: InodeType, _offset: usize) -> Result<()> {
        if name != "." && name != ".." && ino != 0 {
            self.0
                .push(u32::try_from(ino).map_err(|_| Error::new(Errno::EOVERFLOW))?);
        }
        Ok(())
    }
}

impl Inode {
    pub(in ext4) fn reclaim_directory_children(&self) -> Result<Vec<Ext4Ino>> {
        if self.type_ != InodeType::Dir {
            return Ok(Vec::new());
        }
        let mut collector = ReclaimInodeCollector(Vec::new());
        self.inner.read().readdir_at(0, &mut collector)?;
        Ok(collector.0)
    }

    /// Looks up a child entry by name and reads its inode.
    pub(in ext4) fn lookup(&self, name: &str) -> Result<Arc<Inode>> {
        let ino = self.inner.read().find_entry_ino(name)?;
        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem dropped"))?;
        fs.read_inode(ino)
    }

    /// Iterates directory entries from `offset`, feeding them to `visitor`,
    /// and updates the directory's atime (like ext2).
    pub(in ext4) fn readdir_at(
        &self,
        offset: usize,
        visitor: &mut dyn DirentVisitor,
    ) -> Result<usize> {
        let result = {
            let inner = self.inner.read();
            inner.readdir_at(offset, visitor)
        };
        self.inner.write().desc.set_atime(utils::now());
        result
    }

    /// Removes an empty sub-directory.
    ///
    pub(in ext4) fn rmdir(&self, name: &str, child: &Inode) -> Result<()> {
        let fs = self.fs()?;

        // The `DirDentry.children` lock in the VFS layer keeps the parent
        // directory entry stable during this operation, so we only need to lock
        // all related inodes in order, without rechecking the lookup result.
        let mut guards = MultiInodeInnerGuards::lock(&[self, child]);

        let entry_info = guards.inner(self.ino()).find_entry_info(name)?;
        if entry_info.ino != child.ino() {
            return_errno_with_message!(Errno::EIO, "directory entry changed during rmdir");
        }

        let child_inner = guards.inner_mut(child.ino());
        if child_inner.inode_type() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }
        if !child_inner.empty_dir(child.ino()) {
            return_errno!(Errno::ENOTEMPTY);
        }

        child_inner.set_ctime(utils::now());
        // The child loses its own `.` self-link and the parent's directory entry.
        child_inner.dec_link_count(2);
        if child_inner.link_count() == 0 {
            child_inner.write_back_inode_desc(&fs, child.ino())?;
            let _ = fs.remove_inode(child.ino());
        }

        let parent_inner = guards.inner_mut(self.ino());
        parent_inner.delete_entry(&entry_info)?;
        // The parent loses the `..` reference the removed child held back to it.
        parent_inner.dec_link_count(1);
        parent_inner.set_mtime_ctime(utils::now());

        Ok(())
    }

    /// Creates a child inode and directory entry under this directory.
    pub(in ext4) fn create(
        &self,
        name: &str,
        type_: InodeType,
        perm: FilePerm,
    ) -> Result<Arc<Inode>> {
        if !matches!(
            type_,
            InodeType::File
                | InodeType::Dir
                | InodeType::SymLink
                | InodeType::CharDevice
                | InodeType::BlockDevice
                | InodeType::NamedPipe
                | InodeType::Socket
        ) {
            return_errno!(Errno::EINVAL);
        }

        let is_dir = type_ == InodeType::Dir;
        let dir_entry_file_type = DirEntryFileType::from(type_);

        // Find a slot before creating the child inode to avoid wasting an inode
        // allocation if the directory cannot accept a new entry. The VFS dentry
        // layer has already validated that `name` is absent.
        let fs = self.fs()?;
        let mut parent_inner = self.inner.write();
        let slot = match parent_inner.find_dir_slot(name.len())? {
            Some(slot) => slot,
            None => parent_inner.grow_dir_block()?,
        };

        // The new inode is not yet visible in the inode cache until
        // `insert_inode` below. This is safe because the VFS dentry layer holds
        // an `upread` guard on the children set, preventing concurrent `create` /
        // `lookup_via_fs` on this directory, so a concurrent `lookup_via_fs`
        // won't build a second `Arc<Inode>` from the on-disk desc and insert it.
        let child = fs.create_inode(self.ino, type_, perm)?;
        let child_ino = child.ino();

        // Taking `child.inner.write()` while holding `parent_inner.write()` does
        // not need ino-ordering: the child is brand-new and unpublished (absent
        // from the inode cache, invisible to other threads), so no other thread
        // can hold or contend its `inner` lock.
        let result = if is_dir {
            child
                .inner
                .write()
                .make_empty(child_ino, self.ino)
                .and_then(|_| parent_inner.add_entry(&slot, name, child_ino, dir_entry_file_type))
        } else {
            parent_inner.add_entry(&slot, name, child_ino, dir_entry_file_type)
        };

        if let Err(err) = result {
            // Clear the link count so `Drop` reclaims the half-built inode.
            let mut child_inner = child.inner.write();
            child_inner.set_link_count(0);
            return Err(err);
        }

        // Link the child dir's `..` back to this parent.
        if is_dir {
            parent_inner.inc_link_count(1);
        }
        parent_inner.set_mtime_ctime(utils::now());
        fs.insert_inode(child.clone());
        Ok(child)
    }

    /// Adds a hard link in this directory to an existing inode.
    ///
    /// The VFS layer rejects hard links to directories (with `EPERM`) before
    /// reaching here, so — like ext2 — this does not re-check the type. It
    /// rejects only an overflowing link count (`EOVERFLOW`), mirroring ext2
    /// `Inode::link`. The two inodes (`self` and `old`) are locked through
    /// [`MultiInodeInnerGuards`] in ino order.
    pub(in ext4) fn link(&self, old: &Inode, name: &str) -> Result<()> {
        let fs = self.fs()?;
        let dir_entry_file_type = DirEntryFileType::from(old.inode_type());
        let mut guards = MultiInodeInnerGuards::lock(&[self, old]);

        if guards.inner(old.ino()).link_count() >= MAX_LINK_COUNT {
            return_errno!(Errno::EOVERFLOW);
        }

        let dir_inner = guards.inner_mut(self.ino());
        let slot = match dir_inner.find_dir_slot(name.len())? {
            Some(slot) => slot,
            None => dir_inner.grow_dir_block()?,
        };
        dir_inner.add_entry(&slot, name, old.ino(), dir_entry_file_type)?;
        dir_inner.set_mtime_ctime(utils::now());

        let old_inner = guards.inner_mut(old.ino());
        old_inner.set_ctime(utils::now());
        old_inner.inc_link_count(1);
        old_inner.write_back_inode_desc(&fs, old.ino())?;
        Ok(())
    }

    /// Removes a non-directory entry from this directory.
    ///
    /// On the link count reaching zero, the inode is dropped from the cache and
    /// reclaimed by the last surviving `Arc`.
    pub(in ext4) fn unlink(&self, name: &str, child: &Inode) -> Result<()> {
        let fs = self.fs()?;

        // The `DirDentry.children` lock in the VFS layer keeps the parent
        // directory entry stable during this operation, so we only need to lock
        // all related inodes in order, without rechecking the lookup result.
        let mut guards = MultiInodeInnerGuards::lock(&[self, child]);

        let entry_info = guards.inner(self.ino()).find_entry_info(name)?;
        if entry_info.ino != child.ino() {
            return_errno_with_message!(Errno::EIO, "directory entry changed during unlink");
        }

        let child_inner = guards.inner_mut(child.ino());
        if child_inner.inode_type() == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        {
            let parent_inner = guards.inner_mut(self.ino());
            parent_inner.delete_entry(&entry_info)?;
            parent_inner.set_mtime_ctime(utils::now());
            if fs.uses_nv_pool() {
                parent_inner.sync_data_pages()?;
            }
        }

        // Update timestamps before dropping the target link count.
        let child_inner = guards.inner_mut(child.ino());
        child_inner.set_ctime(utils::now());
        child_inner.dec_link_count(1);
        if child_inner.link_count() == 0 {
            child_inner.write_back_inode_desc(&fs, child.ino())?;
            // Drop the cache's `Arc`; if an fd still holds one the inode stays
            // alive until that last `Arc` drops, then `Drop` reclaims it. We do
            // NOT force reclaim here — refcount + `Drop` handle unlink-of-open.
            let _ = fs.remove_inode(child.ino());
        }
        Ok(())
    }

    /// Renames or moves an already-resolved entry into `new_dir_inode`.
    pub(in ext4) fn rename(
        &self,
        old_name: &str,
        old_inode: &Inode,
        new_dir_inode: &Inode,
        new_name: &str,
        replaced_inode: Option<&Inode>,
    ) -> Result<()> {
        // The `DirDentry.children` lock in the VFS layer keeps both directory
        // entries and resolved inode identities stable. Lock every participant
        // in inode-number order; duplicates are removed by the guard helper.
        let mut guards = MultiInodeInnerGuards::lock(&[
            self as &Inode,
            new_dir_inode,
            old_inode,
            replaced_inode.unwrap_or(old_inode),
        ]);

        self.validate_rename_invariants(&guards, old_inode, replaced_inode)?;

        let fs = self.fs()?;
        if fs.uses_nv_pool() {
            let old_is_dir = old_inode.inode_type() == InodeType::Dir;
            let same_dir = self.ino() == new_dir_inode.ino();
            let src_links_before = guards.inner(self.ino()).link_count();
            let dst_links_before = guards.inner(new_dir_inode.ino()).link_count();
            let ino_links = guards.inner(old_inode.ino()).link_count();
            let replaced_links_before = replaced_inode
                .map(|inode| guards.inner(inode.ino()).link_count())
                .unwrap_or(0);
            let subtract = |value: u16, delta: u16| {
                value
                    .checked_sub(delta)
                    .ok_or_else(|| Error::with_message(Errno::EIO, "rename link count underflow"))
            };
            let (src_links_after, dst_links_after) = if same_dir {
                let after = if old_is_dir && replaced_inode.is_some() {
                    subtract(src_links_before, 1)?
                } else {
                    src_links_before
                };
                (after, after)
            } else {
                let src_after = if old_is_dir {
                    subtract(src_links_before, 1)?
                } else {
                    src_links_before
                };
                let dst_after = if old_is_dir && replaced_inode.is_none() {
                    dst_links_before.checked_add(1).ok_or_else(|| {
                        Error::with_message(Errno::EMLINK, "rename link count overflow")
                    })?
                } else {
                    dst_links_before
                };
                (src_after, dst_after)
            };
            let replaced_links_after = if replaced_inode.is_some() {
                subtract(replaced_links_before, if old_is_dir { 2 } else { 1 })?
            } else {
                0
            };
            let src_slot = guards
                .inner(self.ino())
                .find_entry_info(old_name)?
                .dir_offset;
            let dst_slot = if replaced_inode.is_some() {
                Some(
                    guards
                        .inner(new_dir_inode.ino())
                        .find_entry_info(new_name)?
                        .dir_offset,
                )
            } else {
                let target_inner = guards.inner(new_dir_inode.ino());
                Some(
                    target_inner
                        .find_dir_slot(new_name.len())?
                        .map(|slot| slot.dir_offset + slot.used_rec_len)
                        .unwrap_or_else(|| target_inner.file_size()),
                )
            };
            fs.commit_rename_anchor(&RenameAnchor::new(
                self.ino(),
                src_slot,
                new_dir_inode.ino(),
                dst_slot,
                old_inode.ino(),
                replaced_inode.map(Inode::ino),
                (src_links_before, src_links_after),
                (dst_links_before, dst_links_after),
                (ino_links, ino_links),
                (replaced_links_before, replaced_links_after),
            )?)?;
        }

        self.apply_dir_mutations(
            &mut guards,
            new_dir_inode,
            old_name,
            old_inode,
            replaced_inode,
            new_name,
        )?;

        if fs.uses_nv_pool() {
            // Publish the new name first; a crash can temporarily leave both
            // names, but never loses the inode from the namespace.
            guards.inner_mut(new_dir_inode.ino()).sync_data_pages()?;
            if self.ino() != new_dir_inode.ino() {
                guards.inner_mut(self.ino()).sync_data_pages()?;
            }
            if old_inode.inode_type() == InodeType::Dir
                && old_inode.ino() != self.ino()
                && old_inode.ino() != new_dir_inode.ino()
            {
                guards.inner_mut(old_inode.ino()).sync_data_pages()?;
            }

            // Directory-page writeback may have allocated blocks. Their
            // allocation bits must be durable before publishing inode roots.
            fs.sync_metadata()?;

            let mut inos = vec![self.ino(), new_dir_inode.ino(), old_inode.ino()];
            if let Some(replaced) = replaced_inode {
                inos.push(replaced.ino());
            }
            inos.sort_unstable();
            inos.dedup();
            for ino in inos {
                guards.inner_mut(ino).write_back_inode_desc(&fs, ino)?;
            }
            fs.clear_rename_anchor()?;
        }

        Ok(())
    }

    /// Validates the rename invariants under the locks taken by [`Self::rename`].
    ///
    /// A moved directory must still point to its source parent. Replacement
    /// also requires compatible inode types and an empty target directory.
    fn validate_rename_invariants(
        &self,
        guards: &MultiInodeInnerGuards,
        old_inode: &Inode,
        replaced_inode: Option<&Inode>,
    ) -> Result<()> {
        // A mismatch indicates corruption; do not publish a wrong `..` update.
        if old_inode.inode_type() == InodeType::Dir {
            let old_inner = guards.inner(old_inode.ino());
            let parent_ino = old_inner.find_entry_info("..")?.ino;
            if parent_ino != self.ino() {
                return_errno_with_message!(Errno::EIO, "dotdot entry inconsistent with source dir");
            }
        }

        if let Some(replaced) = replaced_inode {
            let replaced_is_dir = replaced.inode_type() == InodeType::Dir;
            let old_is_dir = old_inode.inode_type() == InodeType::Dir;
            if old_is_dir && !replaced_is_dir {
                return_errno!(Errno::ENOTDIR);
            }
            if !old_is_dir && replaced_is_dir {
                return_errno!(Errno::EISDIR);
            }
            if replaced_is_dir {
                let replaced_inner = guards.inner(replaced.ino());
                if !replaced_inner.empty_dir(replaced.ino()) {
                    return_errno!(Errno::ENOTEMPTY);
                }
            }
        }

        Ok(())
    }

    /// Applies the directory-entry and link-count mutations for [`Self::rename`].
    ///
    /// Cross-directory moves update `..`. Replacements drop the target's link
    /// count and reclaim it when the count reaches zero.
    fn apply_dir_mutations(
        &self,
        guards: &mut MultiInodeInnerGuards,
        target: &Inode,
        old_name: &str,
        old_inode: &Inode,
        replaced_inode: Option<&Inode>,
        new_name: &str,
    ) -> Result<()> {
        let old_is_dir = old_inode.inode_type() == InodeType::Dir;
        let has_replaced = replaced_inode.is_some();
        let old_ino = old_inode.ino();
        let is_same_dir = self.ino() == target.ino();
        let moved_file_type = DirEntryFileType::from(old_inode.inode_type());
        let fs = self.fs()?;

        // Step 4.1: apply the directory-entry mutations.
        if is_same_dir {
            let dir_inner = guards.inner_mut(self.ino());
            if has_replaced {
                dir_inner.overwrite_entry(new_name, old_ino, moved_file_type)?;
            } else {
                dir_inner.add_new_entry(new_name, old_ino, moved_file_type)?;
            }
            if fs.uses_nv_pool() {
                dir_inner.sync_data_pages()?;
                fs.sync_metadata()?;
                dir_inner.write_back_inode_desc(&fs, self.ino())?;
            }
            // Re-read the source entry: `add_new_entry` may have split it
            // (shrinking its `rec_len`), making any earlier `DirEntryInfo` stale.
            let old_info = dir_inner.find_entry_info(old_name)?;
            dir_inner.delete_entry(&old_info)?;
            // Replacing a directory with a directory in the same parent: the
            // parent loses the replaced directory's `..` back-reference.
            if old_is_dir && has_replaced {
                dir_inner.dec_link_count(1);
            }
            dir_inner.set_mtime_ctime(utils::now());
        } else {
            let target_inner = guards.inner_mut(target.ino());
            if has_replaced {
                target_inner.overwrite_entry(new_name, old_ino, moved_file_type)?;
            } else {
                target_inner.add_new_entry(new_name, old_ino, moved_file_type)?;
            }
            // Moving a directory into a fresh name in `target`: `target` gains the
            // moved directory's new `..` back-reference. When replacing, the slot
            // already counted that reference, so `target`'s count is unchanged.
            if old_is_dir && !has_replaced {
                target_inner.inc_link_count(1);
            }
            target_inner.set_mtime_ctime(utils::now());
            if fs.uses_nv_pool() {
                target_inner.sync_data_pages()?;
                fs.sync_metadata()?;
                target_inner.write_back_inode_desc(&fs, target.ino())?;
            }

            let source_inner = guards.inner_mut(self.ino());
            // Re-read the source entry (same staleness reason as above, though
            // here only the target was mutated; kept symmetric with ext2).
            let old_info = source_inner.find_entry_info(old_name)?;
            source_inner.delete_entry(&old_info)?;
            // Moving a directory out of `self`: `self` loses the moved
            // directory's `..` back-reference.
            if old_is_dir {
                source_inner.dec_link_count(1);
            }
            source_inner.set_mtime_ctime(utils::now());
        }

        // Step 4.2: drop the replaced inode's link count and reclaim it if it
        // reaches 0.
        if let Some(replaced) = replaced_inode {
            let replaced_inner = guards.inner_mut(replaced.ino());
            replaced_inner.set_ctime(utils::now());
            // A replaced directory loses both its own `.` self-link and the
            // entry; a replaced non-directory loses only the entry.
            if old_is_dir {
                replaced_inner.dec_link_count(1);
            }
            replaced_inner.dec_link_count(1);

            if replaced_inner.link_count() == 0 {
                replaced_inner.write_back_inode_desc(&fs, replaced.ino())?;
                // Drop the cache's `Arc`. If an fd still holds one the inode stays
                // alive until that last `Arc` (here in the caller's locals, dropped
                // after `guards`) drops, then `Drop` reclaims it.
                let _ = fs.remove_inode(replaced.ino());
            }
        }

        // Step 4.3: repoint a moved directory's `..` at its new parent.
        let old_inner = guards.inner_mut(old_ino);
        if old_is_dir && !is_same_dir {
            let dotdot_entry_info = old_inner.find_entry_info("..")?;
            old_inner.set_entry_target(&dotdot_entry_info, target.ino(), DirEntryFileType::Dir)?;
            // The htree index would describe the now-stale block layout; the
            // A moved directory must be re-indexed before indexed lookup.
            old_inner.remove_flags(FileFlags::INDEX);
            old_inner.set_mtime_ctime(utils::now());
        } else {
            old_inner.set_ctime(utils::now());
        }

        Ok(())
    }
}

const MAX_MULTI_INODE_LOCKS: usize = 4;

/// A guard holding up to [`MAX_MULTI_INODE_LOCKS`] inodes' `inner.write()` locks
/// at once, acquired in ascending ino order with duplicates removed.
///
/// Operations that touch several inodes take their `inner` write locks through
/// this type so every path uses the same order and avoids deadlock.
struct MultiInodeInnerGuards<'a> {
    entries: [Option<(Ext4Ino, RwMutexWriteGuard<'a, InodeInner>)>; MAX_MULTI_INODE_LOCKS],
    len: usize,
}

impl<'a> MultiInodeInnerGuards<'a> {
    /// Acquires `inner.write()` locks on deduplicated inodes in ascending ino
    /// order.
    fn lock(inodes: &[&'a Inode]) -> Self {
        let mut sorted_inodes: [Option<&'a Inode>; MAX_MULTI_INODE_LOCKS] =
            [None; MAX_MULTI_INODE_LOCKS];
        let count = inodes.len().min(MAX_MULTI_INODE_LOCKS);
        for (i, inode) in inodes.iter().take(count).enumerate() {
            sorted_inodes[i] = Some(*inode);
        }
        sorted_inodes[..count].sort_by_key(|opt| opt.unwrap().ino);

        let mut entries: [Option<(Ext4Ino, RwMutexWriteGuard<'a, InodeInner>)>;
            MAX_MULTI_INODE_LOCKS] = [None, None, None, None];
        let mut len = 0;
        let mut prev_ino = None;
        for slot in sorted_inodes.iter().take(count).flatten() {
            if prev_ino == Some(slot.ino) {
                continue;
            }
            prev_ino = Some(slot.ino);
            entries[len] = Some((slot.ino, slot.inner.write()));
            len += 1;
        }
        Self { entries, len }
    }

    /// Returns a shared reference to the held `inner` of inode `ino`.
    fn inner(&self, ino: Ext4Ino) -> &InodeInner {
        let (_, guard) = self.entries[..self.len]
            .iter()
            .flatten()
            .find(|(entry_ino, _)| *entry_ino == ino)
            .expect("requested inode inner lock must be held");
        guard
    }

    /// Returns a mutable reference to the held `inner` of inode `ino`.
    fn inner_mut(&mut self, ino: Ext4Ino) -> &mut InodeInner {
        let (_, guard) = self.entries[..self.len]
            .iter_mut()
            .flatten()
            .find(|(entry_ino, _)| *entry_ino == ino)
            .expect("requested inode inner lock must be held");
        guard
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::{
        super::super::test_utils::{Ext4Fixture, Ext4FixtureBuilder, make_empty_file_inode},
        *,
    };
    use crate::{
        fs::{
            fs_impls::ext4::{fs::ROOT_INO, rename_anchor::RENAME_ANCHOR_HOME},
            nv_pool::{
                config::{NvPoolConfig, NvSimMode},
                dram::DramNvBackend,
                frame::NvPool,
            },
        },
        time::clocks,
    };

    const DIR_INO: u32 = 12;

    fn perm() -> FilePerm {
        FilePerm::from_bits_truncate(0o644)
    }

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

    /// A fixture whose block *and* inode bitmaps are marked, with a `.`/`..`
    /// directory at `DIR_INO` ready to receive `create`d children. `DIR_INO`'s
    /// `..` points to the root inode (2).
    fn fixture_for_create() -> Ext4Fixture {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_inode_bitmap_metadata_marked()
            // Reserve the pre-placed directory's inode so a created child never
            // gets handed `DIR_INO` back (which would alias the directory).
            .with_reserved_inode(DIR_INO)
            .build()
            .unwrap();
        let mut raw = make_empty_file_inode();
        raw.mode = 0o040755; // S_IFDIR | 0755
        raw.link_count = 2;
        f.write_raw_inode(DIR_INO, &raw);
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        dir.inner.write().make_empty(DIR_INO, 2).unwrap();
        f
    }

    #[ktest]
    fn directory_page_is_committed_as_nv_metadata_before_ssd() {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_inode_bitmap_metadata_marked()
            .with_reserved_inode(DIR_INO)
            .with_nv_pool(nv_pool())
            .build()
            .unwrap();
        let mut raw = make_empty_file_inode();
        raw.mode = 0o040755;
        raw.link_count = 2;
        f.write_raw_inode(DIR_INO, &raw);
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        dir.inner.write().make_empty(DIR_INO, 2).unwrap();
        let pblock = dir
            .inner
            .read()
            .block_manager()
            .unwrap()
            .map_blocks(0)
            .unwrap()
            .pblock()
            .unwrap();

        dir.sync_all().unwrap();

        let mut ssd = [0; BLOCK_SIZE];
        f.disk
            .segment()
            .read_bytes(pblock as usize * BLOCK_SIZE, &mut ssd)
            .unwrap();
        assert_eq!(ssd, [0; BLOCK_SIZE]);
        let mut nv = [0; BLOCK_SIZE];
        f.ext4
            .nv_state()
            .unwrap()
            .read_meta_block(pblock)
            .unwrap()
            .read(&mut nv)
            .unwrap();
        assert_eq!(u32::from_ne_bytes(nv[0..4].try_into().unwrap()), DIR_INO);
        assert_eq!(dir.lookup(".").unwrap().ino(), DIR_INO);
    }

    #[ktest]
    fn rename_commits_namespace_and_clears_anchor() {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_inode_bitmap_metadata_marked()
            .with_reserved_inode(DIR_INO)
            .with_nv_pool(nv_pool())
            .build()
            .unwrap();
        let mut raw = make_empty_file_inode();
        raw.mode = 0o040755;
        raw.link_count = 2;
        f.write_raw_inode(DIR_INO, &raw);
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        dir.inner.write().make_empty(DIR_INO, 2).unwrap();
        dir.sync_all().unwrap();
        let child = dir.create("old", InodeType::File, perm()).unwrap();
        dir.sync_all().unwrap();

        dir.rename("old", &child, &dir, "new", None).unwrap();

        assert!(dir.lookup("old").is_err());
        assert_eq!(dir.lookup("new").unwrap().ino(), child.ino());
        assert!(
            f.ext4
                .nv_state()
                .unwrap()
                .resident_frame(RENAME_ANCHOR_HOME)
                .is_none()
        );
    }

    fn prepare_interrupted_rename() -> (Ext4Fixture, Arc<Inode>, Arc<Inode>, RenameAnchor) {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_inode_bitmap_metadata_marked()
            .with_reserved_inode(DIR_INO)
            .with_nv_pool(nv_pool())
            .build()
            .unwrap();
        let mut raw = make_empty_file_inode();
        raw.mode = 0o040755;
        raw.link_count = 2;
        f.write_raw_inode(DIR_INO, &raw);
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        dir.inner.write().make_empty(DIR_INO, 2).unwrap();
        let child = dir.create("old", InodeType::File, perm()).unwrap();
        let inner = dir.inner.read();
        let src_slot = inner.find_entry_info("old").unwrap().dir_offset;
        let slot = inner.find_dir_slot("new".len()).unwrap().unwrap();
        let dst_slot = slot.dir_offset + slot.used_rec_len;
        drop(inner);
        let anchor = RenameAnchor::new(
            DIR_INO,
            src_slot,
            DIR_INO,
            Some(dst_slot),
            child.ino(),
            None,
            (dir.link_count(), dir.link_count()),
            (dir.link_count(), dir.link_count()),
            (child.link_count(), child.link_count()),
            (0, 0),
        )
        .unwrap();
        anchor.persist(f.ext4.nv_state().unwrap()).unwrap();
        (f, dir, child, anchor)
    }

    #[ktest]
    fn rename_recovery_rolls_back_when_source_entry_remains() {
        let (f, dir, child, anchor) = prepare_interrupted_rename();
        dir.inner
            .write()
            .add_new_entry(
                "new",
                child.ino(),
                DirEntryFileType::from(child.inode_type()),
            )
            .unwrap();

        recover_rename(&f.ext4, anchor).unwrap();

        assert_eq!(dir.lookup("old").unwrap().ino(), child.ino());
        assert!(dir.lookup("new").is_err());
        assert!(
            f.ext4
                .nv_state()
                .unwrap()
                .resident_frame(RENAME_ANCHOR_HOME)
                .is_none()
        );
    }

    #[ktest]
    fn rename_recovery_finishes_when_source_entry_is_gone() {
        let (f, dir, child, anchor) = prepare_interrupted_rename();
        {
            let mut inner = dir.inner.write();
            inner
                .add_new_entry(
                    "new",
                    child.ino(),
                    DirEntryFileType::from(child.inode_type()),
                )
                .unwrap();
            let old = inner.find_entry_info("old").unwrap();
            inner.delete_entry(&old).unwrap();
        }

        recover_rename(&f.ext4, anchor).unwrap();

        assert!(dir.lookup("old").is_err());
        assert_eq!(dir.lookup("new").unwrap().ino(), child.ino());
        assert!(
            f.ext4
                .nv_state()
                .unwrap()
                .resident_frame(RENAME_ANCHOR_HOME)
                .is_none()
        );
    }

    #[ktest]
    fn reclaim_frees_unreachable_inode_but_preserves_namespace_tree() {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_inode_bitmap_metadata_marked()
            .with_reserved_inode(ROOT_INO)
            .with_nv_pool(nv_pool())
            .build()
            .unwrap();
        let mut raw = make_empty_file_inode();
        raw.mode = 0o040755;
        raw.link_count = 2;
        f.write_raw_inode(ROOT_INO, &raw);
        let root = f.ext4.read_inode(ROOT_INO).unwrap();
        root.inner.write().make_empty(ROOT_INO, ROOT_INO).unwrap();
        root.sync_all().unwrap();
        let reachable = root.create("kept", InodeType::File, perm()).unwrap();
        root.sync_all().unwrap();

        let orphan = f
            .ext4
            .create_inode(ROOT_INO, InodeType::File, perm())
            .unwrap();
        let orphan_ino = orphan.ino();
        drop(orphan);

        let stats = f.ext4.reclaim_unreachable().unwrap();

        assert_eq!(stats.reclaimed_inodes, 1);
        assert_eq!(root.lookup("kept").unwrap().ino(), reachable.ino());
        assert!(!f.ext4.block_group(0).is_inode_allocated(orphan_ino));
    }

    /// Unlink-of-open: while an `Arc` to the child is held, `unlink` removes the
    /// name but does NOT free the inode (it stays allocated). Dropping the held
    /// `Arc` then reclaims it via `Drop`, restoring the free counts. This is the
    /// key refcount/reclaim test.
    #[ktest]
    fn unlink_of_open_defers_reclaim() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        let free_inodes_before = f.ext4.block_group(0).free_inodes_count();
        let free_blocks_before = f.ext4.block_group(0).free_blocks_count();

        // Create a file with data, and keep an `Arc` open across the unlink.
        let open = dir.create("open.txt", InodeType::File, perm()).unwrap();
        let child_ino = open.ino();
        let mut reader = VmReader::from(&[3u8; 32][..]).to_fallible();
        open.write_at(0, &mut reader).unwrap();

        dir.unlink("open.txt", open.as_ref()).unwrap();

        // Name gone, but the inode is still allocated: an fd holds it open.
        assert!(dir.lookup("open.txt").is_err());
        assert!(
            f.ext4.is_inode_allocated(child_ino),
            "inode freed while still open"
        );
        assert_eq!(open.link_count(), 0);

        // Releasing the last `Arc` triggers `Drop` reclaim.
        drop(open);
        assert!(!f.ext4.is_inode_allocated(child_ino));
        assert_eq!(
            f.ext4.block_group(0).free_inodes_count(),
            free_inodes_before
        );
        assert_eq!(
            f.ext4.block_group(0).free_blocks_count(),
            free_blocks_before
        );
    }
}
