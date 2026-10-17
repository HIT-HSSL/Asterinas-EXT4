// SPDX-License-Identifier: MPL-2.0

//! Per-mount I/O statistics for the EXT4 research baseline.

#[cfg(any(feature = "ext4-research-stats", ktest))]
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;

#[cfg(any(feature = "ext4-research-stats", ktest))]
use aster_block::BLOCK_SIZE;

#[cfg(all(not(ktest), feature = "ext4-research-stats"))]
pub(super) fn sync_clock_now() -> Duration {
    aster_time::read_monotonic_time()
}

#[cfg(any(ktest, not(feature = "ext4-research-stats")))]
#[inline(always)]
pub(super) fn sync_clock_now() -> Duration {
    Duration::ZERO
}

/// The EXT4 path that submitted a block I/O request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Ext4IoPath {
    SyncRead,
    SyncWrite,
    AsyncRead,
    AsyncWrite,
    ExtentWriteback,
}

/// A timed stage on the inode synchronization path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Ext4SyncStage {
    Xattr,
    GlobalMetadata,
    InodeLockWait,
    DataPages,
    InodeDesc,
    DeviceFlush,
}

/// A point-in-time copy of the research I/O counters.
#[cfg(any(feature = "ext4-research-stats", ktest))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Ext4IoStatsSnapshot {
    pub(super) total_bios: usize,
    pub(super) read_bios: usize,
    pub(super) write_bios: usize,
    pub(super) four_kib_bios: usize,
    pub(super) total_bytes: usize,
    pub(super) max_bio_bytes: usize,
    pub(super) current_in_flight: usize,
    pub(super) peak_in_flight: usize,
    pub(super) sync_bios: usize,
    pub(super) async_bios: usize,
    pub(super) extent_writeback_bios: usize,
    pub(super) writeback_inode_visits: usize,
    pub(super) sync_all_calls: usize,
    pub(super) sync_data_calls: usize,
    pub(super) sync_all_ns: usize,
    pub(super) sync_data_ns: usize,
    pub(super) xattr_ns: usize,
    pub(super) global_metadata_ns: usize,
    pub(super) inode_lock_wait_ns: usize,
    pub(super) data_pages_ns: usize,
    pub(super) inode_desc_ns: usize,
    pub(super) device_flush_ns: usize,
    pub(super) scanned_groups: usize,
}

/// Lock-free counters that observe EXT4 I/O without changing its ordering.
#[derive(Debug, Default)]
pub(super) struct Ext4IoStats {
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    total_bios: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    read_bios: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    write_bios: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    four_kib_bios: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    total_bytes: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    max_bio_bytes: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    current_in_flight: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    peak_in_flight: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    sync_bios: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    async_bios: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    extent_writeback_bios: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    writeback_inode_visits: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    sync_all_calls: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    sync_data_calls: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    sync_all_ns: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    sync_data_ns: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    xattr_ns: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    global_metadata_ns: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    inode_lock_wait_ns: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    data_pages_ns: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    inode_desc_ns: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    device_flush_ns: AtomicUsize,
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    scanned_groups: AtomicUsize,
}

impl Ext4IoStats {
    #[inline(always)]
    pub(super) fn record_submission(&self, nbytes: usize, path: Ext4IoPath) {
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            self.total_bios.fetch_add(1, Ordering::Relaxed);
            self.total_bytes.fetch_add(nbytes, Ordering::Relaxed);
            self.max_bio_bytes.fetch_max(nbytes, Ordering::Relaxed);
            if nbytes == BLOCK_SIZE {
                self.four_kib_bios.fetch_add(1, Ordering::Relaxed);
            }

            match path {
                Ext4IoPath::SyncRead => {
                    self.read_bios.fetch_add(1, Ordering::Relaxed);
                    self.sync_bios.fetch_add(1, Ordering::Relaxed);
                }
                Ext4IoPath::SyncWrite => {
                    self.write_bios.fetch_add(1, Ordering::Relaxed);
                    self.sync_bios.fetch_add(1, Ordering::Relaxed);
                }
                Ext4IoPath::AsyncRead => {
                    self.read_bios.fetch_add(1, Ordering::Relaxed);
                    self.async_bios.fetch_add(1, Ordering::Relaxed);
                }
                Ext4IoPath::AsyncWrite => {
                    self.write_bios.fetch_add(1, Ordering::Relaxed);
                    self.async_bios.fetch_add(1, Ordering::Relaxed);
                }
                Ext4IoPath::ExtentWriteback => {
                    self.write_bios.fetch_add(1, Ordering::Relaxed);
                    self.async_bios.fetch_add(1, Ordering::Relaxed);
                    self.extent_writeback_bios.fetch_add(1, Ordering::Relaxed);
                }
            }

            let in_flight = self.current_in_flight.fetch_add(1, Ordering::Relaxed) + 1;
            self.peak_in_flight.fetch_max(in_flight, Ordering::Relaxed);
        }
        #[cfg(not(any(feature = "ext4-research-stats", ktest)))]
        let _ = (nbytes, path);
    }

    #[inline(always)]
    pub(super) fn record_completion(&self) {
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            let previous = self.current_in_flight.fetch_sub(1, Ordering::Relaxed);
            debug_assert!(previous > 0, "EXT4 I/O completion without submission");
        }
    }

    #[inline(always)]
    pub(super) fn record_writeback_inode(&self) {
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            self.writeback_inode_visits.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[inline(always)]
    pub(super) fn record_sync_call(&self, full: bool, elapsed_ns: usize) {
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            if full {
                self.sync_all_calls.fetch_add(1, Ordering::Relaxed);
                self.sync_all_ns.fetch_add(elapsed_ns, Ordering::Relaxed);
            } else {
                self.sync_data_calls.fetch_add(1, Ordering::Relaxed);
                self.sync_data_ns.fetch_add(elapsed_ns, Ordering::Relaxed);
            }
        }
        #[cfg(not(any(feature = "ext4-research-stats", ktest)))]
        let _ = (full, elapsed_ns);
    }

    #[inline(always)]
    pub(super) fn record_sync_stage(&self, stage: Ext4SyncStage, elapsed_ns: usize) {
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            let counter = match stage {
                Ext4SyncStage::Xattr => &self.xattr_ns,
                Ext4SyncStage::GlobalMetadata => &self.global_metadata_ns,
                Ext4SyncStage::InodeLockWait => &self.inode_lock_wait_ns,
                Ext4SyncStage::DataPages => &self.data_pages_ns,
                Ext4SyncStage::InodeDesc => &self.inode_desc_ns,
                Ext4SyncStage::DeviceFlush => &self.device_flush_ns,
            };
            counter.fetch_add(elapsed_ns, Ordering::Relaxed);
        }
        #[cfg(not(any(feature = "ext4-research-stats", ktest)))]
        let _ = (stage, elapsed_ns);
    }

    #[inline(always)]
    pub(super) fn record_scanned_groups(&self, groups: usize) {
        #[cfg(any(feature = "ext4-research-stats", ktest))]
        {
            self.scanned_groups.fetch_add(groups, Ordering::Relaxed);
        }
        #[cfg(not(any(feature = "ext4-research-stats", ktest)))]
        let _ = groups;
    }

    #[cfg(ktest)]
    pub(super) fn snapshot(&self) -> Ext4IoStatsSnapshot {
        Ext4IoStatsSnapshot {
            total_bios: self.total_bios.load(Ordering::Relaxed),
            read_bios: self.read_bios.load(Ordering::Relaxed),
            write_bios: self.write_bios.load(Ordering::Relaxed),
            four_kib_bios: self.four_kib_bios.load(Ordering::Relaxed),
            total_bytes: self.total_bytes.load(Ordering::Relaxed),
            max_bio_bytes: self.max_bio_bytes.load(Ordering::Relaxed),
            current_in_flight: self.current_in_flight.load(Ordering::Relaxed),
            peak_in_flight: self.peak_in_flight.load(Ordering::Relaxed),
            sync_bios: self.sync_bios.load(Ordering::Relaxed),
            async_bios: self.async_bios.load(Ordering::Relaxed),
            extent_writeback_bios: self.extent_writeback_bios.load(Ordering::Relaxed),
            writeback_inode_visits: self.writeback_inode_visits.load(Ordering::Relaxed),
            sync_all_calls: self.sync_all_calls.load(Ordering::Relaxed),
            sync_data_calls: self.sync_data_calls.load(Ordering::Relaxed),
            sync_all_ns: self.sync_all_ns.load(Ordering::Relaxed),
            sync_data_ns: self.sync_data_ns.load(Ordering::Relaxed),
            xattr_ns: self.xattr_ns.load(Ordering::Relaxed),
            global_metadata_ns: self.global_metadata_ns.load(Ordering::Relaxed),
            inode_lock_wait_ns: self.inode_lock_wait_ns.load(Ordering::Relaxed),
            data_pages_ns: self.data_pages_ns.load(Ordering::Relaxed),
            inode_desc_ns: self.inode_desc_ns.load(Ordering::Relaxed),
            device_flush_ns: self.device_flush_ns.load(Ordering::Relaxed),
            scanned_groups: self.scanned_groups.load(Ordering::Relaxed),
        }
    }

    /// Returns the completed interval and starts fresh cumulative counters.
    ///
    /// `current_in_flight` is a gauge shared with completion callbacks, so it is
    /// preserved. The next interval's peak starts at that current value.
    #[cfg(any(feature = "ext4-research-stats", ktest))]
    pub(super) fn snapshot_and_reset(&self) -> Ext4IoStatsSnapshot {
        let current_in_flight = self.current_in_flight.load(Ordering::Relaxed);
        Ext4IoStatsSnapshot {
            total_bios: self.total_bios.swap(0, Ordering::Relaxed),
            read_bios: self.read_bios.swap(0, Ordering::Relaxed),
            write_bios: self.write_bios.swap(0, Ordering::Relaxed),
            four_kib_bios: self.four_kib_bios.swap(0, Ordering::Relaxed),
            total_bytes: self.total_bytes.swap(0, Ordering::Relaxed),
            max_bio_bytes: self.max_bio_bytes.swap(0, Ordering::Relaxed),
            current_in_flight,
            peak_in_flight: self
                .peak_in_flight
                .swap(current_in_flight, Ordering::Relaxed),
            sync_bios: self.sync_bios.swap(0, Ordering::Relaxed),
            async_bios: self.async_bios.swap(0, Ordering::Relaxed),
            extent_writeback_bios: self.extent_writeback_bios.swap(0, Ordering::Relaxed),
            writeback_inode_visits: self.writeback_inode_visits.swap(0, Ordering::Relaxed),
            sync_all_calls: self.sync_all_calls.swap(0, Ordering::Relaxed),
            sync_data_calls: self.sync_data_calls.swap(0, Ordering::Relaxed),
            sync_all_ns: self.sync_all_ns.swap(0, Ordering::Relaxed),
            sync_data_ns: self.sync_data_ns.swap(0, Ordering::Relaxed),
            xattr_ns: self.xattr_ns.swap(0, Ordering::Relaxed),
            global_metadata_ns: self.global_metadata_ns.swap(0, Ordering::Relaxed),
            inode_lock_wait_ns: self.inode_lock_wait_ns.swap(0, Ordering::Relaxed),
            data_pages_ns: self.data_pages_ns.swap(0, Ordering::Relaxed),
            inode_desc_ns: self.inode_desc_ns.swap(0, Ordering::Relaxed),
            device_flush_ns: self.device_flush_ns.swap(0, Ordering::Relaxed),
            scanned_groups: self.scanned_groups.swap(0, Ordering::Relaxed),
        }
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn io_stats_classify_size_path_and_concurrency() {
        let stats = Ext4IoStats::default();

        stats.record_submission(4096, Ext4IoPath::SyncRead);
        stats.record_completion();

        stats.record_submission(16 * 4096, Ext4IoPath::ExtentWriteback);
        stats.record_submission(4096, Ext4IoPath::AsyncWrite);
        stats.record_writeback_inode();
        stats.record_writeback_inode();

        let active = stats.snapshot();
        assert_eq!(active.total_bios, 3);
        assert_eq!(active.read_bios, 1);
        assert_eq!(active.write_bios, 2);
        assert_eq!(active.four_kib_bios, 2);
        assert_eq!(active.total_bytes, 18 * 4096);
        assert_eq!(active.max_bio_bytes, 16 * 4096);
        assert_eq!(active.current_in_flight, 2);
        assert_eq!(active.peak_in_flight, 2);
        assert_eq!(active.sync_bios, 1);
        assert_eq!(active.async_bios, 2);
        assert_eq!(active.extent_writeback_bios, 1);
        assert_eq!(active.writeback_inode_visits, 2);

        stats.record_completion();
        stats.record_completion();
        assert_eq!(stats.snapshot().current_in_flight, 0);
    }

    #[ktest]
    fn snapshot_and_reset_starts_a_new_completed_io_interval() {
        let stats = Ext4IoStats::default();
        stats.record_submission(4096, Ext4IoPath::ExtentWriteback);
        stats.record_writeback_inode();
        stats.record_completion();

        let completed = stats.snapshot_and_reset();
        assert_eq!(completed.total_bios, 1);
        assert_eq!(completed.peak_in_flight, 1);
        assert_eq!(completed.writeback_inode_visits, 1);
        assert_eq!(stats.snapshot(), Ext4IoStatsSnapshot::default());
    }

    #[ktest]
    fn sync_stats_accumulate_stages_calls_and_scanned_groups() {
        let stats = Ext4IoStats::default();

        stats.record_sync_call(true, 10_000);
        stats.record_sync_call(false, 8_000);
        stats.record_sync_stage(Ext4SyncStage::GlobalMetadata, 1_000);
        stats.record_sync_stage(Ext4SyncStage::Xattr, 500);
        stats.record_sync_stage(Ext4SyncStage::InodeLockWait, 2_000);
        stats.record_sync_stage(Ext4SyncStage::DataPages, 3_000);
        stats.record_sync_stage(Ext4SyncStage::InodeDesc, 4_000);
        stats.record_sync_stage(Ext4SyncStage::DeviceFlush, 5_000);
        stats.record_scanned_groups(2048);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.sync_all_calls, 1);
        assert_eq!(snapshot.sync_data_calls, 1);
        assert_eq!(snapshot.sync_all_ns, 10_000);
        assert_eq!(snapshot.sync_data_ns, 8_000);
        assert_eq!(snapshot.global_metadata_ns, 1_000);
        assert_eq!(snapshot.xattr_ns, 500);
        assert_eq!(snapshot.inode_lock_wait_ns, 2_000);
        assert_eq!(snapshot.data_pages_ns, 3_000);
        assert_eq!(snapshot.inode_desc_ns, 4_000);
        assert_eq!(snapshot.device_flush_ns, 5_000);
        assert_eq!(snapshot.scanned_groups, 2048);
    }
}
