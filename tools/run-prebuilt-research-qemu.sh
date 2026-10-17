#!/usr/bin/env bash

set -euo pipefail

DRY_RUN=0
if [[ "${1:-}" == --dry-run ]]; then
    DRY_RUN=1
    shift
fi
((!$#)) || {
    printf 'Usage: %s [--dry-run]\n' "$0" >&2
    exit 2
}

ISO=${RESEARCH_PREBUILT_ISO:-target/osdk/asterinas/asterinas-osdk-bin.iso}
CONFIG_BACKEND=${RESEARCH_CONFIG_BACKEND:?RESEARCH_CONFIG_BACKEND is required}

[[ -f "$ISO" ]] || {
    printf 'Error: prebuilt Asterinas ISO not found: %s\n' "$ISO" >&2
    exit 2
}
[[ -f "$CONFIG_BACKEND" ]] || {
    printf 'Error: runtime config image not found: %s\n' "$CONFIG_BACKEND" >&2
    exit 2
}

mapfile -d "" -t qemu_args_array < <(
    QEMU_ARGS_FORMAT=nul ./tools/qemu_args.sh normal
)

RESEARCH_QEMU_HEADLESS=${RESEARCH_QEMU_HEADLESS:-false}
case "$RESEARCH_QEMU_HEADLESS" in
    true)
        headless_args=()
        index=0
        while ((index < ${#qemu_args_array[@]})); do
            if [[ "${qemu_args_array[$index]}" == -display ]]; then
                index=$((index + 2))
                continue
            fi
            headless_args+=("${qemu_args_array[$index]}")
            index=$((index + 1))
        done
        qemu_args_array=("${headless_args[@]}" -display none)
        ;;
    false) ;;
    *)
        printf 'Error: RESEARCH_QEMU_HEADLESS must be true or false\n' >&2
        exit 2
        ;;
esac

qemu_command=(
    qemu-system-x86_64
    -drive "file=$ISO,format=raw,index=2,media=cdrom"
    "${qemu_args_array[@]}"
    -accel kvm
    -drive "if=none,format=raw,id=researchcfg,file=$CONFIG_BACKEND,readonly=on"
    -device "virtio-blk-pci,bus=pcie.0,addr=0x1d,drive=researchcfg,serial=vresearchcfg,disable-legacy=on,disable-modern=off,queue-size=64,num-queues=1,request-merging=off,backend_defaults=off,discard=off,write-zeroes=off,event_idx=off,indirect_desc=off,queue_reset=off"
)

if ((DRY_RUN)); then
    printf '%q ' "${qemu_command[@]}"
    printf '\n'
    exit 0
fi

set +e
"${qemu_command[@]}"
status=$?
set -e

# isa-debug-exit encodes the guest status as (status << 1) | 1.
case "$status" in
    0|33) exit 0 ;;
    65) exit 1 ;;
    *) exit "$status" ;;
esac
