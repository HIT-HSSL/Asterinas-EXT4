// SPDX-License-Identifier: MPL-2.0

//! Page-cache infrastructure shared by filesystems, memory mappings, and
//! writeback.
//!
//! This module is the filesystem-facing entry point to the page-cache
//! subsystem. A filesystem typically keeps one [`PageCache`] per file (or per
//! inode data stream) and uses it for buffered I/O, truncate/extend, and cache
//! invalidation.
//!
//! # Overview
//!
//! The subsystem is split into four layers with different responsibilities:
//!
//! - [`PageCache`] is the per-file façade used by filesystems. It exposes
//!   buffered I/O, resize, flush, and invalidation operations in filesystem
//!   terms.
//! - [`Vmo`] is the lower-level memory object underneath a page cache. It owns
//!   the page array, commits pages on demand, and is the abstraction shared
//!   with page-fault and mapping code.
//! - [`CachePage`] stores cached page contents, while
//!   [`cache_page::PageState`] records whether those contents are uninitialized,
//!   clean, or dirty.
//! - [`PageCacheBackend`] is the storage-facing contract used
//!   to make a page readable or durable. [`BlockAsPageCacheBackend`] is a
//!   helper interface for backends that can satisfy that contract with
//!   block-device BIOs.
//!
//! For a file with a backend, the steady-state flow is
//! `filesystem metadata -> PageCache -> Vmo -> PageCacheBackend -> block
//! device or remote server`. Anonymous page caches use the same `PageCache` / `Vmo` layers
//! without a backend.
//!
//! # Responsibility Boundary
//!
//! `PageCache` manages page-aligned cache capacity and cached contents. The
//! filesystem still owns file size, extent or block mapping metadata, and the
//! higher-level locking that keeps those decisions stable while the page cache
//! is accessed.
//!
//! Stay at [`PageCache`] when the caller is operating in filesystem terms. Drop
//! to [`Vmo`] only when code needs the lower-level memory-object interface
//! directly, such as `mmap` setup or page-fault handling.
//!
//! # Synchronization Model
//!
//! The page-cache subsystem serializes per-page state transitions, including
//! the backend page states in [`cache_page::PageState`] and the auxiliary
//! writeback tracking bit in [`CachePageMeta`].
//!
//! [`PageCache`] is stored inside a filesystem-level lock or inode lock. It
//! prevents conflicting buffered I/O and resizing. However, it cannot prevent
//! concurrent page faults. They need to be synchronized with page table locks,
//! the reverse-mapping lock, and per-page locks. This module handles the
//! complexity in cooperation with [`super::vmar`].
//!
//! A complete overview of how cached file I/O ("read"/"write" below),
//! `O_DIRECT` file I/O ("read (direct)"/"write (direct)"), page cache
//! management operations ("resize"/"flush"/"evict"), and page faults
//! ("fault (load)"/"fault (store)") interact with each other when they are
//! invoked concurrently is shown below.
//!
//! |                | read            | write           | read (direct) | write (direct)  | resize      | flush       | evict       | fault (load) | fault (store) |
//! |----------------|-----------------|-----------------|---------------|-----------------|-------------|-------------|-------------|--------------|---------------|
//! | read           | N/A             |                 |               |                 |             |             |             |              |               |
//! | write          | inode*          | inode*          |               |                 |             |             |             |              |               |
//! | read (direct)  | N/A             | (flush)         | N/A           |                 |             |             |             |              |               |
//! | write (direct) | (flush + evict) | (flush + evict) | inode*        | inode*          |             |             |             |              |               |
//! | resize         | inode           | inode           | inode         | inode           | inode       |             |             |              |               |
//! | flush          | N/A             | page            | (flush)       | (flush + evict) | rmap + page | rmap + page |             |              |               |
//! | evict          | N/A             | page            | (flush)       | (flush + evict) | rmap + page | rmap + page | rmap + page |              |               |
//! | fault (load)   | N/A             | N/A             | N/A           | (flush + evict) | PT + xarray | N/A         | PT + page   | N/A          |               |
//! | fault (store)  | N/A             | N/A             | (flush)       | (flush + evict) | PT + xarray | PT + page   | PT + page   | N/A          | N/A           |
//!
//! The annotations in the table are explained below.
//!
//!  - If two operations have no semantic conflicts, even if they are called
//!    concurrently (e.g., if both operations try to read something), the
//!    corresponding entry is noted as `N/A`.
//!
//!  - Userspace programs should not mix cached and direct file I/O. Otherwise,
//!    data corruption caused by the program will be the user's fault. The
//!    corresponding entry is noted as `(OP)` where `OP` is the operation
//!    inserted by the filesystem before starting direct file I/O, which
//!    ensures the correctness when the two operations are called
//!    **sequentially**.
//!
//!  - For two operations protected by the inode lock, the entry is noted as
//!    `inode` or `inode*`. The latter, `inode*`, means that lock usage is not
//!    yet enforced at the page cache layer or the Rust type level (i.e.,
//!    [`PageCache`] operations do not yet take `&mut self`).
//!
//!  - In this table, `flush` and `evict` are assumed to be able to be called
//!    without holding the inode lock (e.g., under memory pressure). But as we
//!    have not implemented any related mechanisms, they can currently only be
//!    called from the filesystem while holding at least the inode read lock.
//!
//!  - For interactions between page faults and page cache management, the
//!    correct order of multiple locks is important to ensure that the cached
//!    page is always in a consistent state, even when operations are called
//!    concurrently. The corresponding entry is noted as `LockA + LockB`. We
//!    use `rmap` to denote the reverse-mapping lock, `PT` to denote the page
//!    table lock, `page` to denote the page lock, and `xarray` to denote the
//!    lock of VMO pages. For details, see the comments in the implementation.

use core::{
    fmt::Debug,
    ops::{Deref, Range},
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use align_ext::AlignExt;
use aster_block::bio::{BioCompleteFn, BioDirection, BioSegment, BioStatus};
use io_util::{
    IoError,
    batch::{IoBatch, IoCompletion},
};
use ostd::mm::{Segment, VmIo, VmIoFill, io::util::HasVmReaderWriter};

use crate::prelude::*;

mod cache_page;
#[cfg(ktest)]
mod tests;
mod vmo;

pub(crate) use cache_page::{CachePage, CachePageExt, CachePageMeta, LockedCachePage};
pub(crate) use vmo::{Vmo, VmoCommitError, VmoFlags, VmoMapMode, VmoOptions};

/// The page cache for a file-like object.
///
/// This is the abstraction a filesystem usually stores in an inode: it handles
/// buffered reads and writes, writeback, invalidation, and page-cache resizing,
/// while delegating per-page population and writeback to the underlying [`Vmo`].
///
/// A `PageCache` owns cached page contents and a page-aligned capacity. It does
/// not own:
///
/// - the filesystem's file size (EOF);
/// - extent or block-mapping metadata; or
/// - the higher-level synchronization that keeps metadata, buffered I/O,
///   invalidation, and VM mappings coherent.
///
/// Filesystems backed by persistent or remote storage create it with
/// [`PageCache::new_with_backend`] and a [`PageCacheBackend`]. Purely
/// in-memory filesystems can use [`PageCache::new_anon`] to get the same
/// buffered-I/O interface without a backend.
///
/// A `PageCache` is more than just an `Arc<Vmo>` wrapper. It acts as a unique
/// handle to the underlying [`Vmo`], enabling additional operations and
/// enforcing the correct concurrency contract (see the module-level
/// documentation of [`crate::vm::page_cache`]). Typically, a user (such as a
/// file system) should place a `PageCache` inside a [`Mutex`] or [`RwMutex`].
/// Operations requiring exclusive access to the page cache handle require
/// `&mut self`, so a proper inode or file system lock must be obtained in
/// advance.
///
/// Reach for [`PageCache::as_vmo`] only when a lower-level consumer such as
/// memory mapping or page-fault code must operate on the underlying [`Vmo`]
/// directly.
#[derive(Debug)]
pub(crate) struct PageCache(Arc<Vmo>);

impl PageCache {
    /// Creates a page cache with a backend and the specified initial capacity
    /// in bytes.
    pub(crate) fn new_with_backend(
        size: usize,
        backend: Weak<dyn PageCacheBackend>,
    ) -> Result<Self> {
        Ok(Self(VmoOptions::new_page_cache(size, backend).alloc()?))
    }

    /// Creates an anonymous page cache with the specified initial capacity in
    /// bytes.
    pub(crate) fn new_anon(size: usize) -> Result<Self> {
        Ok(Self(VmoOptions::new_anon(size).alloc()?))
    }

    /// Returns the wrapped [`Vmo`].
    pub(crate) fn as_vmo(&self) -> &Arc<Vmo> {
        &self.0
    }

    /// Returns the current page-cache capacity in bytes.
    ///
    /// This size is page-aligned and may exceed the file size. The filesystem
    /// remains responsible for tracking EOF separately.
    pub(crate) fn size(&self) -> usize {
        self.0.size()
    }

    /// Resizes the page-cache capacity to cover a new file size.
    ///
    /// `new_file_size` is the post-resize file size requested by the filesystem.
    /// The underlying cache capacity is rounded up to page boundaries. If the
    /// new size is smaller than the current size, pages that fall entirely
    /// within the truncated range will be decommitted (freed). For the page
    /// that is only partially truncated (i.e., the page containing the new
    /// boundary), the truncated portion will be filled with zeros instead.
    ///
    /// The `old_file_size` must be the file length before this resize. It is
    /// used to determine the boundary of previously valid data so that only the
    /// discarded range within a partially truncated tail page is zero-filled.
    ///
    /// Extending the page cache does not eagerly allocate pages and therefore
    /// cannot return an error. Shrinking may fail because it can zero-fill or
    /// decommit existing pages.
    ///
    /// # Size Synchronization
    ///
    /// `PageCache::resize` only updates the page-aligned [`Vmo`] capacity. The
    /// filesystem must keep that capacity synchronized with its own file size
    /// under the same inode- or file-level lock that excludes conflicting
    /// buffered I/O and resizing.
    ///
    /// The required ordering is:
    ///
    /// - When extending a file, update the file size before increasing
    ///   [`Vmo::size`] so subsequent reads can observe the new range.
    /// - When truncating a file, shrink [`Vmo::size`] before decreasing the
    ///   file size so reads beyond the new EOF cannot observe stale cached
    ///   pages.
    ///
    /// Accordingly, `old_file_size` must be the pre-resize file size captured
    /// inside that resize critical section.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use ostd::mm::PAGE_SIZE;
    ///
    /// use crate::vm::page_cache::PageCache;
    ///
    /// let mut page_cache = PageCache::new_anon(0).unwrap();
    ///
    /// // Extend: publish the new file size first, then grow the page cache
    /// // with the previous file size.
    /// let mut file_size = PAGE_SIZE + 123;
    /// page_cache.resize(file_size, 0).unwrap();
    /// assert_eq!(page_cache.size(), 2 * PAGE_SIZE);
    ///
    /// // Truncate: shrink the page cache first, while passing the old file
    /// // size captured in the same critical section.
    /// let old_file_size = file_size;
    /// file_size = 512;
    /// page_cache.resize(file_size, old_file_size).unwrap();
    /// assert_eq!(page_cache.size(), PAGE_SIZE);
    /// ```
    pub(crate) fn resize(&mut self, new_file_size: usize, old_file_size: usize) -> Result<()> {
        let vmo = &self.0;
        assert!(vmo.flags.contains(VmoFlags::RESIZABLE));

        if new_file_size < old_file_size && !new_file_size.is_multiple_of(PAGE_SIZE) {
            let fill_zero_end = old_file_size.min(new_file_size.align_up(PAGE_SIZE));
            self.fill_zeros(new_file_size..fill_zero_end)?;
        } else if new_file_size > old_file_size && !old_file_size.is_multiple_of(PAGE_SIZE) {
            let fill_zero_end = new_file_size.min(old_file_size.align_up(PAGE_SIZE));
            self.fill_zeros(old_file_size..fill_zero_end)?;
        }

        let rmap = if new_file_size < old_file_size {
            Some(vmo.rmap.lock())
        } else {
            None
        };

        let new_cache_size = new_file_size.align_up(PAGE_SIZE);
        let locked_pages = vmo.pages.lock();
        let old_cache_size = vmo.size();
        if new_cache_size == old_cache_size {
            return Ok(());
        }

        vmo.size.store(new_cache_size, Ordering::Release);

        if let Some(locked_rmap) = rmap {
            let decommit_range = new_cache_size..old_cache_size;
            if let Some(backed_vmo) = vmo.as_backed_vmo() {
                backed_vmo.decommit_pages(locked_rmap, locked_pages, &decommit_range)?;
            } else {
                vmo.decommit_anon_pages(locked_rmap, locked_pages, decommit_range)?;
            }
        }

        Ok(())
    }

    /// Flushes dirty pages in the specified range to the backend storage.
    ///
    /// This walks the current cache contents, submits writeback for pages that
    /// are dirty when this pass reaches them, and waits for the submitted I/O
    /// to complete.
    ///
    /// Filesystems that need `fsync`-like guarantees must still exclude
    /// concurrent writers or repeat the operation until their own ordering
    /// requirements are met.
    ///
    /// If the given range exceeds the current size of the page cache, only the pages within
    /// the valid range will be flushed.
    pub(crate) fn flush_range(&self, range: Range<usize>) -> Result<()> {
        let Some(vmo) = self.0.as_backed_vmo() else {
            return Ok(());
        };

        vmo.flush_dirty_pages(&range)
    }

    /// Flushes at most `max_pages` candidates from a range.
    ///
    /// Filesystem background writers use this bounded primitive for fair,
    /// memory-limited scheduling. Explicit synchronization should use
    /// [`Self::flush_range`] to drain the complete range.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(crate) fn flush_range_batch(&self, range: Range<usize>, max_pages: usize) -> Result<bool> {
        if max_pages == 0 {
            return_errno_with_message!(Errno::EINVAL, "writeback batch must contain pages");
        }
        let Some(vmo) = self.0.as_backed_vmo() else {
            return Ok(false);
        };
        vmo.flush_dirty_pages_batch(&range, max_pages)
    }

    /// Flushes one bounded writeback batch using the requested completion mode.
    pub(crate) fn flush_range_batch_with_mode(
        &self,
        range: Range<usize>,
        max_pages: usize,
        mode: WritebackMode,
    ) -> Result<WritebackBatchResult> {
        if max_pages == 0 {
            return_errno_with_message!(Errno::EINVAL, "writeback batch must contain pages");
        }
        let Some(vmo) = self.0.as_backed_vmo() else {
            return Ok(WritebackBatchResult::empty());
        };
        vmo.flush_dirty_pages_batch_with_mode(&range, max_pages, mode)
    }

    /// Evicts clean pages within the specified range from the page cache.
    ///
    /// Only pages in the `UpToDate` state are removed. Dirty and uninitialized
    /// pages are left in place. This is useful for invalidating cached data that
    /// is no longer needed and can be re-read from the backend if necessary
    /// (e.g., before direct I/O operations).
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(crate) fn evict_range(&self, range: Range<usize>) -> Result<()> {
        let Some(vmo) = self.0.as_backed_vmo() else {
            return Ok(());
        };

        vmo.evict_up_to_date_pages(&range)
    }

    /// Flushes dirty pages and then evicts clean pages in the specified range.
    ///
    /// This is the standard preparation step before issuing direct I/O that must
    /// bypass the page cache. It uses the same locking requirements as
    /// [`PageCache::flush_range`] and [`PageCache::evict_range`].
    pub(crate) fn invalidate_range(&self, range: Range<usize>) -> Result<()> {
        let Some(vmo) = self.0.as_backed_vmo() else {
            return Ok(());
        };

        vmo.flush_dirty_pages(&range)?;
        vmo.evict_up_to_date_pages(&range)
    }

    /// Fills the specified range of the page cache with zeros.
    ///
    /// Callers must hold the filesystem-level lock that serializes operations in
    /// the target range.
    pub(crate) fn fill_zeros(&self, range: Range<usize>) -> Result<()> {
        self.0.fill_zeros(range)
    }

    /// Returns the contiguous byte ranges backed by dirty pages in `range`.
    ///
    /// Filesystems may use this snapshot to prepare storage mappings before
    /// asking the page cache to submit writeback.
    #[cfg(ktest)]
    pub(crate) fn dirty_ranges(&self, range: Range<usize>) -> Vec<Range<usize>> {
        let Some(vmo) = self.0.as_backed_vmo() else {
            return Vec::new();
        };
        vmo.dirty_ranges(&range)
    }
}

impl VmIo for PageCache {
    fn read(&self, offset: usize, writer: &mut VmWriter) -> ostd::Result<()> {
        self.0.read(offset, writer)?;
        Ok(())
    }

    // TODO: This should require `&mut` to ensure that the caller holds the
    // inode lock. However, we need to fix the exfat code first, since it does
    // not hold the write lock when handling `write()`.
    fn write(&self, offset: usize, reader: &mut VmReader) -> ostd::Result<()> {
        self.0.write(offset, reader)?;
        Ok(())
    }
}

/// A storage backend for a backed [`PageCache`].
///
/// This trait is the high-level contract used by the page-cache layer to load
/// page contents from storage and write dirty contents back. A successful read
/// makes the requested page usable by readers; a successful write makes the
/// page's current contents durable in the backend.
///
/// Direct implementations are useful for storage that does not naturally expose
/// block-device BIOs, such as network filesystems. Filesystems backed by block
/// devices usually implement [`BlockAsPageCacheBackend`] instead.
pub(crate) trait PageCacheBackend: Sync + Send {
    /// Returns an optional owner-specific observer for page writeback state.
    fn writeback_observer(&self) -> Option<Arc<dyn PageCacheWritebackObserver>> {
        None
    }

    /// Returns the dispatcher used by this backend's writeback implementation.
    ///
    /// Direct backends remain synchronous by default. An implementation may
    /// return a dispatcher only if its `write_pages_async` path preserves each
    /// `WritebackPage` and submits it through the PageCache writeback helpers.
    fn writeback_completion_dispatcher(&self) -> Option<Arc<dyn WritebackCompletionDispatcher>> {
        None
    }

    /// Reads a page from the backend asynchronously.
    ///
    /// The caller may try to pass an index that exceeds the size of the
    /// underlying backend (e.g., the file size). This can occur if file
    /// truncation and a page fault occur at the same time. If this happens,
    /// this method should fail with `EINVAL`.
    fn read_page_async(
        &self,
        idx: usize,
        locked_page: LockedCachePage,
        io_batch: &mut IoBatch,
    ) -> Result<()>;

    /// Writes a page to the backend asynchronously.
    ///
    /// If the caller tries to pass an index that exceeds the size of the
    /// underlying backend (e.g., the file size), this method should fail with
    /// `EINVAL`. Note that this cannot happen until we support concurrent file
    /// truncation and page writeback.
    //
    // TODO: Revise this behavior once file truncation and page writeback can
    // happen concurrently.
    fn write_page_async(
        &self,
        idx: usize,
        locked_page: LockedCachePage,
        io_batch: &mut IoBatch,
    ) -> Result<()>;

    /// Writes a sorted batch of independently owned page candidates.
    ///
    /// Backends may override this method to choose I/O boundaries using their
    /// own mapping information. The default preserves the per-page behavior.
    fn write_pages_async(&self, pages: Vec<WritebackPage>, io_batch: &mut IoBatch) -> Result<()> {
        for page in pages {
            let claimed = page.claim_handle();
            let (idx, locked_page, _) = page.into_parts();
            self.write_page_async(idx, locked_page, io_batch)?;
            claimed.store(true, Ordering::Release);
        }
        Ok(())
    }
}

/// One locked dirty page offered to a backend for writeback.
///
/// Each candidate remains independently splittable until the backend chooses
/// an I/O run. Dropping an unprepared candidate unlocks it without cleaning it.
pub(crate) struct WritebackPage {
    idx: usize,
    locked_page: LockedCachePage,
    observer: Option<Arc<dyn PageCacheWritebackObserver>>,
    claimed: Arc<AtomicBool>,
    vmo: Option<Arc<Vmo>>,
    batch_fence: Option<WritebackBatchFence>,
    dispatcher: Option<Arc<dyn WritebackCompletionDispatcher>>,
}

impl WritebackPage {
    fn new(
        idx: usize,
        locked_page: LockedCachePage,
        observer: Option<Arc<dyn PageCacheWritebackObserver>>,
    ) -> Self {
        Self {
            idx,
            locked_page,
            observer,
            claimed: Arc::new(AtomicBool::new(false)),
            vmo: None,
            batch_fence: None,
            dispatcher: None,
        }
    }

    pub(crate) fn index(&self) -> usize {
        self.idx
    }

    fn claim_handle(&self) -> Arc<AtomicBool> {
        self.claimed.clone()
    }

    pub(super) fn bind_vmo(&mut self, vmo: Arc<Vmo>) {
        self.vmo = Some(vmo);
    }

    pub(super) fn bind_batch_fence(&mut self, batch_fence: WritebackBatchFence) {
        self.batch_fence = Some(batch_fence);
    }

    pub(super) fn bind_dispatcher(&mut self, dispatcher: Arc<dyn WritebackCompletionDispatcher>) {
        self.dispatcher = Some(dispatcher);
    }

    fn into_parts(
        self,
    ) -> (
        usize,
        LockedCachePage,
        Option<Arc<dyn PageCacheWritebackObserver>>,
    ) {
        (self.idx, self.locked_page, self.observer)
    }

    fn into_writeback_parts(self) -> WritebackPageParts {
        (
            self.idx,
            self.locked_page,
            self.observer,
            self.claimed,
            self.vmo.expect("writeback page must be bound to its VMO"),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WritebackMode {
    Background,
    Sync,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct WritebackSequence(u64);

impl WritebackSequence {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) const fn value(self) -> u64 {
        self.0
    }
}

/// Records exactly the dispatcher sequences prepared by one flush operation.
#[derive(Clone)]
pub(super) struct WritebackBatchFence(Arc<SpinLock<Option<WritebackSequence>>>);

impl WritebackBatchFence {
    pub(super) fn new() -> Self {
        Self(Arc::new(SpinLock::new(None)))
    }

    fn record(&self, sequence: WritebackSequence) {
        let mut fence = self.0.lock();
        *fence = Some(fence.map_or(sequence, |current| current.max(sequence)));
    }

    pub(super) fn sequence(&self) -> Option<WritebackSequence> {
        *self.0.lock()
    }
}

pub(crate) struct PreparedWritebackCompletion {
    pub(crate) complete_fn: BioCompleteFn,
    pub(crate) sequence: WritebackSequence,
}

pub(crate) trait WritebackCompletionDispatcher: Debug + Send + Sync {
    /// On error, `prepare` must leave `completion` intact. On success, the
    /// dispatcher owns it and must finish it exactly once.
    fn prepare(
        &self,
        completion: &mut Option<WritebackCompletion>,
        nr_pages: usize,
    ) -> Result<PreparedWritebackCompletion>;

    fn finish_submission_failure(&self, sequence: WritebackSequence, error: Error);

    /// Returns a global completion-progress sequence for credit-pressure waits.
    fn completion_progress(&self) -> u64;

    /// Waits until at least one completion has made progress since `observed`.
    fn wait_for_completion_progress_since(&self, observed: u64);

    /// Returns the latest sequence submitted for this dispatcher's mapping.
    fn latest_sequence(&self) -> Option<WritebackSequence>;

    fn wait_through(&self, sequence: WritebackSequence) -> Result<()>;

    /// Consumes one persistent error at or before `sequence` after a synchronous
    /// caller has observed it.
    fn consume_error_through(&self, sequence: WritebackSequence) -> Option<Error>;
}

type WritebackPageParts = (
    usize,
    LockedCachePage,
    Option<Arc<dyn PageCacheWritebackObserver>>,
    Arc<AtomicBool>,
    Arc<Vmo>,
);

pub(crate) trait WritebackPostIo: Any + Send {
    fn finish(self: Box<Self>, status: BioStatus) -> core::result::Result<(), IoError>;

    fn as_any(&self) -> &dyn Any;

    fn try_merge(
        &mut self,
        next: Box<dyn WritebackPostIo>,
    ) -> core::result::Result<(), Box<dyn WritebackPostIo>> {
        Err(next)
    }
}

struct ClosureWritebackPostIo<F>(Option<F>);

impl<F> WritebackPostIo for ClosureWritebackPostIo<F>
where
    F: FnOnce(BioStatus) -> core::result::Result<(), IoError> + Send + 'static,
{
    fn finish(mut self: Box<Self>, status: BioStatus) -> core::result::Result<(), IoError> {
        self.0.take().unwrap()(status)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct WritebackPageCompletion {
    idx: usize,
    page: CachePage,
    observer: Option<Arc<dyn PageCacheWritebackObserver>>,
    vmo: Arc<Vmo>,
}

/// Owns the PageCache state associated with a submitted writeback run.
pub(crate) struct WritebackCompletion {
    pages: Vec<WritebackPageCompletion>,
    post_io: Option<Box<dyn WritebackPostIo>>,
}

impl WritebackCompletion {
    fn new_with_post_io(
        pages: Vec<WritebackPageCompletion>,
        post_io: Box<dyn WritebackPostIo>,
    ) -> Self {
        Self {
            pages,
            post_io: Some(post_io),
        }
    }

    pub(crate) fn try_merge(
        &mut self,
        mut next: WritebackCompletion,
    ) -> core::result::Result<(), WritebackCompletion> {
        let next_post_io = next.post_io.take().unwrap();
        match self.post_io.as_mut().unwrap().try_merge(next_post_io) {
            Ok(()) => {
                self.pages.append(&mut next.pages);
                Ok(())
            }
            Err(next_post_io) => {
                next.post_io = Some(next_post_io);
                Err(next)
            }
        }
    }

    pub(crate) fn finish(self, status: BioStatus) -> Result<()> {
        self.finish_io(status).map_err(Error::from)
    }

    fn finish_io(mut self, status: BioStatus) -> core::result::Result<(), IoError> {
        let result = if status == BioStatus::Complete {
            self.post_io.take().unwrap().finish(status)
        } else {
            Err(io_error_from_bio_status(status))
        };
        let succeeded = result.is_ok();
        for page in self.pages {
            let locked_page = page.page.lock();
            if succeeded {
                let was_redirtied = locked_page.state().is_dirty();
                locked_page.clear_writing_back();
                if was_redirtied {
                    page.vmo.set_page_mark_if_same(
                        page.idx,
                        &locked_page,
                        vmo::PageCacheMark::Dirty,
                    );
                } else if let Some(observer) = page.observer {
                    observer.page_cleaned(page.idx);
                }
            } else {
                locked_page.set_dirty();
                locked_page.clear_writing_back();
                page.vmo
                    .set_page_mark_if_same(page.idx, &locked_page, vmo::PageCacheMark::Dirty);
            }
            page.vmo
                .unset_page_mark_if_same(page.idx, &locked_page, vmo::PageCacheMark::Writeback);
        }
        result
    }
}

pub(crate) struct WritebackBatchResult {
    dispatcher: Option<Arc<dyn WritebackCompletionDispatcher>>,
    fence: Option<WritebackSequence>,
    had_work: bool,
    retry_after_completion: bool,
}

impl WritebackBatchResult {
    pub(crate) fn empty() -> Self {
        Self {
            dispatcher: None,
            fence: None,
            had_work: false,
            retry_after_completion: false,
        }
    }

    pub(super) fn blocked_on_writeback() -> Self {
        Self {
            dispatcher: None,
            fence: None,
            had_work: false,
            retry_after_completion: true,
        }
    }

    pub(super) fn completed() -> Self {
        Self {
            dispatcher: None,
            fence: None,
            had_work: true,
            retry_after_completion: false,
        }
    }

    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(crate) fn wait(self) -> Result<()> {
        if let (Some(dispatcher), Some(sequence)) = (self.dispatcher, self.fence) {
            dispatcher.wait_through(sequence)?;
        }
        Ok(())
    }

    pub(crate) fn wait_and_consume_error(self) -> Result<()> {
        if let (Some(dispatcher), Some(sequence)) = (self.dispatcher, self.fence) {
            if let Err(error) = dispatcher.wait_through(sequence) {
                return Err(dispatcher.consume_error_through(sequence).unwrap_or(error));
            }
        }
        Ok(())
    }

    pub(super) fn detached(
        dispatcher: Arc<dyn WritebackCompletionDispatcher>,
        fence: Option<WritebackSequence>,
    ) -> Self {
        Self {
            dispatcher: Some(dispatcher),
            fence,
            had_work: true,
            retry_after_completion: false,
        }
    }

    pub(crate) fn had_work(&self) -> bool {
        self.had_work
    }

    pub(crate) fn retry_after_completion(&self) -> bool {
        self.retry_after_completion
    }
}

/// Receives page-state events without owning writeback policy.
///
/// Callbacks may run while the page lock is held. Implementations must remain
/// non-blocking and must not acquire inode, page-cache, XArray, or reverse-map
/// locks; atomic accounting, short independent queue locks, and wakeups are
/// permitted.
pub(crate) trait PageCacheWritebackObserver: Debug + Send + Sync {
    fn page_dirtied(&self, page_idx: usize);
    fn page_cleaned(&self, page_idx: usize);

    fn writeback_completion_dispatcher(&self) -> Option<Arc<dyn WritebackCompletionDispatcher>> {
        None
    }
}

impl dyn PageCacheBackend {
    /// Reads a page from the backend synchronously.
    pub(crate) fn read_page(&self, idx: usize, page: LockedCachePage) -> Result<()> {
        let mut io_batch = IoBatch::with_capacity(1);
        self.read_page_async(idx, page, &mut io_batch)?;
        io_batch.wait_all()?;
        Ok(())
    }
}

/// A block-I/O object that can serve as a [`PageCacheBackend`].
///
/// This trait is a convenience layer for filesystems whose cached file data is
/// served by block I/O. Implementors submit the supplied [`BioSegment`] for the
/// page at the requested index.
///
/// The blanket [`PageCacheBackend`] implementation adapts this trait to the
/// generic page-cache backend contract.
///
/// The `complete_fn` passed to submit methods may run from interrupt context,
/// so implementations must not allocate, take blocking locks, or hold a lock
/// that a waiter on the page wait queue may already hold.
//
// TODO: This trait should provide interfaces for reading or writing multiple
// pages in a single BIO to improve efficiency for sequential I/O.
pub(crate) trait BlockAsPageCacheBackend: Sync + Send {
    fn writeback_observer(&self) -> Option<Arc<dyn PageCacheWritebackObserver>> {
        None
    }

    /// Submits read I/O for the page at `idx`.
    ///
    /// `bio_segment` identifies the page memory that must receive the data.
    /// Implementations must attach `complete_fn` to the underlying
    /// asynchronous I/O so the page-cache layer can mark successful
    /// reads up to date and release the page lock.
    ///
    /// If the page only contains zeros, implementations may call `complete_fn`
    /// with [`BioStatus::Zeros`], skip I/O, and leave `bio_segment` unfilled.
    ///
    /// Implementations should fail with `EINVAL` for an out-of-bounds `idx`.
    /// See also [`PageCacheBackend::read_page_async`].
    fn submit_read_bio(
        &self,
        idx: usize,
        bio_segment: BioSegment,
        complete_fn: BioCompleteFn,
        io_batch: &mut IoBatch,
    ) -> Result<()>;

    /// Submits write I/O for the page at `idx`.
    ///
    /// `bio_segment` contains the stable page snapshot that must be written.
    /// Implementations must attach `complete_fn` to the underlying
    /// asynchronous I/O so the page-cache layer can finish writeback
    /// bookkeeping and report failures.
    ///
    /// Implementations should fail with `EINVAL` for an out-of-bounds `idx`.
    /// See also [`PageCacheBackend::write_page_async`].
    fn submit_write_bio(
        &self,
        idx: usize,
        bio_segment: BioSegment,
        complete_fn: BioCompleteFn,
        io_batch: &mut IoBatch,
    ) -> Result<()>;

    /// Chooses and submits BIOs for a sorted batch of writeback candidates.
    ///
    /// Filesystems with extent information can override this method to build
    /// multi-page BIOs. Other block backends retain deterministic per-page I/O.
    fn submit_write_pages(&self, pages: Vec<WritebackPage>, io_batch: &mut IoBatch) -> Result<()> {
        for page in pages {
            submit_one_block_write_page(self, page, io_batch)?;
        }
        Ok(())
    }
}

/// Snapshots and marks a filesystem-selected run as writeback, then delegates
/// construction of the actual BIO to `submit`.
pub(crate) fn submit_writeback_run(
    pages: Vec<WritebackPage>,
    io_batch: &mut IoBatch,
    submit: impl FnOnce(Vec<BioSegment>, BioCompleteFn, &mut IoBatch) -> Result<()>,
) -> Result<()> {
    submit_writeback_run_with_post_io(pages, io_batch, submit, |_| Ok(()))
}

/// Like [`submit_writeback_run`], with a process-context step after device I/O
/// and before pages are declared clean.
pub(crate) fn submit_writeback_run_with_post_io(
    pages: Vec<WritebackPage>,
    io_batch: &mut IoBatch,
    submit: impl FnOnce(Vec<BioSegment>, BioCompleteFn, &mut IoBatch) -> Result<()>,
    post_io: impl FnOnce(BioStatus) -> core::result::Result<(), IoError> + Send + 'static,
) -> Result<()> {
    submit_writeback_run_with_post_io_job(
        pages,
        io_batch,
        submit,
        Box::new(ClosureWritebackPostIo(Some(post_io))),
    )
}

/// Like [`submit_writeback_run_with_post_io`], with a merge-aware post-I/O job.
pub(crate) fn submit_writeback_run_with_post_io_job(
    pages: Vec<WritebackPage>,
    io_batch: &mut IoBatch,
    submit: impl FnOnce(Vec<BioSegment>, BioCompleteFn, &mut IoBatch) -> Result<()>,
    post_io: Box<dyn WritebackPostIo>,
) -> Result<()> {
    debug_assert!(!pages.is_empty());

    let dispatcher = pages
        .first()
        .and_then(|page| page.dispatcher.as_ref().cloned());
    let batch_fence = pages
        .first()
        .and_then(|page| page.batch_fence.as_ref().cloned());
    let nr_pages = pages.len();
    let mut completion_pages = Vec::with_capacity(nr_pages);
    // The filesystem has already verified that this run maps to contiguous
    // device blocks, so one DMA snapshot avoids a descriptor per cache page.
    let bio_segment = BioSegment::alloc(pages.len(), BioDirection::ToDevice);
    {
        let mut segment_writer = bio_segment.writer().unwrap();
        for page in pages {
            let (idx, locked_page, observer, claimed, vmo) = page.into_writeback_parts();
            let locked_page = locked_page.wait_until_finish_writing_back();

            segment_writer.write(&mut locked_page.reader());

            locked_page.set_writing_back();
            locked_page.set_up_to_date();
            claimed.store(true, Ordering::Release);
            completion_pages.push(WritebackPageCompletion {
                idx,
                page: locked_page.unlock(),
                observer,
                vmo,
            });
        }
    }
    let mut completion = Some(WritebackCompletion::new_with_post_io(
        completion_pages,
        post_io,
    ));

    if let Some(dispatcher) = dispatcher {
        let prepared = match dispatcher.prepare(&mut completion, nr_pages) {
            Ok(prepared) => prepared,
            Err(err) => {
                completion.take().unwrap().finish(BioStatus::IoError).ok();
                return Err(err);
            }
        };
        if let Some(batch_fence) = batch_fence {
            batch_fence.record(prepared.sequence);
        }
        let res = submit(vec![bio_segment], prepared.complete_fn, io_batch);
        if let Err(error) = &res {
            dispatcher.finish_submission_failure(prepared.sequence, *error);
        }
        return res;
    }

    let status = Arc::new(AtomicU32::new(BioStatus::Init as u32));
    let completion_status = status.clone();
    let complete_fn: BioCompleteFn = Box::new(move |status| {
        completion_status.store(status as u32, Ordering::Release);
    });
    let res = submit(vec![bio_segment], complete_fn, io_batch);
    if res.is_ok() {
        io_batch.push(Arc::new(DeferredWritebackCompletion {
            status,
            completion: SpinLock::new(completion),
        }));
    } else {
        completion.unwrap().finish(BioStatus::IoError).ok();
    }
    res
}

fn submit_one_block_write_page<T: BlockAsPageCacheBackend + ?Sized>(
    backend: &T,
    page: WritebackPage,
    io_batch: &mut IoBatch,
) -> Result<()> {
    let idx = page.index();
    submit_writeback_run(
        vec![page],
        io_batch,
        |mut segments, complete_fn, io_batch| {
            backend.submit_write_bio(idx, segments.pop().unwrap(), complete_fn, io_batch)
        },
    )
}

struct DeferredWritebackCompletion {
    status: Arc<AtomicU32>,
    completion: SpinLock<Option<WritebackCompletion>>,
}

impl IoCompletion for DeferredWritebackCompletion {
    fn wait(&self) -> core::result::Result<(), IoError> {
        let status =
            BioStatus::try_from(self.status.load(Ordering::Acquire)).unwrap_or(BioStatus::IoError);
        let completion = self
            .completion
            .lock()
            .take()
            .expect("deferred writeback completion waited twice");
        completion.finish_io(status)
    }
}

fn io_error_from_bio_status(status: BioStatus) -> IoError {
    match status {
        BioStatus::NotSupported => IoError::Unsupported,
        BioStatus::NoSpace => IoError::OutOfSpace,
        _ => IoError::Failed,
    }
}

impl<T: BlockAsPageCacheBackend> PageCacheBackend for T {
    fn writeback_observer(&self) -> Option<Arc<dyn PageCacheWritebackObserver>> {
        BlockAsPageCacheBackend::writeback_observer(self)
    }

    fn writeback_completion_dispatcher(&self) -> Option<Arc<dyn WritebackCompletionDispatcher>> {
        BlockAsPageCacheBackend::writeback_observer(self)
            .and_then(|observer| observer.writeback_completion_dispatcher())
    }

    fn read_page_async(
        &self,
        idx: usize,
        locked_page: LockedCachePage,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        let bio_segment = BioSegment::new_from_segment(
            Segment::from(locked_page.deref().clone()).into(),
            BioDirection::FromDevice,
        );

        let complete_fn: BioCompleteFn = Box::new(move |status| {
            if status == BioStatus::Zeros {
                locked_page.fill_zeros(0, PAGE_SIZE).unwrap();
                locked_page.set_up_to_date();
            } else if status == BioStatus::Complete {
                locked_page.set_up_to_date();
            }
            // The page lock is released when `locked_page` (LockedCachePage) is dropped here.
        });

        self.submit_read_bio(idx, bio_segment, complete_fn, io_batch)
    }

    fn write_page_async(
        &self,
        idx: usize,
        locked_page: LockedCachePage,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        submit_one_block_write_page(
            self,
            WritebackPage::new(idx, locked_page, self.writeback_observer()),
            io_batch,
        )
    }

    fn write_pages_async(&self, pages: Vec<WritebackPage>, io_batch: &mut IoBatch) -> Result<()> {
        self.submit_write_pages(pages, io_batch)
    }
}
