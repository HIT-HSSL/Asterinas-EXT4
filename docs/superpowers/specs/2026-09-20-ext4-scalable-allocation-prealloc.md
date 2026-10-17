# Scalable EXT4 Allocation and Preallocation Design

## Goal

Remove filesystem-wide exclusive locking from the normal EXT4 block-allocation
path and add Linux-style inode and per-CPU preallocation so allocations for
different inodes and block groups can proceed concurrently.

## Scope

This design applies to extent-backed regular files on conventional, JBD2, and
NVPool mounts. It covers delayed-allocation accounting, physical data and
extent-metadata allocation, preallocation ownership, reclaim coordination,
metadata persistence, rollback, observability, and tests.

Indirect-block files retain the existing allocation behavior until they are
explicitly migrated to the new allocator. Inode allocation is outside this
change. The writeback submit/completion worker topology is also outside this
change, except for consuming the new allocation interfaces.

## Current Problem

`Ext4::alloc_blocks_inner` currently acquires the filesystem-wide reclaim
mutation mutex and `super_block.write()` before searching block groups. Both
exclusive locks remain held through the complete ring search and the selected
group's buddy/bitmap update. Consequently, allocations for unrelated inodes
and unrelated block groups serialize.

Delayed-allocation reservation also takes `super_block.write()` merely to
update reservation counters. Background writeback additionally holds
`inode.inner.write()` while collecting pages, materializing delayed extents,
mapping pages, and submitting BIOs. The allocator changes in this design do
not by themselves shorten that outer inode lock, but they remove mount-wide
exclusive allocation serialization underneath it.

## Design Principles

1. Normal allocators for different groups must not exclude one another through
   a filesystem-wide exclusive lock.
2. The block-group bitmap and descriptor remain authoritative for physical
   ownership and are mutated under the block group's metadata write lock.
3. Runtime free-space admission is exact enough to prevent oversubscription.
4. Metadata sync and reclaim obtain a coherent allocation snapshot without
   blocking normal allocators against each other.
5. Unconsumed preallocation never appears allocated in the on-disk bitmap, so
   a crash cannot leak an unpublished PA.
6. Every claim transition and rollback is explicit and idempotent.

## Allocation Epoch Gate

Replace `ReclaimTracker::mutation_gate: Mutex<()>` with a sleepable read/write
epoch gate.

- Normal block allocation, block freeing, and PA consumption hold a shared
  guard. Shared holders do not exclude each other.
- Reclaim candidate revalidation and bitmap transition hold an exclusive
  guard.
- Targeted metadata sync and global metadata sync hold an exclusive guard
  while persisting touched groups and the matching superblock free count.
- The gate is an ordering boundary, not an allocator ownership lock. Normal
  allocation ownership is decided only under per-group locks.

The existing touched-object protocol remains in place. An allocation or free
records touched blocks before releasing its shared epoch guard. Reclaim checks
the touched set and performs its transition while holding the exclusive guard.

## Runtime Space Accounting

Move hot-path block accounting out of the mutable in-memory `SuperBlock` and
into an `AllocationAccounting` component owned by `Ext4`.

The component tracks:

- exact physical free blocks;
- blocks available for new unreserved claims;
- delayed-data claims;
- extent-metadata claims;
- inode-PA claims;
- per-CPU-PA claims.

Claims are acquired with compare-and-exchange loops. A claim must be acquired
before reserving a buddy range. Existing delayed claims are transferred into a
PA or allocated state rather than charged again. Additional PA capacity beyond
the delayed request acquires only the incremental claim.

All multi-counter transitions execute under a shared allocation epoch guard.
Metadata sync takes the exclusive guard, preventing it from observing a
partially completed transition. The persisted superblock free count comes from
the exact physical-free counter, not from reservation counters.

`SuperBlock` continues to own immutable geometry, feature flags, inode counts,
and its serialized representation. Block alloc/free no longer holds
`super_block.write()` while searching groups. Sync briefly acquires the
superblock write lock only to build and mark the persisted snapshot.

## Group Allocation

Allocation starts from the goal group and visits candidate groups in ring
order. For each candidate:

1. Acquire only that `BlockGroup::metadata.write()` lock.
2. Consult the buddy cache, excluding active PA-reserved ranges.
3. Reserve or allocate the best contiguous run.
4. Update the group descriptor and bitmap only for consumed allocations.
5. Release the group lock before visiting another group or updating an extent
   tree.

The allocator never holds two block-group metadata locks simultaneously. A
failed group attempt releases its lock before the next group is examined.

## Preallocation Model

Both inode and per-CPU preallocation are enabled, but an allocation request
selects one policy.

- Large files, sequential streams, and requests near an existing inode PA use
  inode preallocation.
- Small-file and spatially unrelated requests use the current CPU's locality
  preallocation.
- Existing matching PAs are consumed before a refill or direct group search.

A `PreallocSpace` records the owning group, physical range, remaining range,
optional inode/logical window, policy, state, and last-use sequence. Its
unconsumed blocks are unavailable in the in-memory buddy but remain clear in
the on-disk bitmap.

### Inode Preallocation

- Initial refill: 1,024 blocks (4 MiB at a 4 KiB block size).
- Growth: 4, 8, 16, then 32 MiB after at least 75 percent of the previous PA
  was consumed.
- Maximum unused capacity per inode: 32 MiB.
- Maximum active logical windows per inode: eight.
- A refill is capped at one eighth of a block group.
- A PA matches only a compatible logical window; unrelated random logical
  writes may create additional windows up to the cap.

### Per-CPU Preallocation

- Refill: 512 blocks (2 MiB at a 4 KiB block size).
- Maximum: one active PA per CPU and filesystem.
- A refill is capped at one eighth of a block group.
- CPU-local lookup does not take a mount-wide registry lock.

The fixed block counts are converted from bytes using the mounted filesystem's
block size and clamped to at least the current request size.

## Preallocation Locking

Normal PA lookup does not use a mount-wide lock.

- Inode PA lookup uses only the inode's PA lock.
- Per-CPU PA lookup uses only the current CPU's local PA lock with preemption
  pinned while accessing CPU-local storage.
- A lookup takes a stable PA reference and releases the owner lock before
  acquiring the PA and group locks.
- PA consumption locks the PA first and its owning group second.
- Publishing a newly refilled PA never holds an owner lock and a group lock at
  the same time. A publication race keeps one PA and returns the loser.

A mount-wide PA registry exists only for pressure reclaim and unmount. It is
not consulted when consuming an existing inode or CPU PA.

## Claim and PA State Transitions

The supported transitions are:

```text
Free -> DelallocClaim -> PreallocClaim -> BitmapAllocated -> ExtentInserted
Free -----------------> PreallocClaim -> BitmapAllocated -> ExtentInserted
Free ---------------------------------> BitmapAllocated -> ExtentInserted
```

- Delayed allocation acquires a `DelallocClaim` without selecting a group.
- PA refill transfers the requested delayed claim and acquires an incremental
  claim only for extra capacity.
- Reserving a PA removes its range from the in-memory buddy but does not change
  the disk bitmap or exact physical-free counter.
- Consuming a PA sets bitmap bits, updates the group descriptor, decreases the
  exact physical-free count, and decreases the owning claim.
- Inserting the extent publishes the physical mapping to the inode.

## Release and Pressure Policy

- Truncate releases inode-PA logical windows wholly beyond the retained range
  and clips or discards intersecting unused portions.
- Unlink and inode reclamation discard all inode PAs.
- Replacing a CPU PA discards its unconsumed predecessor.
- Unmount drains all inode and CPU PAs before final metadata sync.
- ENOSPC first discards per-CPU PAs, then least-recently-used inode PAs, and
  retries allocation once.
- Normal sync does not discard healthy PAs.

Discarding a PA returns the range to the owning group's buddy and returns its
claim. Because the on-disk bitmap was never set, no disk metadata update is
required solely for an unused PA.

## Failure and Rollback

- Refill failure returns both the buddy reservation and its claim.
- Publication races immediately discard the unpublished PA.
- If bitmap allocation succeeds but extent insertion fails, the normal free
  path reverses the bitmap, group descriptor, exact free count, and reclaim
  tracking changes.
- A failed rollback leaves the affected PA retired and reports the original
  allocation error plus a persistent filesystem error.
- JBD2 capture or nojournal metadata-write failure prevents reuse of the
  affected range until recovery resolves its state.
- Repeating cleanup of an already retired or consumed PA is harmless.

## Persistence Ordering

Allocation and free operations complete all accounting, group metadata, and
reclaim-touch updates before releasing their shared epoch guard.

Targeted or global metadata sync obtains the exclusive epoch guard and then:

1. snapshots the touched groups;
2. persists each group bitmap and descriptor;
3. snapshots the exact physical-free counter into the superblock image;
4. persists or journals the superblock image;
5. releases the exclusive guard.

For JBD2 mounts, group metadata and the superblock after-image belong to the
same enclosing commit boundary. For nojournal mounts, group metadata is
written before the superblock free count. Unconsumed PAs require no recovery
because their disk bitmap bits remain clear.

## Observability

Add research counters for:

- allocation requests, requested blocks, and allocated blocks;
- per-group lock wait and hold time;
- number of groups scanned per allocation;
- inode-PA and CPU-PA hits, misses, refills, consumed blocks, and discarded
  blocks;
- claim failures and ENOSPC reclaim retries;
- allocation epoch exclusive stalls;
- rollback attempts and failures.

Counters must be available through the existing EXT4 research-stat report and
must not add logging to the allocation hot path.

## Verification

Unit tests must prove:

- allocators targeting different groups can overlap while holding shared epoch
  guards;
- concurrent allocation from one group never returns overlapping blocks;
- group search releases one group before locking the next;
- delayed claims transferred to PAs are not charged twice;
- every PA refill, consume, discard, and rollback preserves accounting;
- inode and CPU policy selection follows the configured thresholds;
- inode PA growth and caps are enforced;
- truncate, unlink, ENOSPC reclaim, and unmount return unused blocks;
- extent-insertion failure frees consumed blocks;
- reclaim cannot free a concurrently touched block;
- metadata sync observes matching group and superblock counters.

Integration tests must cover JBD2 and nojournal remount consistency after
allocation, PA consumption, PA discard, injected allocation failure, and
injected metadata-write failure.

Performance validation must compare 1, 4, and 16 jobs for new-file append-4K,
multi-file random-4K, and preallocated random-4K. The primary allocator
success criteria are:

- different-inode allocations no longer serialize on a filesystem-wide
  exclusive allocator lock;
- allocator group-lock wait and scan metrics explain remaining contention;
- inode and CPU PA hit rates are reported;
- throughput scaling is evaluated separately from the writeback submit and
  completion-worker bottlenecks.

## Expected File Boundaries

- `kernel/core/src/fs/fs_impls/ext4/allocation.rs`: epoch guard, exact runtime
  accounting, claims, and group-search orchestration.
- `kernel/core/src/fs/fs_impls/ext4/prealloc.rs`: PA types, inode and CPU policy,
  registry, refill, consumption, discard, and pressure reclaim.
- `kernel/core/src/fs/fs_impls/ext4/block_group.rs`: buddy PA reservations and
  group-local consume/discard operations.
- `kernel/core/src/fs/fs_impls/ext4/inode/block_mapping/extent/mod.rs`: PA
  selection, delayed-claim transfer, and extent insertion integration.
- `kernel/core/src/fs/nv_pool/reclaim.rs`: shared/exclusive allocation epoch
  protocol.
- `kernel/core/src/fs/fs_impls/ext4/fs.rs`: mount construction, persistence
  orchestration, compatibility wrappers, and stat reporting.
- Existing EXT4 ktests and research benchmarks: correctness, concurrency,
  failure-injection, and performance coverage.
