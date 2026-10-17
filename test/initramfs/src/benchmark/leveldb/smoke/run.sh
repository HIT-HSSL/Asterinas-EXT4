#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -eu

DB=/ext2/leveldb-smoke
DB_BENCH=/benchmark/bin/db_bench

rm -rf "$DB"

"$DB_BENCH" \
    --db="$DB" \
    --benchmarks=fillseq \
    --num=4096 \
    --value_size=1024

"$DB_BENCH" \
    --db="$DB" \
    --use_existing_db=1 \
    --benchmarks=overwrite \
    --num=4096 \
    --value_size=1024

sync
echo "[leveldb-smoke] complete"
