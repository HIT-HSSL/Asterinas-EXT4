#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -eu

cmdline_value() {
    key=$1
    for item in $(cat /proc/cmdline); do
        case "$item" in
            "$key="*) printf '%s\n' "${item#*=}"; return 0 ;;
        esac
    done
    return 1
}

config_value() {
    variable=$1
    key=$2
    default=$3
    eval "current=\${$variable:-}"
    if [ -n "$current" ]; then
        printf '%s\n' "$current"
    else
        cmdline_value "$key" || printf '%s\n' "$default"
    fi
}

load_runtime_config() {
    config_device=$(cmdline_value ASTER_BASELINE_CONFIG_DEVICE || printf '/dev/vda\n')
    config_required=$(cmdline_value ASTER_BASELINE_CONFIG_REQUIRED || printf '0\n')
    config_file=/tmp/aster-baseline-config.env

    if [ ! -b "$config_device" ]; then
        [ "$config_required" -eq 0 ] && return 0
        fail runtime-config-device 2
    fi
    dd if="$config_device" bs=4096 count=16 2>/dev/null | tr -d '\000' >"$config_file"
    {
        IFS= read -r marker
        [ "$marker" = ASTER_BASELINE_CONFIG_V1 ] || fail runtime-config-marker 2
        while IFS='=' read -r key value; do
            case "$key" in
                ASTER_BASELINE_*) export "$key=$value" ;;
                '') ;;
                *) fail runtime-config-key 2 ;;
            esac
        done
    } <"$config_file"
}

csv_values() {
    printf '%s' "$1" | tr ',' ' '
}

fail() {
    printf 'ASTER_BASELINE_FAILURE point=%s step=%s status=%s\n' \
        "${POINT_ID:-none}" "$1" "${2:-1}"
    exit "${2:-1}"
}

load_runtime_config

DEVICE=$(config_value ASTER_BASELINE_DEVICE ASTER_BASELINE_DEVICE /dev/vda)
MOUNT_POINT=/mnt/aster-ext4
FIO=/benchmark/bin/fio
MKE2FS=/benchmark/bin/mke2fs
TARGETS=$(config_value ASTER_BASELINE_TARGETS ASTER_BASELINE_TARGETS nojournal,nvpool)
WORKLOADS=$(config_value ASTER_BASELINE_WORKLOADS ASTER_BASELINE_WORKLOADS prealloc-rand-4k,prealloc-seq-128k,append-4k,append-128k)
MODES=$(config_value ASTER_BASELINE_MODES ASTER_BASELINE_MODES buffered,direct,fsync)
NJOBS=$(config_value ASTER_BASELINE_NJOBS ASTER_BASELINE_NJOBS 1,2,4,8)
RUNTIME=$(config_value ASTER_BASELINE_RUNTIME ASTER_BASELINE_RUNTIME 60)
FILE_RANGE=$(config_value ASTER_BASELINE_FILE_RANGE ASTER_BASELINE_FILE_RANGE 4294967296)
WRITEBACK_CAPACITY_MB=$(config_value ASTER_BASELINE_WRITEBACK_CAPACITY_MB ASTER_BASELINE_WRITEBACK_CAPACITY_MB 256)
MOUNTED=0
POINT_ID=none

now_ns() {
    awk '{printf "%.0f\n", $1 * 1000000000}' /proc/uptime
}

cleanup() {
    sync || true
    if [ "$MOUNTED" -eq 1 ]; then
        umount "$MOUNT_POINT" || true
        MOUNTED=0
    fi
}
trap cleanup EXIT INT TERM

format_device() {
    target=$1
    if [ "$target" = jbd2 ]; then
        features='extent,filetype,has_journal,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,^orphan_file,^resize_inode'
    else
        features='extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,^orphan_file,^has_journal,^resize_inode'
    fi
    "$MKE2FS" -t ext4 -F -q -b 4096 -I 256 -E nodiscard \
        -O "$features" "$DEVICE" || fail mkfs "$?"
}

mount_target() {
    target=$1
    case "$target" in
        nojournal) options=delalloc,writeback_capacity_mb=$WRITEBACK_CAPACITY_MB ;;
        jbd2) options=delalloc,writeback_capacity_mb=$WRITEBACK_CAPACITY_MB ;;
        nvpool) options=delalloc,nvpool,writeback_capacity_mb=$WRITEBACK_CAPACITY_MB ;;
        *) fail target 2 ;;
    esac
    mount -t ext4 -o "$options" "$DEVICE" "$MOUNT_POINT" || fail mount "$?"
    MOUNTED=1
}

run_point() {
    target=$1 workload=$2 mode=$3 jobs=$4
    POINT_ID="${target}-${workload}-${mode}-nj${jobs}-r1"
    per_job=$((FILE_RANGE / jobs))
    printf 'ASTER_BASELINE_POINT_BEGIN %s\n' "$POINT_ID"
    cleanup
    format_device "$target"
    mkdir -p "$MOUNT_POINT"
    mount_target "$target"

    if [ "${workload#prealloc-}" != "$workload" ]; then
        fallocate -l "$FILE_RANGE" "$MOUNT_POINT/preallocated.dat" || fail fallocate "$?"
    fi
    sync

    case "$mode" in
        buffered) direct=0; fsync=0; sync_io=0 ;;
        direct) direct=1; fsync=0; sync_io=0 ;;
        fsync) direct=0; fsync=1; sync_io=0 ;;
        periodic-fsync) direct=0; fsync=32; sync_io=0 ;;
        osync) direct=0; fsync=0; sync_io=1 ;;
        *) fail mode 2 ;;
    esac
    set -- \
        --name="$POINT_ID" --ioengine=psync --numjobs="$jobs" --iodepth=1 \
        --thread=1 --group_reporting=1 --runtime="$RUNTIME" --direct="$direct" \
        --fsync="$fsync" --sync="$sync_io" --norandommap=0 --output-format=json
    case "$workload" in
        prealloc-rand-4k)
            set -- "$@" --filename="$MOUNT_POINT/preallocated.dat" --rw=randwrite \
                --bs=4k --size="$per_job" --offset_increment="$per_job" --overwrite=1
            ;;
        prealloc-seq-128k)
            set -- "$@" --filename="$MOUNT_POINT/preallocated.dat" --rw=write \
                --bs=128k --size="$per_job" --offset_increment="$per_job" --overwrite=1
            ;;
        append-4k|append-128k)
            [ "$workload" = append-4k ] && block_size=4k || block_size=128k
            set -- "$@" --directory="$MOUNT_POINT" --filename_format='append.$jobnum' \
                --rw=write --bs="$block_size" --size="$per_job" --create_on_open=1 \
                --fallocate=none --unlink_each_loop=1
            ;;
        *) fail workload 2 ;;
    esac

    printf 'ASTER_BASELINE_FIO_COMMAND %s' "$POINT_ID"
    printf ' %s' "$FIO" "$@"
    printf '\nASTER_BASELINE_FIO_BEGIN %s\n' "$POINT_ID"
    start_ns=$(now_ns)
    set +e
    "$FIO" "$@"
    status=$?
    set -e
    printf 'ASTER_BASELINE_FIO_END %s status=%s\n' "$POINT_ID" "$status"
    [ "$status" -eq 0 ] || fail fio "$status"
    sync || fail post-fio-sync "$?"
    end_ns=$(now_ns)
    elapsed_ns=$((end_ns - start_ns))
    printf 'ASTER_BASELINE_DRAIN %s elapsed_ns=%s\n' "$POINT_ID" "$elapsed_ns"
    umount "$MOUNT_POINT" || fail unmount "$?"
    MOUNTED=0
    printf 'ASTER_BASELINE_POINT_END %s\n' "$POINT_ID"
}

for target in $(csv_values "$TARGETS"); do
    for workload in $(csv_values "$WORKLOADS"); do
        for mode in $(csv_values "$MODES"); do
            for jobs in $(csv_values "$NJOBS"); do
                run_point "$target" "$workload" "$mode" "$jobs"
            done
        done
    done
done

POINT_ID=none
printf 'ASTER_BASELINE_SUITE_END status=0\n'
