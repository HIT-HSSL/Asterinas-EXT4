# VirtIO Block Queue Depth 1024 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Raise the research VirtIO block and EXT4 writeback queue limits to 1024 without breaking devices that advertise smaller queues.

**Architecture:** Generalize modern split-ring DMA allocation to multiple pages, then let the block driver negotiate `min(1024, device_max)` and derive all per-request storage from that value. Set the dedicated research QEMU device's default queue size and raise the mount-local EXT4 inflight BIO limit.

**Tech Stack:** Rust `no_std`, Asterinas OSTD DMA APIs, VirtIO split queues, Bash QEMU argument generation, kernel tests.

**Spec:** `docs/superpowers/specs/2026-09-20-virtio-block-queue-depth.md`

## Global Constraints

- Keep `kernel/` entirely safe Rust.
- Preserve existing user modifications in benchmark scripts and `tools/run-prebuilt-research-qemu.sh`.
- Cap the supported split queue size at exactly 1024 entries.
- Keep non-research QEMU device queue settings unchanged.

---

### Task 1: Multi-page modern VirtQueue allocation

**Files:**
- Modify: `kernel/core/comps/virtio/src/queue.rs`

**Interfaces:**
- Produces: a pure modern split-ring layout calculation used by `VirtQueue::new`.
- Produces: modern queues with up to 1024 entries backed by sufficiently sized DMA regions.

- [x] **Step 1: Write failing kernel tests**

Add table-driven `#[ktest]` coverage with hand-derived expected frame counts:

```rust
assert_eq!(modern_queue_frame_counts(64), (1, 1, 1));
assert_eq!(modern_queue_frame_counts(256), (1, 1, 1));
assert_eq!(modern_queue_frame_counts(1024), (4, 1, 3));
```

- [x] **Step 2: Run the smallest compile/test command and confirm the helper is missing**

Run the crate's OSDK check or targeted kernel test build. Expected: failure because
`modern_queue_frame_counts` does not exist.

- [x] **Step 3: Implement the layout calculation and multi-page allocation**

Calculate descriptor bytes as `16 * size`, available-ring bytes as `2 * (3 + size)`, and
used-ring bytes as `6 + 8 * size`; round each independently to `PAGE_SIZE`. Reject queue sizes
above 1024.

- [ ] **Step 4: Re-run targeted verification**

Expected: the queue layout tests compile and pass.

Validation note: the test compiles under `cfg(ktest)`, but the focused OSDK QEMU invocation did
not reach the test runner in the existing container. GRUB boot stopped during firmware startup;
the attempted direct boot entered PXE because no bootable device was selected.

### Task 2: Negotiate and propagate the block queue size

**Files:**
- Modify: `kernel/core/comps/virtio/src/device/block/device.rs`

**Interfaces:**
- Consumes: modern VirtQueue support for 1024 entries.
- Produces: `DeviceInner::queue_size: u16`, equal to `min(1024, device queue 0 maximum)`.

- [x] **Step 1: Add behavior coverage through compile-time and queue-layout consumers**

Make all block-driver capacities derive from one negotiated `queue_size`; stale uses of the
requested constant must fail type checking or review because the actual queue may remain 64.

- [x] **Step 2: Implement negotiated sizing**

Query the device maximum, choose the smaller of it and 1024, allocate request and response DMA
streams using `bytes.div_ceil(PAGE_SIZE)`, initialize the ID allocator with the negotiated size,
and use the stored size for request checks and software staging.

- [x] **Step 3: Compile the affected crate**

Run the project-supported check for `aster-virtio`. Expected: success with no unsafe code or
out-of-range slice construction.

### Task 3: Remove the remaining research-path depth limits

**Files:**
- Modify: `kernel/core/src/fs/fs_impls/ext4/writeback.rs`
- Modify: `tools/qemu_args.sh`

**Interfaces:**
- Produces: EXT4 completion capacity of 1024 inflight BIOs.
- Produces: a dedicated research QEMU disk with `queue-size=1024` unless overridden.

- [x] **Step 1: Raise the EXT4 inflight BIO limit**

Set `MAX_INFLIGHT_WRITEBACK_BIOS` to 1024 while retaining the existing page limit.

- [x] **Step 2: Raise only the research QEMU default**

Set `RESEARCH_BLOCK_QUEUE_SIZE=${RESEARCH_BLOCK_QUEUE_SIZE:-1024}`. Leave default regression,
rootfs, filesystem-matrix, and xfstests devices at 64.

- [x] **Step 3: Verify generated QEMU arguments**

Run `tools/qemu_args.sh normal` with a safe temporary research image path and assert that the
`vresearch` device contains `queue-size=1024`; also verify an explicit override remains honored.

### Task 4: Integrated verification

**Files:**
- Verify all modified production and test files.

**Interfaces:**
- Consumes: Tasks 1-3.
- Produces: build and test evidence for the completed change.

- [x] **Step 1: Format changed Rust files**

Run the repository formatter or targeted `rustfmt` through the pinned toolchain.

- [x] **Step 2: Build the kernel in the project Docker environment**

Run `make kernel`. Expected: successful kernel and initramfs build.

- [x] **Step 3: Review the final diff**

Confirm only the planned files changed, user modifications remain intact, and all capacities use
the negotiated rather than requested block queue size.
