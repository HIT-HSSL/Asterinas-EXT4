#!/usr/bin/env bash

set -euo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
BENCHMARK="$REPO_ROOT/test/initramfs/src/benchmark/fio/storage_path_ceiling/run.sh"
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

sed "s|^FIO=/benchmark/bin/fio$|FIO=$TMP_DIR/fio|" "$BENCHMARK" >"$TMP_DIR/run.sh"
printf '#!/bin/sh\nprintf "%%s\\n" "$*"\n' >"$TMP_DIR/fio"
chmod +x "$TMP_DIR/fio" "$TMP_DIR/run.sh"

output=$(
    EXT4_RESEARCH_TARGET=raw \
    EXT4_RESEARCH_DEVICE=/dev/test \
    EXT4_RESEARCH_WORKLOAD=raw-randwrite-4k-4job \
    EXT4_RESEARCH_NUMJOBS=8 \
    EXT4_RESEARCH_RUNTIME=1 \
    EXT4_RESEARCH_RAMP_TIME=0 \
        "$TMP_DIR/run.sh"
)

grep -Fq -- '--numjobs=8' <<<"$output"
