#!/bin/busybox sh
# PID 1 of the vmmc test initramfs. Prints VMMC-TEST markers to the serial console
# for `cargo xtask boot-test`, then powers off. With `vmmc.interactive` on the kernel
# command line it drops to a shell instead.

/bin/busybox --install -s /bin
export PATH=/bin

mount -t devtmpfs dev /dev
mount -t proc proc /proc
mount -t sysfs sys /sys

# Drivers this kernel has as modules (see xtask/src/modules.rs).
if [ -f /lib/modules/order ]; then
    for module in $(cat /lib/modules/order); do
        insmod "/lib/modules/$module" 2>/dev/null
    done
fi

echo "VMMC-TEST: booted $(uname -r)"
echo "VMMC-TEST: cpus=$(nproc)"
if [ -d /sys/firmware/efi ]; then
    echo "VMMC-TEST: efi=yes"
else
    echo "VMMC-TEST: efi=no"
fi
echo "VMMC-TEST: dmi=$(cat /sys/class/dmi/id/product_name 2>/dev/null)"

# The scratch disk: virtio-blk when booted directly, NVMe namespace 2 when booted
# through UEFI from NVMe (namespace 1 holds the EFI system partition), or a SATA disk
# when booted from CD. Linux names namespaces in the order it finds them, which varies
# (nvme0n2 may be namespace 1), so namespace 2 is found by its ID. Drivers probe in the
# background, so give the disk a few seconds to appear.
nvme_namespace_2() {
    for namespace in /sys/block/nvme0n*; do
        [ "$(cat "$namespace/nsid" 2>/dev/null)" = 2 ] && echo "/dev/${namespace##*/}"
    done
}
disk=
for _ in $(seq 50); do
    for candidate in /dev/vda $(nvme_namespace_2) /dev/sda; do
        [ -b "$candidate" ] && disk=$candidate && break 2
    done
    sleep 0.1
done
case $disk in
/dev/nvme*) disk_name="nvme namespace 2" ;;
*) disk_name=${disk#/dev/} ;;
esac

# Booted from CD (vmmc.cd): the disc must also be readable as ISO 9660 with Joliet.
if grep -q vmmc.cd /proc/cmdline; then
    for _ in $(seq 50); do
        [ -b /dev/sr0 ] && break
        sleep 0.1
    done
    line=
    if mount -t iso9660 -o ro /dev/sr0 /mnt; then
        read -r line </mnt/vmmc.txt
        umount /mnt
    fi
    if [ "$line" = "vmmc cd" ]; then
        echo "VMMC-TEST: cd=ok"
    else
        echo "VMMC-TEST: cd=FAILED"
    fi
fi

# Format the disk, write a file, remount, and read it back.
disk_round_trip() {
    for fs in ext2 vfat; do
        command -v "mkfs.$fs" >/dev/null || continue
        "mkfs.$fs" "$disk" >/dev/null 2>&1 || continue
        mount -t "$fs" "$disk" /mnt || continue
        echo vmmc >/mnt/probe
        sync
        umount /mnt || return 1
        mount -t "$fs" "$disk" /mnt || return 1
        read -r line </mnt/probe
        umount /mnt
        [ "$line" = vmmc ] && return 0
    done
    return 1
}

if [ -z "$disk" ]; then
    echo "VMMC-TEST: disk=missing"
elif disk_round_trip; then
    echo "VMMC-TEST: disk=ok $disk_name"
else
    echo "VMMC-TEST: disk=FAILED $disk_name"
fi

# USB (vmmc.usb): the xHCI's tablet and keyboard, by their pid.codes IDs.
if grep -q vmmc.usb /proc/cmdline; then
    found=
    for _ in $(seq 50); do
        found=
        for device in /sys/bus/usb/devices/*; do
            [ "$(cat "$device/idVendor" 2>/dev/null)" = 1209 ] || continue
            case "$(cat "$device/idProduct")" in
                0001) found="$found tablet" ;;
                0002) found="$found keyboard" ;;
            esac
        done
        [ "$found" = " tablet keyboard" ] || [ "$found" = " keyboard tablet" ] && break
        sleep 0.1
    done
    case "$found" in
        *tablet*keyboard* | *keyboard*tablet*) echo "VMMC-TEST: usb=tablet,keyboard" ;;
        *) echo "VMMC-TEST: usb=FAILED$found" ;;
    esac
fi

# The PS/2 keyboard (vmmc.keyboard): xtask types a line on it, which reaches the first
# virtual console through the kernel's i8042 and keyboard drivers.
if grep -q vmmc.keyboard /proc/cmdline; then
    line=
    echo "VMMC-TEST: keyboard-ready"
    read -r -t 20 line </dev/tty1
    echo "VMMC-TEST: keyboard=$line"
fi

# The TPM (vmmc.tpm): a TPM 2.0, with PCR 0 extended by the firmware's measurements,
# and the physical presence interface version the ACPI _DSM reports.
if grep -q vmmc.tpm /proc/cmdline; then
    for _ in $(seq 50); do
        [ -e /sys/class/tpm/tpm0 ] && break
        sleep 0.1
    done
    version=$(cat /sys/class/tpm/tpm0/tpm_version_major 2>/dev/null)
    pcr0=$(cat /sys/class/tpm/tpm0/pcr-sha256/0 2>/dev/null)
    ppi=$(cat /sys/class/tpm/tpm0/ppi/version 2>/dev/null)
    if [ "$version" = 2 ] && [ -n "$pcr0" ] && echo "$pcr0" | grep -q '[1-9A-Fa-f]'; then
        echo "VMMC-TEST: tpm=2 pcr0=measured ppi=$ppi"
    else
        echo "VMMC-TEST: tpm=FAILED version=$version pcr0=$pcr0 ppi=$ppi"
    fi
fi

# Networking (vmmc.http=PORT): DHCP from the NAT, then an HTTP fetch from the server
# xtask runs on the host, through the gateway, and a DNS lookup.
http=$(sed -n 's/.*vmmc\.http=\([0-9]*\).*/\1/p' /proc/cmdline)
if [ -n "$http" ]; then
    mkdir -p /etc
    cat >/etc/udhcpc.sh <<'EOF'
#!/bin/sh
case "$1" in
    bound|renew)
        ip addr flush dev "$interface"
        ip addr add "$ip/${mask:-24}" dev "$interface"
        [ -n "$router" ] && ip route add default via "$router"
        : >/etc/resolv.conf
        for server in $dns; do echo "nameserver $server" >>/etc/resolv.conf; done
        ;;
esac
EOF
    chmod +x /etc/udhcpc.sh
    ip link set lo up
    ip link set eth0 up
    page=
    if udhcpc -i eth0 -n -q -t 5 -s /etc/udhcpc.sh >/dev/null 2>&1; then
        page=$(wget -q -T 10 -O - "http://10.0.2.2:$http/vmmc")
    fi
    if [ "$page" = "vmmc net ok" ]; then
        echo "VMMC-TEST: net=ok"
    else
        echo "VMMC-TEST: net=FAILED"
    fi
    if nslookup localhost >/dev/null 2>&1; then
        echo "VMMC-TEST: dns=ok"
    else
        echo "VMMC-TEST: dns=FAILED"
    fi
fi

if grep -q vmmc.interactive /proc/cmdline; then
    echo "vmmc test shell; 'poweroff -f' to power off, Ctrl-A x to quit"
    exec setsid cttyhack sh
fi

echo "VMMC-TEST: done"
poweroff -f
