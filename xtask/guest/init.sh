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

# The scratch disk: virtio-blk when booted directly, the second NVMe namespace when
# booted through UEFI from NVMe (the first holds the EFI system partition), or a SATA
# disk when booted from CD. Drivers probe in the background, so give the disk a few
# seconds to appear.
disk=
for _ in $(seq 50); do
    for candidate in /dev/vda /dev/nvme0n2 /dev/sda; do
        [ -b "$candidate" ] && disk=$candidate && break 2
    done
    sleep 0.1
done

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
    echo "VMMC-TEST: disk=ok ${disk#/dev/}"
else
    echo "VMMC-TEST: disk=FAILED ${disk#/dev/}"
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
