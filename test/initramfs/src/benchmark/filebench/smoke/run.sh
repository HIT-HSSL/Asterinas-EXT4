#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -eu

WORKDIR=/ext2/filebench-smoke

rm -rf "$WORKDIR"
mkdir -p "$WORKDIR"
/benchmark/bin/filebench -f /benchmark/filebench/smoke/smoke.f
sync
echo "[filebench-smoke] complete"
