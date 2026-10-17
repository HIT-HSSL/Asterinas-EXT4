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

TARGET=$(config_value EXT4_RESEARCH_TARGET EXT4_RESEARCH_TARGET tmpfs)
DEVICE=$(config_value EXT4_RESEARCH_DEVICE EXT4_RESEARCH_DEVICE /dev/vdd)
WORKLOAD=$(config_value EXT4_RESEARCH_WORKLOAD EXT4_RESEARCH_WORKLOAD tmpfs-seq-128k)
SIZE=$(config_value EXT4_RESEARCH_SIZE EXT4_RESEARCH_SIZE 512M)
RUNTIME=$(config_value EXT4_RESEARCH_RUNTIME EXT4_RESEARCH_RUNTIME 10)
RAMP_TIME=$(config_value EXT4_RESEARCH_RAMP_TIME EXT4_RESEARCH_RAMP_TIME 2)
TIME_BASED=$(config_value EXT4_RESEARCH_TIME_BASED EXT4_RESEARCH_TIME_BASED 1)
BS=$(config_value EXT4_RESEARCH_BS EXT4_RESEARCH_BS '')
NUMJOBS=$(config_value EXT4_RESEARCH_NUMJOBS EXT4_RESEARCH_NUMJOBS '')
IODEPTH=$(config_value EXT4_RESEARCH_IODEPTH EXT4_RESEARCH_IODEPTH 1)
FSYNC_INTERVAL=$(config_value EXT4_RESEARCH_FSYNC EXT4_RESEARCH_FSYNC '')
SYNC_MODE=$(config_value EXT4_RESEARCH_SYNC_MODE EXT4_RESEARCH_SYNC_MODE fsync)
DIRECT=$(config_value EXT4_RESEARCH_DIRECT EXT4_RESEARCH_DIRECT 1)
MOUNT_POINT=/storage-ceiling-tmpfs
TMPFS_FILE=$MOUNT_POINT/ceiling.dat
FIO=/benchmark/bin/fio
MOUNTED=0

cleanup() {
    if [ "$MOUNTED" -eq 1 ]; then
        umount "$MOUNT_POINT"
    fi
}
trap cleanup EXIT

run_fio() {
    printf '[storage-ceiling] fio_command'
    for arg in "$FIO" "$@"; do
        printf ' %s' "$arg"
    done
    printf '\n'
    "$FIO" "$@"
}

prepare_raw_read() {
    echo '[storage-ceiling] precondition_begin name=raw-seq-read-128k'
    run_fio --name=prepare-raw-read --filename="$DEVICE" --size="$SIZE" \
        --ioengine=sync --direct=1 --iodepth=1 --rw=write --bs=1M --numjobs=1
    echo '[storage-ceiling] precondition_end name=raw-seq-read-128k'
}

echo "[storage-ceiling] config target=$TARGET workload=$WORKLOAD device=$DEVICE size=$SIZE runtime=$RUNTIME ramp_time=$RAMP_TIME time_based=$TIME_BASED bs=${BS:-default} numjobs=${NUMJOBS:-default} iodepth=$IODEPTH fsync=${FSYNC_INTERVAL:-default} sync_mode=$SYNC_MODE direct=$DIRECT"
echo "[storage-ceiling] workload_begin name=$WORKLOAD"

case "$WORKLOAD" in
    tmpfs-seq-128k|tmpfs-randwrite-4k)
        [ "$TARGET" = tmpfs ] || { echo 'tmpfs workload requires tmpfs target' >&2; exit 2; }
        mkdir -p "$MOUNT_POINT"
        mount -t tmpfs tmpfs "$MOUNT_POINT"
        MOUNTED=1
        if [ "$WORKLOAD" = tmpfs-seq-128k ]; then
            set -- --name="$WORKLOAD" --filename="$TMPFS_FILE" --size="$SIZE" \
                --ioengine=sync --direct=0 --iodepth="$IODEPTH" --rw=write \
                --bs="${BS:-128K}" --numjobs="${NUMJOBS:-1}" --time_based="$TIME_BASED" \
                --runtime="$RUNTIME" --ramp_time="$RAMP_TIME"
            interval=${FSYNC_INTERVAL:-0}
            if [ "$interval" -gt 0 ]; then
                set -- "$@" "--${SYNC_MODE}=$interval"
            fi
            run_fio "$@"
        else
            run_fio --name=prepare-tmpfs-random --filename="$TMPFS_FILE" --size="$SIZE" \
                --ioengine=sync --direct=0 --iodepth=1 --rw=write --bs=1M --numjobs=1
            run_fio --name="$WORKLOAD" --filename="$TMPFS_FILE" --size="$SIZE" \
                --ioengine=sync --direct=0 --iodepth=1 --rw=randwrite --bs=4K \
                --numjobs=1 --time_based=1 --runtime="$RUNTIME" --ramp_time="$RAMP_TIME"
        fi
        ;;
    raw-seq-write-128k)
        [ "$TARGET" = raw ] || { echo 'raw workload requires raw target' >&2; exit 2; }
        set -- --name="$WORKLOAD" --filename="$DEVICE" --size="$SIZE" \
            --ioengine=sync --direct="$DIRECT" --iodepth="$IODEPTH" --rw=write \
            --bs="${BS:-128K}" --numjobs="${NUMJOBS:-1}" --time_based="$TIME_BASED" \
            --runtime="$RUNTIME" --ramp_time="$RAMP_TIME"
        interval=${FSYNC_INTERVAL:-0}
        if [ "$interval" -gt 0 ]; then
            set -- "$@" "--${SYNC_MODE}=$interval"
        fi
        run_fio "$@"
        ;;
    raw-seq-read-128k)
        [ "$TARGET" = raw ] || { echo 'raw workload requires raw target' >&2; exit 2; }
        prepare_raw_read
        run_fio --name="$WORKLOAD" --filename="$DEVICE" --size="$SIZE" \
            --ioengine=sync --direct=1 --iodepth=1 --rw=read --bs=128K \
            --numjobs=1 --time_based=1 --runtime="$RUNTIME" --ramp_time="$RAMP_TIME"
        ;;
    raw-randwrite-4k-1job)
        [ "$TARGET" = raw ] || { echo 'raw workload requires raw target' >&2; exit 2; }
        run_fio --name="$WORKLOAD" --filename="$DEVICE" --size="$SIZE" \
            --ioengine=sync --direct=1 --iodepth=1 --rw=randwrite --bs=4K \
            --numjobs="${NUMJOBS:-1}" --time_based=1 --runtime="$RUNTIME" --ramp_time="$RAMP_TIME"
        ;;
    raw-randwrite-4k-4job)
        [ "$TARGET" = raw ] || { echo 'raw workload requires raw target' >&2; exit 2; }
        run_fio --name="$WORKLOAD" --filename="$DEVICE" --size=1G \
            --offset_increment=1G --ioengine=sync --direct=1 --iodepth=1 \
            --rw=randwrite --bs=4K --numjobs="${NUMJOBS:-4}" --time_based=1 \
            --runtime="$RUNTIME" --ramp_time="$RAMP_TIME"
        ;;
    *)
        echo "Unknown storage ceiling workload: $WORKLOAD" >&2
        exit 2
        ;;
esac

echo "[storage-ceiling] workload_end name=$WORKLOAD"
