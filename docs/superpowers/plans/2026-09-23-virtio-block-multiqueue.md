# VirtIO Block Multi-Queue Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give Asterinas virtio-blk a functional and scalable multi-queue submission/completion path capable of driving an NVMe-backed QEMU block device with several concurrent request queues.

**Architecture:** Negotiate `VIRTIO_BLK_F_MQ`, build queue-local state for every effective request queue, route BIOs round-robin into per-queue staging queues, and run CPU-affine submit/completion workers per queue. Preserve cross-queue flush ordering with a conservative device-wide flush gate, while keeping normal read/write locks and request tracking queue-local.

**Tech Stack:** Rust `no_std`, Asterinas OSTD synchronization and kernel threads, `aster_block` BIO/request queues, VirtIO 1.2 split virtqueues, PCI MSI-X, Bash QEMU argument generation, kernel `ktest`, fio.

**Spec:** `docs/superpowers/specs/2026-09-23-virtio-block-multiqueue-design.md`

## Global Constraints

- Preserve the user's uncommitted queue-size=1024 implementation in `device.rs`, `queue.rs`, `writeback.rs`, and `tools/qemu_args.sh`.
- Keep `kernel/` entirely safe Rust.
- Keep the single-queue path working when MQ is absent or QEMU uses `num-queues=1`.
- Use round-robin routing so one EXT4 writeback producer exercises all enabled hardware queues.
- Use at most 64 request queues and at most the number of online CPUs.
- Keep normal read/write data-path locks queue-local.
- Do not execute DMA synchronization or BIO completion callbacks in the IRQ top half.
- Do not busy-spin when a virtqueue lacks descriptors.
- Preserve write-before-flush ordering across all queues.
- Do not change unrelated QEMU block devices from `num-queues=1`.
- Build and benchmark in release mode; debug-build performance is not evidence.
- The existing queue-depth files are already dirty. Before implementation, either commit that
  prerequisite as its own reviewed change or run this plan without creating intermediate commits;
  never stage unrelated user changes merely because a task touches the same file.

---

### Task 1: Lock the queue-count and routing policy with pure tests

**Files:**
- Modify: `kernel/core/comps/virtio/src/device/block/mod.rs`
- Create: `kernel/core/comps/virtio/src/device/block/mq.rs`

**Interfaces:**
- Produces: `const DRIVER_MAX_QUEUES: u16 = 64`.
- Produces: `fn effective_queue_count(mq: bool, config_queues: u16, transport_queues: u16, cpu_count: usize) -> Result<u16, VirtioDeviceError>`.
- Produces: `struct QueueRouter { next: AtomicUsize, queue_count: usize }` with `fn select(&self) -> usize`.

- [ ] **Step 1: Add failing queue-count tests**

Add `#[ktest]` cases that encode the exact policy:

```rust
assert_eq!(effective_queue_count(false, 8, 8, 8).unwrap(), 1);
assert_eq!(effective_queue_count(true, 8, 8, 4).unwrap(), 4);
assert_eq!(effective_queue_count(true, 128, 128, 128).unwrap(), 64);
assert!(effective_queue_count(true, 0, 8, 8).is_err());
assert!(effective_queue_count(true, 8, 0, 8).is_err());
```

- [ ] **Step 2: Add a failing round-robin test**

For four queues, call `select()` ten times and assert:

```rust
assert_eq!(selected, [0, 1, 2, 3, 0, 1, 2, 3, 0, 1]);
```

- [ ] **Step 3: Run the focused ktest build and observe failure**

Run:

```bash
make ktest CARGO_OSDK_TEST_ARGS=effective_queue_count
```

Expected: compilation fails because `effective_queue_count` and `QueueRouter` do not exist. If the
repository's ktest filter uses exact suffixes, run the exact generated test names instead.

- [ ] **Step 4: Implement the pure MQ policy module**

Implement `effective_queue_count` with the spec's limits and implement `QueueRouter` using
`AtomicUsize::fetch_add(Ordering::Relaxed) % queue_count`. Reject zero CPU count and zero queue
counts when MQ is enabled.

- [ ] **Step 5: Export the module and rerun focused tests**

Run the two exact ktests. Expected: all queue-count and routing assertions pass.

- [ ] **Step 6: Commit the policy slice**

```bash
git add kernel/core/comps/virtio/src/device/block/mod.rs \
        kernel/core/comps/virtio/src/device/block/mq.rs
git commit -m "feat(virtio-blk): define multiqueue selection policy"
```

---

### Task 2: Negotiate MQ and read the device-specific queue count

**Files:**
- Modify: `kernel/core/comps/virtio/src/device/block/mod.rs`
- Modify: `kernel/core/comps/virtio/src/device/block/device.rs`

**Interfaces:**
- Consumes: `effective_queue_count` from Task 1.
- Produces: `ConfigManager<VirtioBlockConfig>::num_queues() -> u16`.
- Produces: `BlockFeatures::ALL_SUPPORTED` including `BlockFeatures::MQ`.

- [ ] **Step 1: Add failing feature-mask and config-offset tests**

Add a test asserting that an offered MQ bit survives block feature negotiation:

```rust
let offered = (BlockFeatures::BLK_SIZE | BlockFeatures::FLUSH | BlockFeatures::MQ).bits();
assert!(BlockFeatures::negotiated_with_device(offered).contains(BlockFeatures::MQ));
```

Add a compile-time offset assertion that `num_queues` remains a `u16` at the expected VirtIO block
configuration offset. Use `offset_of!(VirtioBlockConfig, num_queues)` rather than a duplicated
magic number in production code.

- [ ] **Step 2: Run the focused feature test and observe failure**

Expected: the test fails because `ALL_SUPPORTED` currently removes `MQ`.

- [ ] **Step 3: Add MQ to the supported feature set**

Change the mask to:

```rust
const ALL_SUPPORTED: u64 =
    Self::BLK_SIZE.bits() | Self::FLUSH.bits() | Self::MQ.bits();
```

- [ ] **Step 4: Add the `num_queues()` config accessor**

Read `u16` at `offset_of!(VirtioBlockConfig, num_queues)`. Call it only when the negotiated feature
set contains `MQ`; otherwise pass `1` into `effective_queue_count` without touching the optional
field.

- [ ] **Step 5: Replace the existing transport-only warning logic**

In `DeviceInner::init`, derive the effective queue count from negotiated MQ, block config,
transport queue count, and `ostd::cpu::num_cpus()`. Log all four inputs and the result once.

- [ ] **Step 6: Run focused ktests and check the component**

Run:

```bash
cargo osdk check --package aster-virtio
```

Expected: success with MQ included only for block devices that offer it.

- [ ] **Step 7: Commit protocol negotiation**

```bash
git add kernel/core/comps/virtio/src/device/block/mod.rs \
        kernel/core/comps/virtio/src/device/block/device.rs
git commit -m "feat(virtio-blk): negotiate multiqueue support"
```

---

### Task 3: Extract independent `BlockQueue` state

**Files:**
- Create: `kernel/core/comps/virtio/src/device/block/queue.rs`
- Modify: `kernel/core/comps/virtio/src/device/block/mod.rs`
- Modify: `kernel/core/comps/virtio/src/device/block/device.rs`

**Interfaces:**
- Produces: `BlockQueue::new(index: u16, requested_size: u16, transport: &mut dyn VirtioTransport) -> Result<Arc<BlockQueue>, VirtioDeviceError>`.
- Produces: `BlockQueue::submit(BioRequest)`.
- Produces: `BlockQueue::drain_completions() -> Vec<CompletedRequest>` where
  `CompletedRequest { request: BioRequest, status: BioStatus }`.
- Produces: `BlockQueue::queue_size() -> u16`.

- [ ] **Step 1: Add a failing token-table unit test**

Extract a queue-local token table backed by `Vec<Option<SubmittedRequest>>`. Test insertion,
duplicate-token rejection, removal, and removal of an empty slot. The table capacity must equal the
queue's negotiated descriptor count.

- [ ] **Step 2: Move single-queue fields into `BlockQueue`**

Move these existing fields without changing behavior:

```rust
queue_size
SpinLock<VirtQueue>
Arc<DmaStream> request storage
Arc<DmaStream> response storage
SyncIdAlloc
submitted request table
```

Keep one `BlockQueue` in `DeviceInner` temporarily so this step remains independently reviewable.

- [ ] **Step 3: Replace `BTreeMap` tracking with the token table**

Install `SubmittedRequest` at `token as usize` while the virtqueue lock is held and before calling
`notify()`. Completion takes the matching slot and treats a missing entry as a driver invariant
failure rather than silently completing the wrong BIO.

- [ ] **Step 4: Preserve queue-size=1024 allocation**

Allocate request/response DMA storage from each queue's actual size using the existing
`div_ceil(PAGE_SIZE)` logic. Keep the current 1024 maximum and smaller-device fallback.

- [ ] **Step 5: Run queue and driver checks**

Run the existing modern ring frame-count ktest and:

```bash
cargo osdk check --package aster-virtio
```

Expected: the one-queue runtime behavior still compiles with no data-path field left at
`DeviceInner` level.

- [ ] **Step 6: Commit queue-state extraction**

```bash
git add kernel/core/comps/virtio/src/device/block/mod.rs \
        kernel/core/comps/virtio/src/device/block/device.rs \
        kernel/core/comps/virtio/src/device/block/queue.rs
git commit -m "refactor(virtio-blk): isolate request queue state"
```

---

### Task 4: Initialize and register all hardware virtqueues

**Files:**
- Modify: `kernel/core/comps/virtio/src/device/block/device.rs`
- Modify: `kernel/core/comps/virtio/src/device/block/queue.rs`

**Interfaces:**
- Consumes: `BlockQueue::new` from Task 3.
- Produces: `DeviceInner::queues: Box<[Arc<BlockQueue>]>`.
- Produces: `DeviceInner::queue_count() -> usize` and `DeviceInner::queue(index) -> &Arc<BlockQueue>`.

- [ ] **Step 1: Add a pure queue-layout construction test**

Test a helper that receives per-index maximum sizes and returns actual sizes. Cover equal sizes,
smaller fallback queues, zero-sized queue rejection, and a non-power-of-two rejection under the
current `VirtQueue` contract.

- [ ] **Step 2: Construct queues `0..effective_queue_count`**

For each index, query `max_queue_size(index)`, choose `min(1024, advertised)`, allocate its
`BlockQueue`, and push it into a temporary vector. Publish the boxed slice only after every queue
succeeds, preserving all-or-nothing initialization.

- [ ] **Step 3: Register one callback per queue**

Call:

```rust
transport.register_queue_callback(index, callback, true)?;
```

The callback captures only that queue and wakes its completion wait queue. Keep the existing
shared-vector fallback supplied by the PCI transport.

- [ ] **Step 4: Derive block metadata from the smallest queue**

Set `max_nr_segments_per_bio` to:

```text
min(queue.queue_size() for every queue) - 2
```

Reject any selected queue smaller than two descriptors.

- [ ] **Step 5: Add initialization diagnostics**

Print one line containing MQ offered/negotiated, configured/effective counts, and a compact list of
queue sizes. Do not print per-request logs.

- [ ] **Step 6: Compile and boot the one-queue fallback**

Run the component check, then boot with `RESEARCH_BLOCK_NUM_QUEUES=1`. Expected: one queue is
created and existing block-device discovery succeeds.

- [ ] **Step 7: Commit multi-queue initialization**

```bash
git add kernel/core/comps/virtio/src/device/block/device.rs \
        kernel/core/comps/virtio/src/device/block/queue.rs
git commit -m "feat(virtio-blk): initialize multiple request queues"
```

---

### Task 5: Add per-queue software staging and round-robin dispatch

**Files:**
- Modify: `kernel/core/comps/virtio/src/device/block/device.rs`
- Modify: `kernel/core/src/device/registry/block.rs`

**Interfaces:**
- Consumes: `QueueRouter` and `DeviceInner::queue_count()`.
- Produces: `BlockDevice::queue_count() -> usize`.
- Produces: `BlockDevice::handle_requests(queue_index: usize)`.
- Produces: one `BioRequestSingleQueue` per hardware request queue.

- [ ] **Step 1: Add a failing one-producer distribution test**

Create four staging queues, enqueue 1024 non-mergeable mock BIOs through `BlockDevice` routing,
and assert each queue receives 256. Use separated sector ranges so request merging cannot hide the
routing result.

- [ ] **Step 2: Replace the single staging queue with a boxed slice**

Create one `BioRequestSingleQueue` per hardware queue. `BlockDevice::enqueue` chooses the queue
with `QueueRouter` and calls only that queue's `enqueue`.

- [ ] **Step 3: Make request handling queue-indexed**

Change `handle_requests()` to `handle_requests(queue_index)`. It dequeues from the matching
staging queue and submits only to the corresponding `BlockQueue`.

- [ ] **Step 4: Spawn one submit worker per queue**

In the block registry, loop over `0..device.queue_count()`. Bind queue `i`'s worker to CPU
`i % num_cpus()` with `ThreadOptions::cpu_affinity`, then repeatedly call
`handle_requests(i)`.

- [ ] **Step 5: Verify single-producer and multi-producer routing**

Run the routing ktest twice: once from one test thread and once from four concurrent producers.
Expected: every queue receives work, no BIO is lost, and total dequeue count equals total enqueue
count.

- [ ] **Step 6: Boot with two and four queues**

Use `RESEARCH_BLOCK_NUM_QUEUES=2` and `4`. Expected: initialization logs show the requested queue
count and raw reads/writes complete without timeout.

- [ ] **Step 7: Commit the first functional MQ milestone**

```bash
git add kernel/core/comps/virtio/src/device/block/device.rs \
        kernel/core/src/device/registry/block.rs
git commit -m "feat(virtio-blk): dispatch requests across hardware queues"
```

At this commit the driver has functional multi-queue submission. Do not make performance claims
until Tasks 6-10 are complete.

---

### Task 6: Defer queue completion out of hard IRQ context

**Files:**
- Modify: `kernel/core/comps/virtio/src/device/block/queue.rs`
- Modify: `kernel/core/comps/virtio/src/device/block/device.rs`
- Modify: `kernel/core/src/device/registry/block.rs`

**Interfaces:**
- Produces: `BlockQueue::notify_completion_irq()` for the IRQ callback.
- Produces: `BlockDevice::handle_completions(queue_index: usize)` for a kernel worker.
- Consumes: `BlockQueue::drain_completions() -> Vec<CompletedRequest>` from Task 3.

- [ ] **Step 1: Add failing completion-isolation tests**

Use two fake queue states with overlapping token numbers. Complete token 0 on queue 1 and assert
that queue 0's token 0 remains installed. Then inject used tokens in reverse order and assert each
BIO receives exactly one completion.

- [ ] **Step 2: Reduce the IRQ callback to a wakeup**

The registered callback must only increment an IRQ counter and call the queue's completion
`WaitQueue::wake_one()`. Remove DMA synchronization and BIO completion from the IRQ path.

- [ ] **Step 3: Implement completion draining in task context**

The completion worker waits until `VirtQueue::can_pop()` is true, drains all available tokens, and
then performs response DMA sync, read-data DMA sync, status conversion, and ID release outside the
virtqueue lock. It returns `CompletedRequest` values to `BlockDevice::handle_completions`, which
performs device-wide accounting and invokes BIO completion callbacks.

- [ ] **Step 4: Spawn and pin one completion worker per queue**

Bind it to the same CPU as that queue's submit worker. Ensure the registry stores or joins worker
handles consistently with current device lifecycle expectations.

- [ ] **Step 5: Run out-of-order and shared-IRQ tests**

Trigger wakeups for queues with and without used entries. Expected: spurious wakeups return safely,
each used ring is drained independently, and queue-local tokens cannot collide.

- [ ] **Step 6: Commit deferred completion**

```bash
git add kernel/core/comps/virtio/src/device/block/queue.rs \
        kernel/core/comps/virtio/src/device/block/device.rs \
        kernel/core/src/device/registry/block.rs
git commit -m "perf(virtio-blk): defer per-queue completions"
```

---

### Task 7: Replace descriptor busy spinning with queue-local backpressure

**Files:**
- Modify: `kernel/core/comps/virtio/src/device/block/queue.rs`

**Interfaces:**
- Produces: queue-local `descriptor_wait_queue: WaitQueue`.
- Produces: `BlockQueue::wait_and_add(...) -> u16`, returning the allocated virtqueue token.

- [ ] **Step 1: Add a failing descriptor-exhaustion test**

Fill a small fake queue, start a submitter that needs three descriptors, verify it enters the wait
path, complete one request, and verify the submitter resumes exactly once.

- [ ] **Step 2: Implement predicate-based sleeping**

Replace the `loop { lock; if unavailable { continue; } }` pattern with `WaitQueue::wait_until`.
The predicate locks only this queue, checks descriptor capacity, installs the chain when capacity
exists, and returns the token.

- [ ] **Step 3: Wake descriptor waiters after recycling**

After completion pops used descriptors and releases the queue lock, call
`descriptor_wait_queue.wake_all()`. Record stall count and elapsed stall time.

- [ ] **Step 4: Verify no busy-spin pattern remains**

Run:

```bash
rg -n "available_desc\(\).*continue|if .*available_desc" \
  kernel/core/comps/virtio/src/device/block
```

Expected: no descriptor-full loop remains in read, write, or flush submission.

- [ ] **Step 5: Commit queue-local backpressure**

```bash
git add kernel/core/comps/virtio/src/device/block/queue.rs
git commit -m "perf(virtio-blk): sleep on exhausted descriptors"
```

---

### Task 8: Preserve flush ordering across queues

**Files:**
- Create: `kernel/core/comps/virtio/src/device/block/flush.rs`
- Modify: `kernel/core/comps/virtio/src/device/block/mod.rs`
- Modify: `kernel/core/comps/virtio/src/device/block/device.rs`
- Modify: `kernel/core/comps/virtio/src/device/block/queue.rs`

**Interfaces:**
- Produces: `FlushGate::admit_write() -> WriteAdmission`.
- Produces: `FlushGate::begin_flush()` and `FlushGate::wait_prior_writes()`.
- Produces: RAII completion methods that decrement write counts and release the flush epoch on every error path.

- [ ] **Step 1: Add flush-gate state-machine tests**

Cover these exact sequences:

```text
write A admitted -> flush begins -> flush waits
write A completes -> flush may submit
write B arrives during flush -> write B waits
flush completes -> write B is admitted
```

Also test a failed write, unsupported flush, failed flush, and two concurrent flush requests.

- [ ] **Step 2: Gate write admission in `BlockDevice::enqueue`**

Before routing a write, wait until no flush is active and increment `outstanding_writes`. Roll the
count back if staging enqueue fails. Reads continue through the normal router but may conservatively
wait behind an active flush in the first implementation.

- [ ] **Step 3: Route flush requests to queue 0**

Mark the flush epoch active before enqueueing the flush. Queue 0's submit worker waits for
`outstanding_writes == 0` before constructing and notifying the device flush request.

- [ ] **Step 4: Release write and flush admissions on completion**

Decrement once per original `SubmittedBio`, including I/O-error paths. Clear `flush_active` only
after the device flush BIO completes or after the unsupported-flush fast path completes.

- [ ] **Step 5: Run cross-queue out-of-order tests**

Complete writes on queue 1 before queue 0 and vice versa. Assert the flush is not submitted until
both pre-flush writes complete and no post-flush write enters a staging queue early.

- [ ] **Step 6: Commit flush correctness**

```bash
git add kernel/core/comps/virtio/src/device/block/flush.rs \
        kernel/core/comps/virtio/src/device/block/mod.rs \
        kernel/core/comps/virtio/src/device/block/device.rs \
        kernel/core/comps/virtio/src/device/block/queue.rs
git commit -m "fix(virtio-blk): order flushes across request queues"
```

---

### Task 9: Add queue-level observability

**Files:**
- Create: `kernel/core/comps/virtio/src/device/block/stats.rs`
- Modify: `kernel/core/comps/virtio/src/device/block/mod.rs`
- Modify: `kernel/core/comps/virtio/src/device/block/device.rs`
- Modify: `kernel/core/comps/virtio/src/device/block/queue.rs`

**Interfaces:**
- Produces: `BlockQueueStats` with atomic counters.
- Produces: `BlockQueueStatsSnapshot` and one aggregate report method on `BlockDevice`.

- [ ] **Step 1: Add counter-transition tests**

Assert submit increments current in-flight and conditionally raises peak; completion decrements
current and increments completed; descriptor stalls and IRQ/spurious wakeups update only their own
queue.

- [ ] **Step 2: Instrument queue transitions**

Record submitted/completed requests and BIOs, current/peak in-flight, read/write/flush counts,
descriptor stalls/time, IRQ wakeups, and spurious completion wakeups. Use relaxed atomics for pure
statistics and existing state synchronization for correctness.

- [ ] **Step 3: Add a compact aggregate report**

Print one line per queue plus one device total when explicitly requested by the research harness.
Do not log each request, IRQ, or completion.

- [ ] **Step 4: Verify accounting under error injection**

Inject success and error completions and assert current in-flight returns to zero and total
submitted equals total completed after drain.

- [ ] **Step 5: Commit observability**

```bash
git add kernel/core/comps/virtio/src/device/block/stats.rs \
        kernel/core/comps/virtio/src/device/block/mod.rs \
        kernel/core/comps/virtio/src/device/block/device.rs \
        kernel/core/comps/virtio/src/device/block/queue.rs
git commit -m "feat(virtio-blk): report per-queue activity"
```

---

### Task 10: Wire research QEMU configuration and functional smoke tests

**Files:**
- Modify: `tools/qemu_args.sh`
- Modify: `tools/run-prebuilt-research-qemu.sh`
- Modify: `test/initramfs/src/benchmark/fio/storage_path_ceiling/run.sh`

**Interfaces:**
- Consumes: existing `RESEARCH_BLOCK_NUM_QUEUES` environment variable.
- Produces: reproducible QEMU commands and result metadata for queue counts 1, 2, and 4.

- [ ] **Step 1: Add shell assertions for generated arguments**

Generate commands with queue counts 1, 2, and 4 and assert the `vresearch` device contains the
exact requested `num-queues` while retaining `queue-size=1024`.

- [ ] **Step 2: Keep default behavior conservative**

Leave the general default at `RESEARCH_BLOCK_NUM_QUEUES=1` until the full test matrix passes. Add
an explicit matrix/harness setting for 2 and 4 rather than silently changing unrelated tests.

- [ ] **Step 3: Propagate queue count through prebuilt runs**

Ensure `qemu-command.sh`, `parameters.json`, or their current equivalents record the actual queue
count. Keep the read-only configuration disk at one queue; only the research data disk varies.

- [ ] **Step 4: Boot functional configurations**

For each of 1, 2, and 4 queues, boot with at least the same number of vCPUs, perform raw read/write,
mount EXT4, create/read/remove a file, issue `sync`, and unmount. Expected: no timeout, missing
completion, or flush deadlock.

- [ ] **Step 5: Validate queue activity**

For four queues, run enough non-mergeable writes to assert every queue has nonzero submitted and
completed counters and aggregate current in-flight returns to zero after drain.

- [ ] **Step 6: Commit harness support**

```bash
git add tools/qemu_args.sh tools/run-prebuilt-research-qemu.sh \
        test/initramfs/src/benchmark
git commit -m "test(virtio-blk): exercise multiqueue devices"
```

Stage only the exact harness files changed; do not include unrelated user benchmark edits.

---

### Task 11: Run raw-device performance and concurrency validation

**Files:**
- Modify: `test/initramfs/src/benchmark/fio/storage_path_ceiling/run.sh`
- Create: result directory through the existing research runner, not as a committed source artifact

**Interfaces:**
- Produces: comparable release-build results for `num-queues=1,2,4`.
- Produces: per-queue driver statistics beside fio JSON.

- [ ] **Step 1: Add raw multi-job matrix points**

Use the existing storage ceiling harness with:

```text
rw=randwrite
bs=4K
direct=1
ioengine=sync
iodepth=1
numjobs=1,4,16
time_based=1
runtime=30
ramp_time=5
```

The multi-job cases create concurrent synchronous producers without relying on unsupported guest
asynchronous fio engines.

- [ ] **Step 2: Build once per code revision in release mode**

Use `RELEASE=1` and record the kernel artifact identity. Vary only QEMU `num-queues`; keep queue
size, host block device, cache mode, AIO backend, CPU count, runtime, and fio command constant.

- [ ] **Step 3: Run the 1/2/4-queue matrix**

Use at least four vCPUs. Collect fio throughput, IOPS, average latency and P99 latency together
with per-queue submissions, completions, peak in-flight, stalls, and IRQ wakeups.

- [ ] **Step 4: Apply the functional verdict**

Reject the implementation if any configured queue remains unused, submitted/completed totals
differ after drain, in-flight does not return to zero, or errors/timeouts occur.

- [ ] **Step 5: Apply the performance verdict**

Compare MQ against the same release single-queue binary. Report whether throughput rises, reaches
the host/backend ceiling, or remains unchanged. Do not attribute unchanged performance to the
device until aggregate and per-queue in-flight counters prove the queues were populated.

- [ ] **Step 6: Run the sequential regression point**

Run raw sequential 128 KiB write with one and four queues. Investigate any four-queue regression
greater than 10%, especially reduced merging caused by per-queue staging.

---

### Task 12: Run EXT4 writeback validation and final review

**Files:**
- Verify all modified source and test files.
- Write experiment conclusions under the existing `research1/implementation/testing` result format.

**Interfaces:**
- Consumes: functional and performance evidence from Tasks 1-11.
- Produces: final evidence separating driver MQ effects from EXT4 writeback effects.

- [ ] **Step 1: Run formatting and static checks**

Run the repository formatter, then:

```bash
cargo osdk check --package aster-virtio
cargo osdk clippy --ktests -- --no-deps
```

Expected: no new errors or warnings in affected components.

- [ ] **Step 2: Run focused and full kernel tests**

Run all new exact MQ ktests followed by:

```bash
make ktest
```

Record pre-existing failures separately; no new block, VirtIO, page-cache, or EXT4 failures are
acceptable.

- [ ] **Step 3: Build the release kernel**

Run the repository-supported release build with `RELEASE=1`. Expected: kernel, initramfs, and boot
artifact build successfully.

- [ ] **Step 4: Repeat EXT4 random-write points**

Run buffered/writeback 4 KiB random write with `numjobs=1,4,16` for virtio queue counts 1 and 4.
Collect existing EXT4 pipeline statistics and the new driver per-queue statistics.

- [ ] **Step 5: Separate remaining bottlenecks**

Classify results using measured evidence:

```text
several virtqueues active + high aggregate inflight + low throughput
    => backend/QEMU/device or per-request CPU cost remains

only one virtqueue active
    => routing/worker defect

all virtqueues active but aggregate inflight remains near one
    => upper submit path still serially waits

raw improves but EXT4 does not
    => remaining bottleneck is above virtio-blk
```

- [ ] **Step 6: Review the final diff**

Confirm there is no global virtqueue/request-map lock on normal read/write, no descriptor busy
loop, no per-request logging, and no accidental changes to unrelated QEMU disks or user benchmark
edits.

- [ ] **Step 7: Commit final validation adjustments**

If Step 4 required an intentional harness correction, stage only
`test/initramfs/src/benchmark/fio/storage_path_ceiling/run.sh` and commit it as
`test(virtio-blk): validate multiqueue performance`. If no source correction was required, do not
create an empty validation commit; preserve the generated results outside the source commit.

---

## Optional Follow-Up: IRQ CPU Affinity

Do not fold IRQ affinity into the first implementation unless the Task 11 profile shows CPU 0
IRQ/top-half saturation after completion deferral. If needed, write a separate design covering
`IrqLine` affinity, interrupt-remapping destination IDs, non-remapped MSI-X address encoding, and
queue-to-CPU rebalancing. This is a cross-OSTD change and deserves independent correctness tests.

