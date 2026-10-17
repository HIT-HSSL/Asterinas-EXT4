# VirtIO Block Queue Depth 1024 Design

## Goal

Remove the current 64-entry VirtIO block queue and EXT4 writeback inflight limits from the
research path so that queue depth is not the first-order explanation for low random-write
throughput.

## Scope

- Allow modern split virtqueues to allocate descriptor, available, and used rings spanning
  multiple pages, with an explicit maximum of 1024 entries.
- Make the VirtIO block driver request up to 1024 queue entries while falling back to a smaller
  device-advertised maximum for existing devices.
- Size block request/response DMA tables and software BIO limits from the negotiated queue size.
- Raise the EXT4 per-mount inflight writeback BIO limit from 64 to 1024.
- Make the dedicated research QEMU block device advertise a queue size of 1024 by default.

## Design

The generic modern split-queue constructor computes page counts from the queue size using the
VirtIO split-ring byte layout. It rejects sizes above 1024 and allocates each ring with enough
contiguous DMA frames. Existing small queues retain their current effective queue size.

The block driver treats 1024 as a requested maximum, queries queue 0's advertised maximum, and
uses the smaller value. That negotiated size is stored in `DeviceInner` and controls the request
DMA table, response DMA table, ID allocator, request-size checks, and software staging limit.
This prevents unrelated QEMU devices that still advertise 64 entries from failing initialization.

The dedicated research disk defaults to `queue-size=1024`. Other test disks remain unchanged and
exercise the fallback behavior.

## Validation

- Kernel tests verify exact modern-ring page counts at 64, 256, and 1024 entries.
- Shell-level argument generation verifies the dedicated research disk advertises 1024 entries.
- The affected Rust crates are formatted and compiled, followed by a kernel build when the local
  Docker environment is available.

## Important Interpretation

With indirect descriptors disabled, one 4 KiB VirtIO block request normally consumes three
descriptors: request header, data, and response. A 1024-entry ring therefore permits roughly 341
simultaneous single-segment requests, not 1024 requests. Raising the EXT4 inflight limit ensures
the filesystem layer does not retain the former 64-BIO ceiling during this experiment.
