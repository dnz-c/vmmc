#!/bin/busybox sh
# PID 1 of the vmmc test initramfs. Prints VMMC-TEST markers to the serial console
# for `cargo xtask boot-test`, then powers off. With `vmmc.interactive` on the kernel
# command line it drops to a shell instead.

/bin/busybox --install -s /bin
export PATH=/bin

mount -t devtmpfs dev /dev
mount -t proc proc /proc
mount -t sysfs sys /sys

echo "VMMC-TEST: booted $(uname -r)"
echo "VMMC-TEST: cpus=$(nproc)"

# Format the first virtio disk, write a file, remount, and read it back.
disk_round_trip() {
    for fs in ext2 vfat; do
        command -v "mkfs.$fs" >/dev/null || continue
        "mkfs.$fs" /dev/vda >/dev/null 2>&1 || continue
        mount -t "$fs" /dev/vda /mnt || continue
        echo vmmc >/mnt/probe
        sync
        umount /mnt || return 1
        mount -t "$fs" /dev/vda /mnt || return 1
        read -r line </mnt/probe
        umount /mnt
        [ "$line" = vmmc ] && return 0
    done
    return 1
}

if [ ! -b /dev/vda ]; then
    echo "VMMC-TEST: disk=missing"
elif disk_round_trip; then
    echo "VMMC-TEST: disk=ok"
else
    echo "VMMC-TEST: disk=FAILED"
fi

if grep -q vmmc.interactive /proc/cmdline; then
    echo "vmmc test shell; 'poweroff -f' to power off, Ctrl-A x to quit"
    exec setsid cttyhack sh
fi

echo "VMMC-TEST: done"
poweroff -f
