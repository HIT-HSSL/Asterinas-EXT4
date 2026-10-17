#!/bin/bash

set -euo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

qemu_args=$(
    ATTACH_DEFAULT_TEST_IMAGES=false \
    RESEARCH_BLOCK_BACKEND=/dev/nvme1n1p1 \
    RESEARCH_BLOCK_NUM_QUEUES=4 \
    RESEARCH_BLOCK_IOTHREADS=4 \
        "$REPO_ROOT/tools/qemu_args.sh" normal 2>/dev/null
)

grep -Fq -- "-device '{\"driver\":\"virtio-blk-pci\"" <<<"$qemu_args"

for queue in 0 1 2 3; do
    grep -Fq -- "-object iothread,id=research-io${queue}" <<<"$qemu_args"
    grep -Fq -- "{\"iothread\":\"research-io${queue}\",\"vqs\":[${queue}]}" <<<"$qemu_args"
done

mapfile -d '' -t qemu_argv < <(
    QEMU_ARGS_FORMAT=nul \
    ATTACH_DEFAULT_TEST_IMAGES=false \
    RESEARCH_BLOCK_BACKEND=/dev/nvme1n1p1 \
    RESEARCH_BLOCK_NUM_QUEUES=4 \
    RESEARCH_BLOCK_IOTHREADS=4 \
        "$REPO_ROOT/tools/qemu_args.sh" normal 2>/dev/null
)

json_device_found=false
for arg in "${qemu_argv[@]}"; do
    if [[ "$arg" == '{"driver":"virtio-blk-pci"'* ]]; then
        json_device_found=true
        [[ "$arg" != *"'"* ]]
    fi
done
[[ "$json_device_found" == true ]]

default_args=$(
    ATTACH_DEFAULT_TEST_IMAGES=false \
    RESEARCH_BLOCK_BACKEND=/dev/nvme1n1p1 \
    RESEARCH_BLOCK_NUM_QUEUES=4 \
        "$REPO_ROOT/tools/qemu_args.sh" normal 2>/dev/null
)
[[ "$default_args" != *"research-io"* ]]
[[ "$default_args" != *"iothread-vq-mapping"* ]]

if ATTACH_DEFAULT_TEST_IMAGES=false \
    RESEARCH_BLOCK_BACKEND=/dev/nvme1n1p1 \
    RESEARCH_BLOCK_NUM_QUEUES=4 \
    RESEARCH_BLOCK_IOTHREADS=2 \
        "$REPO_ROOT/tools/qemu_args.sh" normal >/dev/null 2>&1; then
    echo "mismatched queue and IOThread counts were accepted" >&2
    exit 1
fi

if QEMU_ARGS_FORMAT=nul "$REPO_ROOT/tools/qemu_args.sh" tdx >/dev/null 2>&1; then
    echo "NUL output unexpectedly accepted an unsupported scheme" >&2
    exit 1
fi

if ATTACH_DEFAULT_TEST_IMAGES=false \
    RESEARCH_BLOCK_BACKEND=/dev/nvme1n1p1 \
    RESEARCH_BLOCK_NUM_QUEUES=4 \
    RESEARCH_BLOCK_IOTHREADS=4 \
        "$REPO_ROOT/tools/qemu_args.sh" microvm >/dev/null 2>&1; then
    echo "microvm unexpectedly accepted research IOThreads" >&2
    exit 1
fi
