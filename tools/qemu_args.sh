#!/bin/bash

# SPDX-License-Identifier: MPL-2.0

# This script is used to generate QEMU arguments for OSDK.
# Usage: `qemu_args.sh [scheme]`
#  - scheme: "normal", "test", "microvm" or "iommu";
# Other arguments are configured via environmental variables:
#  - OVMF: "on" or "off";
#  - BOOT_METHOD: "qemu-direct", "grub-rescue-iso" or "grub-qcow2";
#  - BOOT_PROTOCOL: "multiboot", "multiboot2", "linux-legacy32", "linux-efi-pe64" or "linux-efi-handover64";
#  - NETDEV: "user" or "tap";
#  - VHOST: "off" or "on";
#  - VSOCK: "off" or "on";
#  - VIRTIOFS: "off" or "on";
#  - VIRTIOFS_TAG: mount tag for virtio-fs device;
#  - VIRTIOFS_SOCKET: vhost-user socket path for the virtio-fs server;
#  - INITRAMFS: "on" or "off"; when "off", attach `rootfs.img` as an extra block device.
#  - CONSOLE: "hvc0" to enable virtio console;
#  - SMP: number of CPUs;
#  - MEM: amount of memory, e.g. "8G";
#  - VNC_PORT: VNC port, default is "42";
#  - ATTACH_EXT4_IMAGE: "true" or "false", whether to attach the ext
#    regression image and mount-flavor matrix images.
#  - ATTACH_DEFAULT_TEST_IMAGES: "true" or "false", whether to attach the
#    ext2, exfat, LTP, and emulated NVMe regression images.
#  - XFSTESTS_NEEDS_BLOCK_DEVICES: "true" or "false", whether to attach
#    xfstests images (xfstests_test.img and xfstests_scratch.img) to the VM.
#  - RESEARCH_BLOCK_BACKEND: optional raw image or host block-device path to
#    expose as a dedicated virtio-blk research disk.
#  - RESEARCH_BLOCK_IOTHREADS: number of QEMU IOThreads dedicated to the
#    research disk. Use 0 to keep QEMU's default AioContext.

OVMF=${OVMF:-"on"}
VHOST=${VHOST:-"off"}
VSOCK=${VSOCK:-"off"}
VIRTIOFS=${VIRTIOFS:-"off"}
NETDEV=${NETDEV:-"user"}
CONSOLE=${CONSOLE:-"hvc0"}
ATTACH_EXT4_IMAGE=${ATTACH_EXT4_IMAGE:-false}
ATTACH_DEFAULT_TEST_IMAGES=${ATTACH_DEFAULT_TEST_IMAGES:-true}
XFSTESTS_NEEDS_BLOCK_DEVICES=${XFSTESTS_NEEDS_BLOCK_DEVICES:-false}
RESEARCH_BLOCK_BACKEND=${RESEARCH_BLOCK_BACKEND:-}
RESEARCH_BLOCK_CACHE=${RESEARCH_BLOCK_CACHE:-none}
RESEARCH_BLOCK_AIO=${RESEARCH_BLOCK_AIO:-threads}
RESEARCH_BLOCK_QUEUE_SIZE=${RESEARCH_BLOCK_QUEUE_SIZE:-1024}
RESEARCH_BLOCK_NUM_QUEUES=${RESEARCH_BLOCK_NUM_QUEUES:-1}
RESEARCH_BLOCK_IOTHREADS=${RESEARCH_BLOCK_IOTHREADS:-0}
QEMU_ARGS_FORMAT=${QEMU_ARGS_FORMAT:-shell}

case "$QEMU_ARGS_FORMAT" in
    shell|nul) ;;
    *) echo "Invalid QEMU_ARGS_FORMAT=$QEMU_ARGS_FORMAT" 1>&2; exit 1 ;;
esac
# NUL argv output is an internal interface for the two array-based consumers below.
if [ "$QEMU_ARGS_FORMAT" = nul ]; then
    case "${1:-}" in
        normal|common) ;;
        *) echo "QEMU_ARGS_FORMAT=nul only supports normal and common schemes" 1>&2; exit 1 ;;
    esac
fi

emit_qemu_args() {
    local qemu_args=$1
    if [ "$QEMU_ARGS_FORMAT" = shell ]; then
        # Preserve the historical whitespace-collapsed shell output for OSDK callers.
        # shellcheck disable=SC2086
        echo $qemu_args
        return
    fi

    local -a qemu_argv
    read -r -a qemu_argv <<<"$qemu_args"
    printf '%s\0' "${qemu_argv[@]}"
}

if [ "$ATTACH_EXT4_IMAGE" != "true" ] && [ "$ATTACH_EXT4_IMAGE" != "false" ]; then
    echo "Invalid ATTACH_EXT4_IMAGE=${ATTACH_EXT4_IMAGE}" 1>&2
    exit 1
fi

if [ "$ATTACH_DEFAULT_TEST_IMAGES" != "true" ] && \
   [ "$ATTACH_DEFAULT_TEST_IMAGES" != "false" ]; then
    echo "Invalid ATTACH_DEFAULT_TEST_IMAGES=${ATTACH_DEFAULT_TEST_IMAGES}" 1>&2
    exit 1
fi

if [ "$XFSTESTS_NEEDS_BLOCK_DEVICES" != "true" ] && \
   [ "$XFSTESTS_NEEDS_BLOCK_DEVICES" != "false" ]; then
    echo "Invalid XFSTESTS_NEEDS_BLOCK_DEVICES=${XFSTESTS_NEEDS_BLOCK_DEVICES}" 1>&2
    exit 1
fi
VIRTIOFS_TAG=${VIRTIOFS_TAG:-"aster-virtiofs"}
VIRTIOFS_SOCKET=${VIRTIOFS_SOCKET:-"/tmp/vhostqemu/vfs.sock"}

RESEARCH_BLOCK_DRIVE_ARGS=""
RESEARCH_BLOCK_Q35_DEVICE_ARGS=""
RESEARCH_BLOCK_MICROVM_DEVICE_ARGS=""
RESEARCH_BLOCK_IOTHREAD_ARGS=""
if [ -n "$RESEARCH_BLOCK_BACKEND" ]; then
    [[ "$RESEARCH_BLOCK_BACKEND" =~ ^[-+._/a-zA-Z0-9]+$ ]] || {
        echo "Invalid RESEARCH_BLOCK_BACKEND path" 1>&2
        exit 1
    }
    [[ "$RESEARCH_BLOCK_CACHE" =~ ^(none|directsync|writeback|unsafe)$ ]] || {
        echo "Invalid RESEARCH_BLOCK_CACHE=$RESEARCH_BLOCK_CACHE" 1>&2
        exit 1
    }
    [[ "$RESEARCH_BLOCK_AIO" =~ ^(native|threads|io_uring)$ ]] || {
        echo "Invalid RESEARCH_BLOCK_AIO=$RESEARCH_BLOCK_AIO" 1>&2
        exit 1
    }
    [[ "$RESEARCH_BLOCK_QUEUE_SIZE" =~ ^[1-9][0-9]*$ ]] || {
        echo "Invalid RESEARCH_BLOCK_QUEUE_SIZE=$RESEARCH_BLOCK_QUEUE_SIZE" 1>&2
        exit 1
    }
    [[ "$RESEARCH_BLOCK_NUM_QUEUES" =~ ^[1-9][0-9]*$ ]] || {
        echo "Invalid RESEARCH_BLOCK_NUM_QUEUES=$RESEARCH_BLOCK_NUM_QUEUES" 1>&2
        exit 1
    }
    [[ "$RESEARCH_BLOCK_IOTHREADS" =~ ^[0-9]+$ ]] || {
        echo "Invalid RESEARCH_BLOCK_IOTHREADS=$RESEARCH_BLOCK_IOTHREADS" 1>&2
        exit 1
    }
    if [ "$RESEARCH_BLOCK_IOTHREADS" -ne 0 ] && \
       [ "$RESEARCH_BLOCK_IOTHREADS" -ne "$RESEARCH_BLOCK_NUM_QUEUES" ]; then
        echo "RESEARCH_BLOCK_IOTHREADS must be 0 or match RESEARCH_BLOCK_NUM_QUEUES" 1>&2
        exit 1
    fi

    RESEARCH_BLOCK_IOTHREAD_MAPPING=""
    for ((queue = 0; queue < RESEARCH_BLOCK_IOTHREADS; queue++)); do
        RESEARCH_BLOCK_IOTHREAD_ARGS+=" -object iothread,id=research-io$queue"
        [ -z "$RESEARCH_BLOCK_IOTHREAD_MAPPING" ] || RESEARCH_BLOCK_IOTHREAD_MAPPING+=","
        RESEARCH_BLOCK_IOTHREAD_MAPPING+="{\"iothread\":\"research-io$queue\",\"vqs\":[$queue]}"
    done

    RESEARCH_BLOCK_DRIVE_ARGS="-drive if=none,format=raw,id=research,file=$RESEARCH_BLOCK_BACKEND,cache=$RESEARCH_BLOCK_CACHE,aio=$RESEARCH_BLOCK_AIO"
    if [ -n "$RESEARCH_BLOCK_IOTHREAD_MAPPING" ]; then
        RESEARCH_BLOCK_Q35_DEVICE_JSON="{\"driver\":\"virtio-blk-pci\",\"bus\":\"pcie.0\",\"addr\":\"0x1e\",\"drive\":\"research\",\"serial\":\"vresearch\",\"disable-legacy\":\"on\",\"disable-modern\":false,\"queue-size\":$RESEARCH_BLOCK_QUEUE_SIZE,\"num-queues\":$RESEARCH_BLOCK_NUM_QUEUES,\"request-merging\":false,\"backend_defaults\":\"off\",\"discard\":false,\"write-zeroes\":false,\"event_idx\":false,\"indirect_desc\":false,\"queue_reset\":false,\"iothread-vq-mapping\":[$RESEARCH_BLOCK_IOTHREAD_MAPPING]}"
        if [ "$QEMU_ARGS_FORMAT" = shell ]; then
            RESEARCH_BLOCK_Q35_DEVICE_ARGS="-device '$RESEARCH_BLOCK_Q35_DEVICE_JSON'"
        else
            RESEARCH_BLOCK_Q35_DEVICE_ARGS="-device $RESEARCH_BLOCK_Q35_DEVICE_JSON"
        fi
    else
        RESEARCH_BLOCK_Q35_DEVICE_ARGS="-device virtio-blk-pci,bus=pcie.0,addr=0x1e,drive=research,serial=vresearch,disable-legacy=on,disable-modern=off,queue-size=$RESEARCH_BLOCK_QUEUE_SIZE,num-queues=$RESEARCH_BLOCK_NUM_QUEUES,request-merging=off,backend_defaults=off,discard=off,write-zeroes=off,event_idx=off,indirect_desc=off,queue_reset=off"
    fi
    RESEARCH_BLOCK_MICROVM_DEVICE_ARGS="-device virtio-blk-device,drive=research,serial=vresearch"
fi

if [ "$1" = microvm ] && [ "$RESEARCH_BLOCK_IOTHREADS" -ne 0 ]; then
    echo "RESEARCH_BLOCK_IOTHREADS is only supported by the Q35 research device" 1>&2
    exit 1
fi

DEFAULT_TEST_DRIVE_ARGS=""
DEFAULT_TEST_Q35_DEVICE_ARGS=""
DEFAULT_TEST_MICROVM_DEVICE_ARGS=""
DEFAULT_TEST_RISCV_DEVICE_ARGS=""
DEFAULT_TEST_NVME_ARGS=""
if [ "$ATTACH_DEFAULT_TEST_IMAGES" = "true" ]; then
    DEFAULT_TEST_DRIVE_ARGS="\
        -drive if=none,format=raw,id=x0,file=./test/initramfs/build/ext2.img \
        -drive if=none,format=raw,id=x1,file=./test/initramfs/build/exfat.img \
        -drive if=none,format=raw,id=x2,file=./test/initramfs/build/ltp_dev.img"
    DEFAULT_TEST_Q35_DEVICE_ARGS="\
        -device virtio-blk-pci,bus=pcie.0,addr=0x6,drive=x0,serial=vext2,disable-legacy=on,disable-modern=off,queue-size=64,num-queues=1,request-merging=off,backend_defaults=off,discard=off,write-zeroes=off,event_idx=off,indirect_desc=off,queue_reset=off \
        -device virtio-blk-pci,bus=pcie.0,addr=0x7,drive=x1,serial=vexfat,disable-legacy=on,disable-modern=off,queue-size=64,num-queues=1,request-merging=off,backend_defaults=off,discard=off,write-zeroes=off,event_idx=off,indirect_desc=off,queue_reset=off \
        -device virtio-blk-pci,bus=pcie.0,addr=0x8,drive=x2,serial=vltpdev,disable-legacy=on,disable-modern=off,queue-size=64,num-queues=1,request-merging=off,backend_defaults=off,discard=off,write-zeroes=off,event_idx=off,indirect_desc=off,queue_reset=off"
    DEFAULT_TEST_MICROVM_DEVICE_ARGS="\
        -device virtio-blk-device,drive=x0,serial=vext2 \
        -device virtio-blk-device,drive=x1,serial=vexfat \
        -device virtio-blk-device,drive=x2,serial=vltpdev"
    DEFAULT_TEST_RISCV_DEVICE_ARGS="\
        -device virtio-blk-device,drive=x2 \
        -device virtio-blk-device,drive=x1 \
        -device virtio-blk-device,drive=x0"
    DEFAULT_TEST_NVME_ARGS="\
        -drive if=none,format=raw,id=nvme0n1,file=./test/initramfs/build/nvme0n1.img \
        -device nvme,drive=nvme0n1,serial=nvme0n1"
fi

# Draw all host ports from a single `shuf` invocation,
# so that none of them will conflict with others.
mapfile -t RAND_PORTS < <(shuf -i 1024-65535 -n 7)
SSH_RAND_PORT=${SSH_PORT:-${RAND_PORTS[0]}}
NGINX_RAND_PORT=${NGINX_PORT:-${RAND_PORTS[1]}}
REDIS_RAND_PORT=${REDIS_PORT:-${RAND_PORTS[2]}}
IPERF_RAND_PORT=${IPERF_PORT:-${RAND_PORTS[3]}}
LMBENCH_TCP_LAT_RAND_PORT=${LMBENCH_TCP_LAT_PORT:-${RAND_PORTS[4]}}
LMBENCH_TCP_BW_RAND_PORT=${LMBENCH_TCP_BW_PORT:-${RAND_PORTS[5]}}
MEMCACHED_RAND_PORT=${MEMCACHED_PORT:-${RAND_PORTS[6]}}

# Optional QEMU arguments. Opt in them manually if needed.
# QEMU_OPT_ARG_DUMP_PACKETS="-object filter-dump,id=filter0,netdev=net01,file=virtio-net.pcap"

if [ "$NETDEV" = "user" ]; then
    echo "[$1] Forwarded QEMU guest port: $SSH_RAND_PORT->22; $NGINX_RAND_PORT->8080 $REDIS_RAND_PORT->6379 $IPERF_RAND_PORT->5201 $LMBENCH_TCP_LAT_RAND_PORT->31234 $LMBENCH_TCP_BW_RAND_PORT->31236 $MEMCACHED_RAND_PORT->11211" 1>&2
    NETDEV_ARGS="-netdev user,id=net01,hostfwd=tcp::$SSH_RAND_PORT-:22,hostfwd=tcp::$NGINX_RAND_PORT-:8080,hostfwd=tcp::$REDIS_RAND_PORT-:6379,hostfwd=tcp::$IPERF_RAND_PORT-:5201,hostfwd=tcp::$LMBENCH_TCP_LAT_RAND_PORT-:31234,hostfwd=tcp::$LMBENCH_TCP_BW_RAND_PORT-:31236,hostfwd=tcp::$MEMCACHED_RAND_PORT-:11211"
elif [ "$NETDEV" = "tap" ]; then
    THIS_SCRIPT_DIR=$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )
    QEMU_IFUP_SCRIPT_PATH=$THIS_SCRIPT_DIR/net/qemu-ifup.sh
    QEMU_IFDOWN_SCRIPT_PATH=$THIS_SCRIPT_DIR/net/qemu-ifdown.sh
    NETDEV_ARGS="-netdev tap,id=net01,script=$QEMU_IFUP_SCRIPT_PATH,downscript=$QEMU_IFDOWN_SCRIPT_PATH,vhost=$VHOST"
else
    echo "Invalid netdev" 1>&2
    NETDEV_ARGS="-nic none"
fi

VIRTIO_NET_FEATURES=",csum=off,ctrl_guest_offloads=off,ctrl_mac_addr=off,ctrl_rx_extra=off,ctrl_rx=off,ctrl_vlan=off,ctrl_vq=off,event_idx=off,guest_announce=off,guest_csum=off,guest_ecn=off,guest_tso4=off,guest_tso6=off,guest_ufo=off,guest_uso4=off,guest_uso6=off,host_ecn=off,host_tso4=off,host_tso6=off,host_ufo=off,host_uso=off,indirect_desc=off,mrg_rxbuf=off,queue_reset=off"

if [ "$CONSOLE" = "hvc0" ]; then
    # Kernel logs are printed to all consoles. Redirect serial output to a file to avoid duplicate logs.
    CONSOLE_ARGS="-device virtconsole,chardev=mux -serial file:qemu-serial.log"
else
    CONSOLE_ARGS="-serial chardev:mux"
fi

if [ "$INITRAMFS" = "off" ]; then
    ROOTFS_DRIVE_ARGS="-drive if=none,format=raw,id=rootfs,file=./test/initramfs/build/rootfs.img"
fi

if [ "$1" = "riscv" ]; then
    # NOTE: The initramfs assumes that ext2.img, exfat.img, and ltp_dev.img appear as
    # `/dev/vda`, `/dev/vdb`, and `/dev/vdc`, respectively. RISC-V virtio-mmio
    # block devices are discovered in reverse command-line order, so list them
    # in the reverse of the desired device-node order.
    # When booting without initramfs, `rootfs.img` is placed before these devices
    # so that it is discovered after them as `/dev/vdd`.
    # TODO: Once UUID-based mounting is implemented, this strict ordering will no longer be required.
    if [ "$INITRAMFS" = "off" ]; then
        ROOTFS_RISCV_DEVICE_ARGS="-device virtio-blk-device,drive=rootfs"
    fi

    QEMU_ARGS="\
        -cpu rv64,svpbmt=true,zkr=true \
        -machine virt \
        -m ${MEM:-8G} \
        -smp ${SMP:-1} \
        --no-reboot \
        -nographic \
        -display none \
        -monitor chardev:mux \
        -chardev stdio,id=mux,mux=on,signal=off,logfile=qemu.log \
        $ROOTFS_DRIVE_ARGS \
        $DEFAULT_TEST_DRIVE_ARGS \
        $ROOTFS_RISCV_DEVICE_ARGS \
        $DEFAULT_TEST_RISCV_DEVICE_ARGS \
        -device virtio-keyboard-device \
        -device virtio-serial-device \
        $CONSOLE_ARGS \
    "
    emit_qemu_args "$QEMU_ARGS"
    exit 0
fi

if [ "$1" = "tdx" ]; then
    TDX_OBJECT='{ "qom-type": "tdx-guest", "id": "tdx0", "sept-ve-disable": true, "quote-generation-socket": { "type": "vsock", "cid": "1", "port": "4050" } }'
    if [ "$INITRAMFS" = "off" ]; then
        ROOTFS_TDX_DEVICE_ARGS="-device virtio-blk-pci,bus=pcie.0,addr=0xb,drive=rootfs,serial=vrootfs,disable-legacy=on,disable-modern=off,queue-size=64,num-queues=1,request-merging=off,backend_defaults=off,discard=off,write-zeroes=off,event_idx=off,indirect_desc=off,queue_reset=off"
    fi

    QEMU_ARGS="\
        -m ${MEM:-8G} \
        -smp ${SMP:-1} \
        -vga none \
        -nographic \
        -monitor pty \
        -nodefaults \
        -bios /root/ovmf/release/OVMF.fd \
        -cpu host,-kvm-steal-time,pmu=off \
        -machine q35,kernel-irqchip=split,confidential-guest-support=tdx0 \
        -object '$TDX_OBJECT' \
        $DEFAULT_TEST_DRIVE_ARGS \
        $ROOTFS_DRIVE_ARGS \
        $DEFAULT_TEST_Q35_DEVICE_ARGS \
        $ROOTFS_TDX_DEVICE_ARGS \
        -device virtio-net-pci,netdev=net01,disable-legacy=on,disable-modern=off$VIRTIO_NET_FEATURES \
        -device virtio-keyboard-pci,disable-legacy=on,disable-modern=off \
        $NETDEV_ARGS \
        $QEMU_OPT_ARG_DUMP_PACKETS \
        -chardev stdio,id=mux,mux=on,logfile=qemu.log \
        -device virtio-serial,romfile= \
        $CONSOLE_ARGS \
        -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
        -monitor chardev:mux \
        -d guest_errors \
    "
    emit_qemu_args "$QEMU_ARGS"
    exit 0
fi

COMMON_QEMU_ARGS="\
    -cpu Icelake-Server,+x2apic \
    -smp ${SMP:-1} \
    -m ${MEM:-8G} \
    --no-reboot \
    -nographic \
    -display vnc=0.0.0.0:${VNC_PORT:-42} \
    -monitor chardev:mux \
    -chardev stdio,id=mux,mux=on,signal=off,logfile=qemu.log \
    $NETDEV_ARGS \
    $QEMU_OPT_ARG_DUMP_PACKETS \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    $DEFAULT_TEST_DRIVE_ARGS \
    $RESEARCH_BLOCK_DRIVE_ARGS \
    $RESEARCH_BLOCK_IOTHREAD_ARGS \
    $ROOTFS_DRIVE_ARGS \
"

if [ "$ATTACH_EXT4_IMAGE" = "true" ]; then
    COMMON_QEMU_ARGS="$COMMON_QEMU_ARGS \
        -drive if=none,format=raw,id=x4,file=./test/initramfs/build/ext4.img \
    "
    MATRIX_IDX=0
    for image in ext2 ext2_i128 ext2_i128_nori ext4_journal ext4_noextents neg_nofiletype neg_rev0 neg_metadata_csum; do
        COMMON_QEMU_ARGS="$COMMON_QEMU_ARGS \
            -drive if=none,format=raw,id=mx$MATRIX_IDX,file=./test/initramfs/build/ext_matrix/$image.img \
        "
        MATRIX_IDX=$((MATRIX_IDX + 1))
    done
fi

# Add xfstests drives when the selected file system needs block devices.
if [ "$XFSTESTS_NEEDS_BLOCK_DEVICES" = "true" ]; then
    COMMON_QEMU_ARGS="$COMMON_QEMU_ARGS \
    -drive if=none,format=raw,id=x3,file=./test/initramfs/build/xfstests_test.img \
    -drive if=none,format=raw,id=x4,file=./test/initramfs/build/xfstests_scratch.img \
"
fi

if [ "$1" = "iommu" ]; then
    if [ "$OVMF" = "off" ]; then
        echo "Warning: OVMF is off, enabling it for IOMMU support." 1>&2
        OVMF="on"
    fi
    IOMMU_DEV_EXTRA=",iommu_platform=on,ats=on"
    IOMMU_EXTRA_ARGS="\
        -device intel-iommu,intremap=on,device-iotlb=on \
        -device ioh3420,id=pcie.0,chassis=1 \
    "
    # TODO: Add support for enabling IOMMU on AMD platforms
fi

if [ "$ATTACH_DEFAULT_TEST_IMAGES" = "true" ] && [ -n "$IOMMU_DEV_EXTRA" ]; then
    DEFAULT_TEST_Q35_DEVICE_ARGS=${DEFAULT_TEST_Q35_DEVICE_ARGS//queue_reset=off/queue_reset=off$IOMMU_DEV_EXTRA}
fi

if [ "$INITRAMFS" = "off" ]; then
    ROOTFS_MICROVM_DEVICE_ARGS="-device virtio-blk-device,drive=rootfs,serial=vrootfs"
    ROOTFS_Q35_DEVICE_ARGS="-device virtio-blk-pci,bus=pcie.0,addr=0xc,drive=rootfs,serial=vrootfs,disable-legacy=on,disable-modern=off,queue-size=64,num-queues=1,request-merging=off,backend_defaults=off,discard=off,write-zeroes=off,event_idx=off,indirect_desc=off,queue_reset=off$IOMMU_DEV_EXTRA"
fi

if [ "$1" = "microvm" ]; then
    QEMU_ARGS="\
        $COMMON_QEMU_ARGS \
        -machine microvm,rtc=on \
        -nodefaults \
        -no-user-config \
        $DEFAULT_TEST_MICROVM_DEVICE_ARGS \
        $RESEARCH_BLOCK_MICROVM_DEVICE_ARGS \
        $ROOTFS_MICROVM_DEVICE_ARGS \
        -device virtio-keyboard-device \
        -device virtio-net-device,netdev=net01 \
        -device virtio-serial-device \
        $CONSOLE_ARGS \
    "
else
    QEMU_ARGS="\
        $COMMON_QEMU_ARGS \
        -machine q35,kernel-irqchip=split \
        $DEFAULT_TEST_Q35_DEVICE_ARGS \
        $RESEARCH_BLOCK_Q35_DEVICE_ARGS \
        $ROOTFS_Q35_DEVICE_ARGS \
        -object rng-random,id=rng0,filename=/dev/urandom \
        -device virtio-rng-pci,bus=pcie.0,addr=0x9,disable-legacy=on,disable-modern=off,rng=rng0,event_idx=off,indirect_desc=off,queue_reset=off$IOMMU_DEV_EXTRA \
        -device virtio-net-pci,netdev=net01,disable-legacy=on,disable-modern=off$VIRTIO_NET_FEATURES$IOMMU_DEV_EXTRA \
        -device virtio-serial-pci,disable-legacy=on,disable-modern=off$IOMMU_DEV_EXTRA \
        $DEFAULT_TEST_NVME_ARGS \
        $CONSOLE_ARGS \
        $IOMMU_EXTRA_ARGS \
    "
fi

if [ "$ATTACH_EXT4_IMAGE" = "true" ]; then
    if [ "$1" = "microvm" ]; then
        QEMU_ARGS="$QEMU_ARGS \
            -device virtio-blk-device,drive=x4,serial=vext4 \
        "
    else
        QEMU_ARGS="$QEMU_ARGS \
            -device virtio-blk-pci,bus=pcie.0,addr=0xb,drive=x4,serial=vext4,disable-legacy=on,disable-modern=off,queue-size=64,num-queues=1,request-merging=off,backend_defaults=off,discard=off,write-zeroes=off,event_idx=off,indirect_desc=off,queue_reset=off$IOMMU_DEV_EXTRA \
        "
    fi

    # Attach after the established regression disk, preserving vda..vdd.
    MATRIX_ADDR=12
    MATRIX_IDX=0
    for image in ext2 ext2_i128 ext2_i128_nori ext4_journal ext4_noextents neg_nofiletype neg_rev0 neg_metadata_csum; do
        if [ "$1" = "microvm" ]; then
            QEMU_ARGS="$QEMU_ARGS \
                -device virtio-blk-device,drive=mx$MATRIX_IDX,serial=mx_$image \
            "
        else
            QEMU_ARGS="$QEMU_ARGS \
                -device virtio-blk-pci,bus=pcie.0,addr=$(printf 0x%x $MATRIX_ADDR),drive=mx$MATRIX_IDX,serial=mx_$image,disable-legacy=on,disable-modern=off,queue-size=64,num-queues=1,request-merging=off,backend_defaults=off,discard=off,write-zeroes=off,event_idx=off,indirect_desc=off,queue_reset=off$IOMMU_DEV_EXTRA \
            "
        fi
        MATRIX_ADDR=$((MATRIX_ADDR + 1))
        MATRIX_IDX=$((MATRIX_IDX + 1))
    done
fi

# Add xfstests devices when the selected file system needs block devices.
if [ "$XFSTESTS_NEEDS_BLOCK_DEVICES" = "true" ]; then
    if [ "$1" = "microvm" ]; then
        QEMU_ARGS="$QEMU_ARGS \
        -device virtio-blk-device,drive=x3,serial=vxfstest \
        -device virtio-blk-device,drive=x4,serial=vxfsscratch \
    "
    else
        QEMU_ARGS="$QEMU_ARGS \
        -device virtio-blk-pci,bus=pcie.0,addr=0xa,drive=x3,serial=vxfstest,disable-legacy=on,disable-modern=off,queue-size=64,num-queues=1,request-merging=off,backend_defaults=off,discard=off,write-zeroes=off,event_idx=off,indirect_desc=off,queue_reset=off$IOMMU_DEV_EXTRA \
        -device virtio-blk-pci,bus=pcie.0,addr=0xb,drive=x4,serial=vxfsscratch,disable-legacy=on,disable-modern=off,queue-size=64,num-queues=1,request-merging=off,backend_defaults=off,discard=off,write-zeroes=off,event_idx=off,indirect_desc=off,queue_reset=off$IOMMU_DEV_EXTRA \
    "
    fi
fi

if [ "$VIRTIOFS" = "on" ]; then
    echo "[$1] Enabled virtio-fs: tag=$VIRTIOFS_TAG, socket=$VIRTIOFS_SOCKET" 1>&2
    QEMU_ARGS="
        $QEMU_ARGS \
        -object memory-backend-memfd,id=mem0,size=${MEM:-8G},share=on \
        -numa node,memdev=mem0 \
        -chardev socket,id=char0,path=$VIRTIOFS_SOCKET \
        -device vhost-user-fs-pci,chardev=char0,tag=$VIRTIOFS_TAG \
    "
fi

if [ "$VSOCK" = "on" ]; then
    # RAND_CID=$(shuf -i 3-65535 -n 1)
    RAND_CID=3
    echo "[$1] Launched QEMU VM with CID $RAND_CID" 1>&2
    if [ "$1" = "microvm" ]; then
        QEMU_ARGS="$QEMU_ARGS \
            -device vhost-vsock-device,guest-cid=$RAND_CID \
        "
    else
        QEMU_ARGS="$QEMU_ARGS \
            -device vhost-vsock-pci,id=vhost-vsock-pci0,guest-cid=$RAND_CID,disable-legacy=on,disable-modern=off$IOMMU_DEV_EXTRA \
        "
    fi
fi

# When using qemu-direct boot, OVMF depends on the boot protocol:
# linux-efi-* protocols require OVMF; other protocols (e.g. multiboot) do not.
if [ "$BOOT_METHOD" = "qemu-direct" ]; then
    if [ "$BOOT_PROTOCOL" = "linux-efi-pe64" ] || [ "$BOOT_PROTOCOL" = "linux-efi-handover64" ]; then
        OVMF="on"
    else
        OVMF="off"
    fi
fi

# When using `grub-rescue-iso` or `grub-qcow2` boot, OVMF must be enabled.
# Currently, the project's `grub-mkrescue` (in container image) only contained
# `x86_64-efi` platform modules — no `i386-pc`. This meant the generated ISO/qcow2
# could only be loaded by OVMF.
if [ "$BOOT_METHOD" = "grub-rescue-iso" ] || [ "$BOOT_METHOD" = "grub-qcow2" ]; then
    OVMF="on"
fi

if [ "$OVMF" = "on" ]; then
    if [ "$1" = "microvm" ]; then
        QEMU_ARGS="${QEMU_ARGS} \
            -bios /root/ovmf/release/microvm/MICROVM.fd \
        "
    else
        QEMU_ARGS="${QEMU_ARGS} \
            -bios /root/ovmf/release/OVMF.fd \
        "
    fi
fi

emit_qemu_args "$QEMU_ARGS"
