#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -eu

cmdline_value() {
    key=$1
    for item in $(cat /proc/cmdline); do
        case "$item" in
            "$key="*)
                printf '%s\n' "${item#*=}"
                return 0
                ;;
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
    CONFIG_DEVICE=${EXT4_RESEARCH_CONFIG_DEVICE:-/dev/vdd}
    CONFIG_FILE=/tmp/ext4-research-config.env
    CONFIG_REQUIRED=$(cmdline_value EXT4_RESEARCH_CONFIG_REQUIRED || printf '0\n')

    if [ ! -b "$CONFIG_DEVICE" ]; then
        [ "$CONFIG_REQUIRED" -eq 0 ] && return 0
        echo "[ext4-research] required runtime config device is missing: $CONFIG_DEVICE" >&2
        exit 2
    fi

    dd if="$CONFIG_DEVICE" bs=4096 count=16 2>/dev/null | tr -d '\000' >"$CONFIG_FILE"
    {
        IFS= read -r marker
        if [ "$marker" != EXT4_RESEARCH_CONFIG_V1 ]; then
            echo "[ext4-research] invalid runtime config marker" >&2
            exit 2
        fi
        while IFS='=' read -r key value; do
            case "$key" in
                EXT4_RESEARCH_*) export "$key=$value" ;;
                '') ;;
                *)
                    echo "[ext4-research] invalid runtime config key: $key" >&2
                    exit 2
                    ;;
            esac
        done
    } <"$CONFIG_FILE"
}

load_runtime_config

DEVICE=$(config_value EXT4_RESEARCH_DEVICE EXT4_RESEARCH_DEVICE /dev/vdd)
MOUNT_POINT=$(config_value EXT4_RESEARCH_MOUNT_POINT EXT4_RESEARCH_MOUNT_POINT /ext4-research)
WORKLOAD=$(config_value EXT4_RESEARCH_WORKLOAD EXT4_RESEARCH_WORKLOAD all)
SIZE=$(config_value EXT4_RESEARCH_SIZE EXT4_RESEARCH_SIZE 256M)
RUNTIME=$(config_value EXT4_RESEARCH_RUNTIME EXT4_RESEARCH_RUNTIME 30)
RAMP_TIME=$(config_value EXT4_RESEARCH_RAMP_TIME EXT4_RESEARCH_RAMP_TIME 5)
TIME_BASED=$(config_value EXT4_RESEARCH_TIME_BASED EXT4_RESEARCH_TIME_BASED 1)
BS=$(config_value EXT4_RESEARCH_BS EXT4_RESEARCH_BS '')
NUMJOBS=$(config_value EXT4_RESEARCH_NUMJOBS EXT4_RESEARCH_NUMJOBS '')
IODEPTH=$(config_value EXT4_RESEARCH_IODEPTH EXT4_RESEARCH_IODEPTH 1)
FSYNC_INTERVAL=$(config_value EXT4_RESEARCH_FSYNC EXT4_RESEARCH_FSYNC '')
SYNC_MODE=$(config_value EXT4_RESEARCH_SYNC_MODE EXT4_RESEARCH_SYNC_MODE fsync)
DIRECT=$(config_value EXT4_RESEARCH_DIRECT EXT4_RESEARCH_DIRECT 0)
MOUNT_OPTIONS=$(config_value EXT4_RESEARCH_MOUNT_OPTIONS EXT4_RESEARCH_MOUNT_OPTIONS delalloc)
FIO=/benchmark/bin/fio
MOUNTED_HERE=0

mkdir -p "$MOUNT_POINT"
if ! awk -v mount_point="$MOUNT_POINT" '$2 == mount_point { found = 1 } END { exit !found }' /proc/mounts; then
    mount -t ext4 -o "$MOUNT_OPTIONS" "$DEVICE" "$MOUNT_POINT"
    MOUNTED_HERE=1
fi

cleanup() {
    sync
    if [ "$MOUNTED_HERE" -eq 1 ]; then
        umount "$MOUNT_POINT"
    fi
}
trap cleanup EXIT

run_workload() {
    name=$1
    shift
    echo "[ext4-research] workload_begin name=$name"
    set -- \
        --name="$name" \
        --directory="$MOUNT_POINT" \
        --size="$SIZE" \
        --ioengine=sync \
        --direct="$DIRECT" \
        --iodepth="$IODEPTH" \
        --group_reporting=1 \
        --ramp_time="$RAMP_TIME" \
        --runtime="$RUNTIME" \
        "$@"
    if [ "$TIME_BASED" -eq 1 ]; then
        set -- "$@" --time_based=1
    fi
    echo "[ext4-research] fio_command $FIO $*"
    "$FIO" "$@"
    sync
    echo "[ext4-research] workload_end name=$name"
}

prepare_overwrite_files() {
    name=$1
    jobs=$2
    echo "[ext4-research] precondition_begin name=$name"
    set -- \
        --name="prepare-$name" \
        --directory="$MOUNT_POINT" \
        --size="$SIZE" \
        --ioengine=sync \
        --direct="$DIRECT" \
        --iodepth="$IODEPTH" \
        --rw=write \
        --bs=128k \
        --numjobs="$jobs" \
        --nrfiles=1 \
        --filename_format='writeback.$jobnum.$filenum' \
        --fallocate=none \
        --fsync_on_close=1
    echo "[ext4-research] fio_command $FIO $*"
    "$FIO" "$@"
    sync
    echo "[ext4-research] precondition_end name=$name"
}

prepare_small_overwrite_files() {
    name=$1
    echo "[ext4-research] precondition_begin name=$name"
    set -- \
        --name="prepare-$name" \
        --directory="$MOUNT_POINT" \
        --size=256M \
        --filesize=256K \
        --ioengine=sync \
        --direct="$DIRECT" \
        --iodepth="$IODEPTH" \
        --rw=write \
        --bs=128k \
        --numjobs=1 \
        --nrfiles=1024 \
        --filename_format='small-writeback.$filenum' \
        --fallocate=none \
        --fsync_on_close=1
    echo "[ext4-research] fio_command $FIO $*"
    "$FIO" "$@"
    sync
    echo "[ext4-research] precondition_end name=$name"
}

run_random_4k() {
    prepare_overwrite_files random-4k 1
    set -- --rw=randwrite --bs="${BS:-4K}" --numjobs="${NUMJOBS:-1}" \
        --nrfiles=1 --filename_format='writeback.$jobnum.$filenum' \
        --overwrite=1 --fallocate=none
    if [ -n "$FSYNC_INTERVAL" ]; then
        interval=$FSYNC_INTERVAL
    else
        interval=32
    fi
    if [ "$interval" -gt 0 ]; then
        set -- "$@" "--${SYNC_MODE}=$interval"
    fi
    run_workload random-4k "$@"
}

run_sequential_128k() {
    set -- --rw=write --bs="${BS:-128K}" --numjobs="${NUMJOBS:-1}"
    if [ -n "$FSYNC_INTERVAL" ]; then
        if [ "$FSYNC_INTERVAL" -gt 0 ]; then
            set -- "$@" "--${SYNC_MODE}=$FSYNC_INTERVAL"
        fi
    else
        set -- "$@" --fsync_on_close=1
    fi
    run_workload sequential-128k "$@"
}

run_multi_file_random_4k() {
    jobs=${NUMJOBS:-4}
    prepare_overwrite_files multi-file-random-4k "$jobs"
    set -- --rw=randwrite --bs="${BS:-4K}" --numjobs="$jobs" \
        --nrfiles=1 --filename_format='writeback.$jobnum.$filenum' \
        --overwrite=1 --fallocate=none
    if [ -n "$FSYNC_INTERVAL" ]; then
        interval=$FSYNC_INTERVAL
    else
        interval=32
    fi
    if [ "$interval" -gt 0 ]; then
        set -- "$@" "--${SYNC_MODE}=$interval"
    fi
    run_workload multi-file-random-4k "$@"
}

run_small_files_random_4k() {
    prepare_small_overwrite_files small-files-random-4k
    set -- --rw=randwrite --bs="${BS:-4K}" --numjobs=1 \
        --nrfiles=1024 --filesize=256K \
        --filename_format='small-writeback.$filenum' \
        --file_service_type=roundrobin:1 --overwrite=1 --fallocate=none
    if [ -n "$FSYNC_INTERVAL" ]; then
        interval=$FSYNC_INTERVAL
    else
        interval=1
    fi
    if [ "$interval" -gt 0 ]; then
        set -- "$@" "--${SYNC_MODE}=$interval"
    fi
    run_workload small-files-random-4k "$@"
}

prepare_baseline_file() {
    jobs=${NUMJOBS:-1}
    echo "[ext4-research] precondition_begin name=$WORKLOAD"
    set -- \
        --name="prepare-$WORKLOAD" \
        --directory="$MOUNT_POINT" \
        --size="$SIZE" \
        --ioengine=sync \
        --direct=0 \
        --rw=write \
        --bs=1M \
        --numjobs="$jobs" \
        --group_reporting=1 \
        --nrfiles=1 \
        --filename_format='baseline-prealloc.$jobnum' \
        --fallocate=none \
        --fsync_on_close=1
    echo "[ext4-research] fio_command $FIO $*"
    "$FIO" "$@"
    sync
    echo "[ext4-research] precondition_end name=$WORKLOAD"
}

baseline_sync_option() {
    if [ -n "$FSYNC_INTERVAL" ] && [ "$FSYNC_INTERVAL" -gt 0 ]; then
        printf '%s\n' "--${SYNC_MODE}=$FSYNC_INTERVAL"
    fi
}

run_baseline_prealloc() {
    rw=$1
    bs=$2
    jobs=${NUMJOBS:-1}
    prepare_baseline_file
    set -- --rw="$rw" --bs="$bs" --numjobs="$jobs" --nrfiles=1 \
        --filename_format='baseline-prealloc.$jobnum' --overwrite=1 --fallocate=none
    sync_option=$(baseline_sync_option)
    [ -z "$sync_option" ] || set -- "$@" "$sync_option"
    run_workload "$WORKLOAD" "$@"
}

run_baseline_append() {
    bs=$1
    jobs=${NUMJOBS:-1}
    set -- --rw=write --bs="$bs" --numjobs="$jobs" --nrfiles=1 \
        --filename_format='baseline-append.$jobnum' --fallocate=none
    sync_option=$(baseline_sync_option)
    [ -z "$sync_option" ] || set -- "$@" "$sync_option"
    run_workload "$WORKLOAD" "$@"
}

echo "[ext4-research] fio_config workload=$WORKLOAD device=$DEVICE mount=$MOUNT_POINT mount_options=$MOUNT_OPTIONS size=$SIZE runtime=$RUNTIME ramp_time=$RAMP_TIME time_based=$TIME_BASED bs=${BS:-default} numjobs=${NUMJOBS:-default} iodepth=$IODEPTH fsync=${FSYNC_INTERVAL:-default} sync_mode=$SYNC_MODE direct=$DIRECT"

case "$WORKLOAD" in
    random-4k)
        run_random_4k
        ;;
    sequential-128k)
        run_sequential_128k
        ;;
    multi-file-random-4k)
        run_multi_file_random_4k
        ;;
    small-files-random-4k)
        run_small_files_random_4k
        ;;
    baseline-prealloc-rand4k)
        run_baseline_prealloc randwrite 4K
        ;;
    baseline-prealloc-seq128k)
        run_baseline_prealloc write 128K
        ;;
    baseline-append-4k)
        run_baseline_append 4K
        ;;
    baseline-append-128k)
        run_baseline_append 128K
        ;;
    all)
        run_random_4k
        run_sequential_128k
        run_multi_file_random_4k
        ;;
    *)
        echo "Unknown EXT4 research workload: $WORKLOAD" >&2
        exit 2
        ;;
esac
