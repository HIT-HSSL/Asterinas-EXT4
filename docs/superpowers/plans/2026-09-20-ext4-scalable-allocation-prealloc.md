# Scalable EXT4 Allocation and Preallocation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow EXT4 block allocations for different inodes and block groups to execute concurrently, then reduce allocator entries with adaptive per-inode and per-CPU preallocation.

**Architecture:** Replace the allocator's filesystem-wide exclusive mutation and superblock locks with a shared allocation epoch plus exact atomic runtime accounting. Keep physical ownership under per-group metadata locks. Add crash-safe in-memory preallocation whose unused ranges are reserved only in the buddy cache and become persistent bitmap allocations when consumed.

**Tech Stack:** Rust 2024, Asterinas kernel, OSTD synchronization and CPU-local APIs, EXT4 extent/buddy allocator, kernel-mode `ktest`, JBD2 and nojournal metadata paths.

**Spec:** `docs/superpowers/specs/2026-09-20-ext4-scalable-allocation-prealloc.md`

## Global Constraints

- Kernel code remains safe Rust; no `unsafe` code under `kernel/`.
- Preserve existing on-disk EXT4 structures and mount compatibility.
- Normal allocation may take a shared filesystem epoch guard but no filesystem-wide exclusive lock.
- Never hold two block-group metadata locks simultaneously.
- Unconsumed PA ranges remain clear in the on-disk bitmap.
- No new external dependencies.
- Preserve JBD2, nojournal, NVPool, reclaim, truncate, unlink, and unmount correctness.
- Use test-first development and observe every new behavioral test fail before production changes.

---

### Task 1: Shared Allocation Epoch

**Files:**
- Modify: `kernel/core/src/fs/nv_pool/reclaim.rs`
- Test: `kernel/core/src/fs/nv_pool/reclaim.rs`

**Interfaces:**
- Produces: `ReclaimTracker::lock_mutation_shared() -> RwMutexReadGuard<'_, ()>`
- Produces: `ReclaimEpoch::lock_mutation_exclusive() -> RwMutexWriteGuard<'_, ()>` or an equivalent private exclusive acquisition used by `reclaim_if_untouched`.
- Preserves: touched-object ABA protection.

- [ ] **Step 1: Write failing concurrency tests**

Add ktests that start two execution contexts, hold two shared guards concurrently, and prove an exclusive reclaim transition cannot enter until both shared guards are released. Keep the existing touched-object tests unchanged.

```rust
#[ktest]
fn allocator_mutations_share_the_epoch() {
    let tracker = Arc::new(ReclaimTracker::new());
    let first = tracker.lock_mutation_shared();
    let second = tracker.lock_mutation_shared();
    drop(second);
    drop(first);
}

#[ktest]
fn reclaim_transition_excludes_allocator_mutations() {
    let tracker = ReclaimTracker::new();
    let epoch = tracker
        .begin([ObjectId::Block(7)].into_iter().collect())
        .unwrap();
    let _shared = tracker.lock_mutation_shared();
    assert!(!epoch.try_reclaim_if_untouched(ObjectId::Block(7), || Ok(true)).unwrap());
}
```

- [ ] **Step 2: Run the tests and verify RED**

Run: `cargo osdk test allocator_mutations_share_the_epoch`

Expected: compilation fails because the shared/exclusive epoch API does not exist.

- [ ] **Step 3: Replace the mutex with a read/write epoch gate**

Change `mutation_gate` to `Arc<RwMutex<()>>`. Normal mutations acquire `.read()`; `reclaim_if_untouched` acquires `.write()` before checking and marking `touched`. Add a test-only nonblocking exclusive probe only if OSTD exposes no suitable try-write API; do not expose it outside `cfg(ktest)`.

- [ ] **Step 4: Run reclaim tests and verify GREEN**

Run: `cargo osdk test reclaim`

Expected: all reclaim tracker tests pass.

- [ ] **Step 5: Commit**

```bash
git add kernel/core/src/fs/nv_pool/reclaim.rs
git commit -m "refactor(ext4): allow shared allocator epochs"
```

### Task 2: Exact Runtime Allocation Accounting

**Files:**
- Create: `kernel/core/src/fs/fs_impls/ext4/allocation.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/mod.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/fs.rs`
- Test: `kernel/core/src/fs/fs_impls/ext4/allocation.rs`

**Interfaces:**
- Produces: `AllocationAccounting::new(free_blocks: u64)`
- Produces: `try_claim(kind: ClaimKind, blocks: u32) -> Result<BlockClaim>`
- Produces: `BlockClaim::split`, `transfer`, `consume`, and idempotent `release` semantics.
- Produces: `physical_free_blocks() -> u64` and `available_blocks() -> u64`.

- [ ] **Step 1: Write failing accounting tests**

Cover exact admission, rollback-on-drop, delayed-to-PA transfer without a second charge, incremental PA growth, consumption decreasing physical free space, and concurrent claims never exceeding capacity.

```rust
#[ktest]
fn transferring_delalloc_to_inode_pa_does_not_double_charge() {
    let accounting = Arc::new(AllocationAccounting::new(100));
    let delayed = accounting.try_claim(ClaimKind::DelayedData, 8).unwrap();
    let pa = delayed.transfer(ClaimKind::InodePrealloc, 8).unwrap();
    assert_eq!(accounting.available_blocks(), 92);
    assert_eq!(pa.remaining(), 8);
}

#[ktest]
fn consuming_claim_decreases_physical_free_once() {
    let accounting = Arc::new(AllocationAccounting::new(100));
    let mut claim = accounting.try_claim(ClaimKind::CpuPrealloc, 16).unwrap();
    claim.consume(4).unwrap();
    assert_eq!(accounting.physical_free_blocks(), 96);
    assert_eq!(accounting.available_blocks(), 84);
}
```

- [ ] **Step 2: Run and verify RED**

Run: `cargo osdk test transferring_delalloc_to_inode_pa`

Expected: compilation fails because `allocation` and its types do not exist.

- [ ] **Step 3: Implement accounting and RAII claims**

Use atomic compare-and-exchange loops for admission. Keep claim ownership explicit in a non-`Clone` `BlockClaim`; its `Drop` releases remaining capacity. Require callers to hold a shared allocation epoch across transitions that also mutate group state. Use checked arithmetic and return `ENOSPC`, `EINVAL`, or `EOVERFLOW` rather than saturating ownership counts.

- [ ] **Step 4: Initialize accounting at mount**

Construct `AllocationAccounting` from the validated superblock free-block count. Keep existing public free-space reporting temporarily on the superblock until Task 4 switches every writer and reader together.

- [ ] **Step 5: Run tests and verify GREEN**

Run: `cargo osdk test allocation_accounting`

Expected: all accounting tests pass, including the concurrent overcommit test.

- [ ] **Step 6: Commit**

```bash
git add kernel/core/src/fs/fs_impls/ext4/allocation.rs kernel/core/src/fs/fs_impls/ext4/mod.rs kernel/core/src/fs/fs_impls/ext4/fs.rs
git commit -m "feat(ext4): add exact runtime block accounting"
```

### Task 3: Group-Local Buddy Reservations

**Files:**
- Modify: `kernel/core/src/fs/fs_impls/ext4/buddy_cache.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/block_group.rs`
- Test: `kernel/core/src/fs/fs_impls/ext4/buddy_cache.rs`
- Test: `kernel/core/src/fs/fs_impls/ext4/block_group.rs`

**Interfaces:**
- Produces: `BlockGroup::reserve_blocks(count, goal) -> Result<Option<GroupReservation>>`
- Produces: `BlockGroup::consume_reservation(&mut GroupReservation, count) -> Result<Range<Ext4Bid>>`
- Produces: `BlockGroup::discard_reservation(GroupReservation) -> Result<()>`
- Preserves: existing direct `alloc_blocks` behavior through the same group-local primitive.

- [ ] **Step 1: Write failing buddy-reservation tests**

Tests must prove a reserved range is excluded from `find_run` while its bitmap bits remain free, partial consumption sets only consumed bits, discard restores the unconsumed range, and concurrent consumers cannot overlap.

```rust
#[ktest]
fn prealloc_reservation_is_unavailable_but_not_bitmap_allocated() {
    let mut fixture = BlockGroupFixture::new(128);
    let reservation = fixture.group.reserve_blocks(16, fixture.first()).unwrap().unwrap();
    assert!(!fixture.group.is_block_allocated(reservation.start()));
    assert_ne!(fixture.group.reserve_blocks(16, fixture.first()).unwrap().unwrap().range(), reservation.range());
}
```

- [ ] **Step 2: Run and verify RED**

Run: `cargo osdk test prealloc_reservation_is_unavailable`

Expected: compilation fails because reservation APIs do not exist.

- [ ] **Step 3: Extend buddy state**

Represent PA-reserved ranges separately from the disk bitmap while maintaining buddy summaries. All reserve, consume, discard, bitmap, descriptor, and buddy mutations occur under `BlockGroup::metadata.write()`. Make `GroupReservation` non-`Clone` and bind it to one group and generation.

- [ ] **Step 4: Run buddy and block-group tests**

Run: `cargo osdk test buddy_cache`

Run: `cargo osdk test block_group`

Expected: new and existing tests pass.

- [ ] **Step 5: Commit**

```bash
git add kernel/core/src/fs/fs_impls/ext4/buddy_cache.rs kernel/core/src/fs/fs_impls/ext4/block_group.rs
git commit -m "feat(ext4): reserve preallocation ranges in group buddies"
```

### Task 4: Remove Exclusive Filesystem Locks From Block Allocation

**Files:**
- Modify: `kernel/core/src/fs/fs_impls/ext4/allocation.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/fs.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/impl_for_vfs/fs.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/super_block.rs`
- Test: `kernel/core/src/fs/fs_impls/ext4/fs.rs`

**Interfaces:**
- Produces: `Ext4::allocation_accounting()` and group-search helpers in `allocation.rs`.
- Changes: `alloc_blocks`, `alloc_reserved_blocks`, and `free_blocks` use shared epoch guards, runtime claims, and one group lock at a time.
- Changes: statfs and superblock persistence read exact runtime physical-free count.

- [ ] **Step 1: Write failing allocation-concurrency tests**

Add a test-only group hook/barrier that pauses an allocation while it owns group A. Start a second allocation targeted at group B and assert it reaches group B before group A is released. Add a same-group stress test asserting unique ranges and exact accounting.

```rust
#[ktest]
fn allocations_in_different_groups_overlap() {
    let fixture = MultiGroupFixture::new();
    let pause = fixture.pause_group_allocation(0);
    let first = fixture.spawn_alloc(0, 8);
    pause.wait_until_entered();
    let second = fixture.spawn_alloc(1, 8);
    assert!(second.wait_until_group_entered(1));
    pause.release();
    assert!(first.join().is_ok());
    assert!(second.join().is_ok());
}
```

- [ ] **Step 2: Run and verify RED**

Run: `cargo osdk test allocations_in_different_groups_overlap`

Expected: test times out or fails because `super_block.write()` and the mutation mutex serialize the two allocations.

- [ ] **Step 3: Move group search to `allocation.rs`**

Acquire a shared epoch guard, obtain or transfer a claim, and visit groups without holding the superblock lock. Hold only one group metadata lock at a time. On success consume the exact claim and update physical-free accounting; on failure let the claim roll back. Keep reserved-root permission checks before claim acquisition.

- [ ] **Step 4: Switch free-space readers and persistence**

Use runtime physical-free accounting for statfs and serialized superblock block counts. Targeted/global metadata sync acquire the exclusive epoch guard, sync group metadata, write the matching count, and only then clear dirty state.

- [ ] **Step 5: Remove obsolete block-count mutations**

Delete hot-path `SuperBlock::inc_free_blocks` and `dec_free_blocks` calls. Retain decoding and serialization helpers required for mount and disk images. Replace `reserve_data_blocks` and metadata reservation admission with `AllocationAccounting` claims.

- [ ] **Step 6: Run focused and full EXT4 ktests**

Run: `cargo osdk test allocations_in_different_groups_overlap`

Run: `cargo osdk test ext4`

Expected: different-group test passes; existing allocation, reservation, truncate, and persistence tests remain green.

- [ ] **Step 7: Commit**

```bash
git add kernel/core/src/fs/fs_impls/ext4/allocation.rs kernel/core/src/fs/fs_impls/ext4/fs.rs kernel/core/src/fs/fs_impls/ext4/impl_for_vfs/fs.rs kernel/core/src/fs/fs_impls/ext4/super_block.rs
git commit -m "perf(ext4): remove global allocation serialization"
```

### Task 5: Preallocation Core and Lifecycle

**Files:**
- Create: `kernel/core/src/fs/fs_impls/ext4/prealloc.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/mod.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/allocation.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/fs.rs`
- Test: `kernel/core/src/fs/fs_impls/ext4/prealloc.rs`

**Interfaces:**
- Produces: `PreallocSpace`, `PreallocKind`, `PreallocState`, and `PreallocManager`.
- Produces: `consume(count)`, `discard()`, stable-reference, retirement, and idempotent cleanup behavior.
- Produces: mount-wide pressure registry used only for reclaim/unmount.

- [ ] **Step 1: Write failing PA state-machine tests**

Cover partial consume, full consume, discard, double discard, publication loser cleanup, invalid generation rejection, and claim conservation.

- [ ] **Step 2: Run and verify RED**

Run: `cargo osdk test prealloc_state`

Expected: compilation fails because PA types do not exist.

- [ ] **Step 3: Implement PA state and manager**

Use an internal sleepable lock for PA state and stable `Arc` references. PA consumption locks PA then calls the owning group's consume primitive. Owner publication never nests owner and group locks. Registry entries are weak references plus last-use sequence so normal consumption avoids the registry lock.

- [ ] **Step 4: Implement pressure and unmount drain**

Discard CPU PAs first, then least-recently-used inode PAs. Retry ENOSPC once. Drain all PAs before final metadata sync. Return unused claims and buddy reservations exactly once.

- [ ] **Step 5: Run tests and verify GREEN**

Run: `cargo osdk test prealloc`

Expected: all PA lifecycle tests pass.

- [ ] **Step 6: Commit**

```bash
git add kernel/core/src/fs/fs_impls/ext4/prealloc.rs kernel/core/src/fs/fs_impls/ext4/mod.rs kernel/core/src/fs/fs_impls/ext4/allocation.rs kernel/core/src/fs/fs_impls/ext4/fs.rs
git commit -m "feat(ext4): add crash-safe preallocation spaces"
```

### Task 6: Adaptive Per-Inode Preallocation

**Files:**
- Modify: `kernel/core/src/fs/fs_impls/ext4/inode/block_mapping/extent/mod.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/inode/block_mapping/mod.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/inode/file.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/prealloc.rs`
- Test: `kernel/core/src/fs/fs_impls/ext4/inode/block_mapping/extent/mod.rs`
- Test: `kernel/core/src/fs/fs_impls/ext4/inode/file.rs`

**Interfaces:**
- Produces: per-inode PA set keyed by compatible logical windows.
- Produces: refill sizes of 4, 8, 16, and 32 MiB with 75 percent growth threshold.
- Enforces: eight windows and 32 MiB unused capacity per inode.

- [ ] **Step 1: Write failing policy tests**

Test first refill size, compatible-window reuse, incompatible random windows, growth after 75 percent consumption, no growth below the threshold, group-size cap, window cap eviction, and unused-capacity cap.

- [ ] **Step 2: Run and verify RED**

Run: `cargo osdk test inode_prealloc`

Expected: tests fail because extent allocation always calls the direct allocator.

- [ ] **Step 3: Add inode PA ownership**

Store inode PA state in `ExtentManager`. Select inode PA for large/sequential or window-matching requests. Convert the delayed claim into the requested PA claim and charge only refill capacity beyond the delayed request.

- [ ] **Step 4: Integrate allocation and rollback**

Consume the matching PA before direct group search. If extent insertion fails, free consumed blocks through the normal shared-epoch free path. Truncate clips/discards PA windows beyond the new size; unlink and inode reclaim discard all inode PAs.

- [ ] **Step 5: Run tests and verify GREEN**

Run: `cargo osdk test inode_prealloc`

Run: `cargo osdk test truncate_releases`

Expected: policy, rollback, truncate, and existing delayed-allocation tests pass.

- [ ] **Step 6: Commit**

```bash
git add kernel/core/src/fs/fs_impls/ext4/inode/block_mapping/extent/mod.rs kernel/core/src/fs/fs_impls/ext4/inode/block_mapping/mod.rs kernel/core/src/fs/fs_impls/ext4/inode/file.rs kernel/core/src/fs/fs_impls/ext4/prealloc.rs
git commit -m "feat(ext4): add adaptive inode preallocation"
```

### Task 7: Per-CPU Locality Preallocation

**Files:**
- Modify: `kernel/core/src/fs/fs_impls/ext4/prealloc.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/allocation.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/fs.rs`
- Test: `kernel/core/src/fs/fs_impls/ext4/prealloc.rs`

**Interfaces:**
- Produces: one CPU-local 2 MiB PA per mounted filesystem and CPU.
- Produces: policy selection that uses CPU PA for small-file or spatially unrelated requests.
- Requires: current CPU pinned during CPU-local lookup/publication only.

- [ ] **Step 1: Write failing CPU-local tests**

Test same-CPU reuse, different-CPU independence, replacement discarding the previous PA, group cap, policy fallback from a nonmatching inode PA, and unmount drain.

- [ ] **Step 2: Run and verify RED**

Run: `cargo osdk test cpu_prealloc`

Expected: compilation fails because CPU PA storage does not exist.

- [ ] **Step 3: Add CPU-local storage**

Use `alloc_cpu_local` with one lock-protected optional PA per CPU. Pin preemption while resolving the current CPU slot, clone the PA reference, then release CPU-local protection before PA/group locking.

- [ ] **Step 4: Implement selection and refill**

Use 512 blocks at 4 KiB, converted by filesystem block size and capped at one eighth of a group. Consume an existing CPU PA before refill. Refill publication races discard the loser without consulting the hot-path global registry.

- [ ] **Step 5: Run tests and verify GREEN**

Run: `cargo osdk test cpu_prealloc`

Expected: all CPU policy, isolation, replacement, and drain tests pass.

- [ ] **Step 6: Commit**

```bash
git add kernel/core/src/fs/fs_impls/ext4/prealloc.rs kernel/core/src/fs/fs_impls/ext4/allocation.rs kernel/core/src/fs/fs_impls/ext4/fs.rs
git commit -m "feat(ext4): add per-CPU locality preallocation"
```

### Task 8: Persistence, Reclaim, and Fault Rollback

**Files:**
- Modify: `kernel/core/src/fs/fs_impls/ext4/allocation.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/prealloc.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/fs.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/block_group.rs`
- Modify: `kernel/core/src/fs/nv_pool/reclaim.rs`
- Test: corresponding module ktests

**Interfaces:**
- Preserves: group metadata before superblock ordering on nojournal mounts.
- Preserves: one enclosing JBD2 commit boundary for group and superblock after-images.
- Produces: persistent allocator failure state when rollback cannot restore ownership safely.

- [ ] **Step 1: Write failing fault-injection tests**

Inject extent insertion failure after PA consumption, group metadata sync failure, superblock sync failure, JBD2 capture failure, and reclaim racing with PA consumption. Assert no range is returned twice and all retryable state remains dirty.

- [ ] **Step 2: Run and verify RED**

Run: `cargo osdk test allocation_rollback`

Expected: at least the PA-consumption and exclusive-sync cases fail before integration.

- [ ] **Step 3: Implement ordered sync and rollback**

Hold the exclusive allocation epoch across touched-group sync and superblock snapshot. Retire uncertain PAs. Route consumed-allocation rollback through `free_blocks` so bitmap, group counters, physical-free accounting, journal capture, and reclaim touches use one path.

- [ ] **Step 4: Run failure and recovery tests**

Run: `cargo osdk test allocation_rollback`

Run: `cargo osdk test reclaim`

Run: `cargo osdk test journal`

Expected: all injected failures preserve ownership and persistent dirty state.

- [ ] **Step 5: Commit**

```bash
git add kernel/core/src/fs/fs_impls/ext4/allocation.rs kernel/core/src/fs/fs_impls/ext4/prealloc.rs kernel/core/src/fs/fs_impls/ext4/fs.rs kernel/core/src/fs/fs_impls/ext4/block_group.rs kernel/core/src/fs/nv_pool/reclaim.rs
git commit -m "fix(ext4): order preallocation persistence and rollback"
```

### Task 9: Allocation and PA Research Statistics

**Files:**
- Modify: `kernel/core/src/fs/fs_impls/ext4/stats.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/allocation.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/prealloc.rs`
- Modify: `kernel/core/src/fs/fs_impls/ext4/fs.rs`
- Test: `kernel/core/src/fs/fs_impls/ext4/stats.rs`

**Interfaces:**
- Produces counters specified in the design without hot-path logging.
- Extends the existing EXT4 research summary with one structured allocation line.

- [ ] **Step 1: Write failing counter tests**

Assert allocation requests/blocks, groups scanned, group lock wait/hold, inode/CPU PA hit/miss/refill/consume/discard, claim failure, pressure retry, epoch stall, and rollback counters reset correctly after snapshot.

- [ ] **Step 2: Run and verify RED**

Run: `cargo osdk test allocation_stats`

Expected: compilation fails because allocation stats are absent.

- [ ] **Step 3: Implement counters and reporting**

Use atomics guarded by `cfg(any(feature = "ext4-research-stats", ktest))`. Record aggregate durations and counts, never one log line per allocation. Add the report beside existing writeback-pipeline reporting.

- [ ] **Step 4: Run and verify GREEN**

Run: `cargo osdk test allocation_stats`

Expected: snapshot/reset and disabled-feature builds pass.

- [ ] **Step 5: Commit**

```bash
git add kernel/core/src/fs/fs_impls/ext4/stats.rs kernel/core/src/fs/fs_impls/ext4/allocation.rs kernel/core/src/fs/fs_impls/ext4/prealloc.rs kernel/core/src/fs/fs_impls/ext4/fs.rs
git commit -m "perf(ext4): report allocation and preallocation statistics"
```

### Task 10: Full Verification and Performance Matrix

**Files:**
- Modify only if tests expose defects in files already owned by Tasks 1-9.
- Record results under the existing research result hierarchy; do not commit generated disk images or raw QEMU artifacts.

**Interfaces:**
- Verifies all design claims and performance acceptance criteria.

- [ ] **Step 1: Format and static-check changed Rust files**

Run: `make format`

Run: `cargo osdk clippy --ktests --features ext4-research-stats -- --no-deps`

Expected: no formatting diff outside touched files and no warnings.

- [ ] **Step 2: Run focused and complete kernel tests**

Run: `cargo osdk test ext4`

Run: `make ktest`

Expected: all EXT4 and full kernel ktests pass.

- [ ] **Step 3: Run concurrency stress**

Run the existing multi-file random-4K and append-4K workloads with 1, 4, and 16 jobs on nojournal and JBD2 mounts. Capture allocation stats, writeback stats, fio JSON, serial logs, and exact QEMU command/environment.

Expected: no duplicate allocation, filesystem error, journal error, or counter mismatch; different-group allocator overlap is nonzero and filesystem-wide exclusive allocator stalls are absent.

- [ ] **Step 4: Run preallocated random-4K control**

Run the existing preallocated random-4K matrix unchanged. Treat it as a control for writeback/completion bottlenecks rather than evidence of allocator scaling.

Expected: allocator request volume is much lower than append/new-file workloads; reported PA metrics and extent-conversion behavior explain the difference.

- [ ] **Step 5: Review the complete diff**

Run: `git diff --check HEAD~9..HEAD`

Run: `git status --short`

Confirm no generated images, unrelated user changes, per-allocation logging, or unsafe Rust entered the commits.

- [ ] **Step 6: Commit verification-only fixes if required**

Stage only the exact source or test files changed while resolving verification failures, inspect the staged diff, and commit them with `test(ext4): harden scalable allocation verification`. If verification requires no fixes, do not create an empty commit.
