# VirtIO Block Multi-Queue Design

## Goal

Add real VirtIO block multi-queue support to Asterinas so one virtio-blk device can negotiate,
initialize, feed, and complete several request virtqueues concurrently. The first milestone makes
the driver submit over multiple hardware queues. The second milestone removes software and
completion-side serialization so the guest can drive an NVMe-backed QEMU block device near its
available IOPS and bandwidth.

## Current Baseline

The current driver has one `BioRequestSingleQueue`, one request worker, one `VirtQueue`, one pair
of request/response DMA areas, one ID allocator, one submitted-request map, and one IRQ callback.
`VIRTIO_BLK_F_MQ` is defined but excluded from `BlockFeatures::ALL_SUPPORTED`. Initialization
warns if the transport exposes multiple queues and then creates only queue 0.

The existing queue-depth work raises queue 0 to at most 1024 descriptors. It must remain intact,
but it does not provide multi-queue support. With indirect descriptors disabled, a one-segment
4 KiB request normally consumes three descriptors, so a 1024-entry ring represents roughly 341
simultaneous requests per queue.

## Scope

- Negotiate `VIRTIO_BLK_F_MQ` when offered.
- Read the block device configuration field `num_queues` only after MQ is negotiated.
- Select an effective queue count bounded by the block config, transport, online CPUs, and a
  driver maximum of 64 queues.
- Construct independent request virtqueues with independent DMA slots and request tracking.
- Route requests across all enabled queues even when they originate from one EXT4 writeback
  thread.
- Provide one submission worker and one deferred completion worker per enabled queue.
- Keep IRQ top halves bounded to waking the corresponding completion worker.
- Replace descriptor-full busy spinning with a queue-local wait/wakeup path.
- Preserve flush ordering across all queues.
- Expose queue-level statistics sufficient to prove that several queues are active concurrently.
- Add QEMU configuration and raw-device performance validation for 1, 2, and 4 queues.

## Non-Goals

- Do not redesign EXT4 writeback in this change.
- Do not add packed virtqueues, indirect descriptors, polling mode, io_uring, or device hot-unplug.
- Do not implement multi-queue NVMe; this work is specifically for virtio-blk.
- Do not require interrupt affinity for the first functional milestone. IRQ affinity is a later
  optimization if deferred completion workers still leave CPU 0 saturated.
- Do not change the default queue count of unrelated rootfs, regression, or xfstests disks.

## Considered Architectures

### A. Per-queue staging and workers with round-robin routing

Each hardware virtqueue owns a software staging queue, submission worker, completion worker, DMA
storage, ID allocator, submitted-request table, and wait queues. `BlockDevice::enqueue` selects a
queue using round robin. This uses every queue even if all requests come from one producer.

This is the selected design. It removes shared queue locks from the data path and matches the
existing kernel-thread ownership model without changing the public `aster_block::BlockDevice`
trait.

### B. Submit directly from `BlockDevice::enqueue`

The caller selects and locks a hardware queue directly. This removes a context switch but lets
filesystem callers block on queue resources, complicates flush gating, and makes IRQ/task-context
lock rules harder to audit. It is not selected for the initial implementation.

### C. Keep one dispatcher and distribute requests to multiple virtqueues

This is the smallest functional change and could prove feature negotiation quickly, but DMA
preparation and descriptor insertion would remain single-threaded. It is useful only as a debug
fallback, not as the target architecture.

## Queue Count Negotiation

The VirtIO block specification defines one request queue unless `VIRTIO_BLK_F_MQ` is negotiated.
After successful feature negotiation, the driver reads `VirtioBlockConfig::num_queues` and chooses:

```text
if MQ is not negotiated:
    effective_queues = 1
else:
    effective_queues = min(
        block_config.num_queues,
        transport.num_queues,
        online_cpu_count,
        DRIVER_MAX_QUEUES,       // 64
    )
```

Zero from any required source is invalid. A queue whose advertised maximum size is zero is not
usable. Each queue chooses `min(1024, transport.max_queue_size(index))`; all enabled queue sizes
must be powers of two under the current split-ring implementation. The block device reports the
minimum `(queue_size - 2)` across enabled queues as `max_nr_segments_per_bio`.

## Component Structure

The block driver is split into focused modules:

```text
device/block/
  mod.rs          feature bits and device configuration accessors
  device.rs       public BlockDevice, initialization, routing, lifecycle
  queue.rs        BlockQueue submission/completion state and workers
  flush.rs        cross-queue flush gate
  stats.rs        per-queue counters and snapshots
```

The main runtime structures are:

```rust
struct BlockDevice {
    device: Arc<DeviceInner>,
    staging_queues: Box<[BioRequestSingleQueue]>,
    next_queue: AtomicUsize,
    flush_gate: Arc<FlushGate>,
    // identity and partition fields unchanged
}

struct DeviceInner {
    config_manager: ConfigManager<VirtioBlockConfig>,
    features: BlockFeatures,
    queues: Box<[Arc<BlockQueue>]>,
    transport: SpinLock<DeviceTransport>,
}

struct BlockQueue {
    index: u16,
    queue_size: u16,
    virtqueue: SpinLock<VirtQueue>,
    request_dma: Arc<DmaStream>,
    response_dma: Arc<DmaStream>,
    id_allocator: SyncIdAlloc,
    submitted: SpinLock<Vec<Option<SubmittedRequest>>>,
    descriptor_wait_queue: WaitQueue,
    completion_wait_queue: WaitQueue,
    stats: BlockQueueStats,
}
```

Every mutable data-path object is queue-local. The transport lock remains device-wide because it
is needed for initialization and configuration events, not normal request submission.

## Request Routing

`BlockDevice::enqueue` uses an atomic round-robin counter for read and write BIOs:

```text
queue_index = next_queue.fetch_add(1, Relaxed) % queue_count
```

Round robin is intentional. CPU-only mapping would send all background writeback requests to one
queue because the current EXT4 mount has one submit worker. The routing decision occurs before
software staging, so each queue's submit worker consumes only work destined for its hardware
queue.

Merging remains local to each `BioRequestSingleQueue`. Random 4 KiB workloads lose no meaningful
merge opportunity; sequential workloads may merge less than the current global FIFO, so the
performance matrix must include sequential regression checks.

## Submission Path

Each queue has one CPU-affine submission worker:

```text
dequeue BioRequest from staging queue
  -> allocate queue-local request slot
  -> prepare request/response DMA
  -> wait until this virtqueue has enough descriptors
  -> add descriptor chain
  -> install SubmittedRequest in queue-local token table
  -> notify device if required
```

The submitted table is indexed directly by virtqueue token rather than using a `BTreeMap`. The
entry is installed while the virtqueue lock is held and before notification. Completion removes
the entry only after popping the used token.

If descriptors are unavailable, the worker sleeps on `descriptor_wait_queue`. Completion wakes
that queue after recycling descriptors. No worker spins while repeatedly taking the queue lock.

## Completion Path

Each queue requests an individual MSI-X vector through the existing indexed transport callback.
If vectors are exhausted, the transport may use its shared-vector fallback; callbacks remain
queue-specific and drain only their associated used ring.

The IRQ callback performs no DMA synchronization or BIO callback execution. It only wakes the
queue's completion worker. The completion worker:

```text
wait until used ring has entries
  -> pop all currently used tokens under the queue lock
  -> release queue lock
  -> synchronize response/data DMA
  -> resolve response status
  -> return CompletedRequest records to BlockDevice
  -> update the device-wide flush gate
  -> complete constituent BIOs
  -> release request slot
  -> wake descriptor waiters
```

Submission and completion workers for queue `i` are pinned to CPU `i % online_cpu_count`. Current
MSI-X delivery may still enter through CPU 0, but the top half is reduced to a wakeup. Queue-local
workers perform the expensive completion work in task context.

`BlockQueue` does not depend on `FlushGate`. It returns queue-local completion records containing
the request and final status. `BlockDevice::handle_completions` updates device-wide write/flush
accounting before consuming the request into its constituent BIO callbacks. This keeps queue state
independent and makes the cross-queue ordering boundary explicit.

## Cross-Queue Flush Ordering

Multi-queue makes FIFO order local to one queue. A flush must not be submitted while earlier
writes remain staged or in flight on another queue.

`FlushGate` serializes flush epochs:

```rust
struct FlushState {
    flush_active: bool,
    outstanding_writes: usize,
}
```

For a write BIO, enqueue waits for `flush_active == false`, increments `outstanding_writes`, then
places the BIO on its selected staging queue. Completion decrements the counter once per original
write BIO.

For a flush BIO, enqueue waits for any prior flush to finish, sets `flush_active = true`, and sends
the flush to queue 0. Queue 0's submission worker waits until `outstanding_writes == 0`, then
submits the device flush. Writes arriving while the flush is active wait before entering a staging
queue. Flush completion clears `flush_active` and wakes blocked writers and later flushes.

This is deliberately conservative. Flush is not the throughput-critical operation in the target
random-write experiment, and correctness is preferred over overlapping post-flush writes.

## Lifecycle and Errors

- Initialization is all-or-nothing. Failure to allocate or configure any selected queue aborts the
  device initialization and lets `DeviceTransport` set `FAILED`.
- A request response error completes every constituent BIO with `BioStatus::IoError`.
- A failed write still decrements `outstanding_writes`, preventing permanent flush deadlock.
- Worker stop flags and wait queues allow orderly teardown even though hot-unplug is out of scope.
- Shared IRQ fallback may wake more than one queue callback; each completion worker checks its own
  ring and safely returns when empty.

## Observability

Each queue records:

- submitted and completed requests;
- submitted and completed BIOs;
- current and peak in-flight requests;
- descriptor-stall count and accumulated stall time;
- IRQ wakeups and spurious completion wakeups;
- read, write, and flush counts.

Initialization logs the offered feature bits, whether MQ was negotiated, block-config queue count,
transport queue count, effective queue count, every queue size, and IRQ registration mode. A
single aggregate report prints all queue counters at benchmark completion; the driver must not log
per request.

## Delivery Milestones

### Milestone 1: Functional driver multi-queue

- MQ is negotiated and `num_queues` is read correctly.
- Queues `0..N-1` are initialized with independent state.
- Round-robin routing causes a single producer to submit through every enabled queue.
- Each queue handles its own completions.
- Single-queue fallback remains functional.

### Milestone 2: Scalable multi-queue path

- Per-queue submit and completion workers run without a global data-path lock.
- Descriptor exhaustion sleeps and wakes instead of spinning.
- Cross-queue flush ordering passes concurrency tests.
- At least two queues show simultaneous in-flight requests during raw 4 KiB random write.
- CPU and queue statistics identify any remaining single-core bottleneck.

### Milestone 3: Performance validation

Run the same release kernel and host backend with `num-queues=1,2,4`, keeping queue size, cache,
AIO backend, fio workload, runtime, and CPU count constant. Test raw-device 4 KiB random writes at
1, 4, and 16 jobs, then repeat the EXT4 writeback points.

Multi-queue is considered functionally successful only when all configured queues submit and
complete I/O. It is considered performance-useful only when it increases achieved IOPS/bandwidth
or reaches the measured host/backend ceiling without increasing P99 latency unboundedly. Merely
showing `num_queues > 1` is not sufficient.

## Testing Strategy

- Pure ktests cover queue-count negotiation, round-robin routing, queue-local token tables,
  descriptor sleep/wakeup, and flush-gate state transitions.
- Driver ktests use a fake queue backend where possible to inject out-of-order completions.
- QEMU boot tests cover MQ absent, MQ offered with 1 queue, and MQ offered with 2/4 queues.
- Raw fio tests establish the block-path ceiling before EXT4 is involved.
- Existing EXT4 and block ktests guard single-queue behavior and flush semantics.
- Release builds are mandatory for performance conclusions.

## Success Criteria

1. The device boots and passes block I/O tests with MQ absent and with 1, 2, and 4 queues.
2. `num-queues=4` creates four request virtqueues and four queue-local state objects.
3. One producer distributes at least 1024 requests across all four queues with count skew at most
   one request before merging effects.
4. No global virtqueue, ID allocator, or submitted-request lock exists in the read/write data path.
5. Descriptor exhaustion produces wait/wakeup activity and no busy-spin loop.
6. A flush cannot be submitted until all writes admitted before it have completed.
7. Raw 4 KiB random-write results include queue activity, aggregate peak in-flight, throughput,
   IOPS, average latency, and P99 latency for 1/2/4 queues.
8. Sequential 128 KiB throughput with four queues does not regress by more than 10% versus one
   queue unless the result is documented and attributed to reduced software merging.

