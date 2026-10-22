#!/bin/sh
# vmmc Tools for Linux, from the vmmc Tools disc: vmmc's agent (vmmc-agent), which shares
# the clipboard with the host and drags files between the host and the guest. Run it as
# root from the disc, which the desktop shows under /media:
#
#     sudo sh /media/$USER/VMMC_TOOLS/install-linux.sh
#
# It installs the agent in /usr/local/bin, lets the desktop's user open the serial port
# the agent talks to the host on (/dev/ttyS1), and starts the agent with each desktop
# session, and now in the session of whoever ran it.

set -e
if [ "$(id -u)" != 0 ]; then
    echo "vmmc Tools: run this as root: sudo sh $0" >&2
    exit 1
fi
here=$(dirname "$0")

install -m 755 "$here/vmmc-agent" /usr/local/bin/vmmc-agent

cat > /etc/udev/rules.d/70-vmmc-agent.rules <<'EOF'
# vmmc's agent talks to the host on the second serial port: whoever uses the desktop
# may open it.
KERNEL=="ttyS1", TAG+="uaccess"
EOF
udevadm control --reload-rules
udevadm trigger --action=change --sysname-match=ttyS1
udevadm settle || true

mkdir -p /etc/xdg/autostart
cat > /etc/xdg/autostart/vmmc-agent.desktop <<'EOF'
[Desktop Entry]
Type=Application
Name=vmmc agent
Comment=Shares the clipboard with the host and drags files to and from it
Exec=/usr/local/bin/vmmc-agent
NoDisplay=true
X-GNOME-Autostart-Phase=Applications
EOF

# Now, in the session of the user who ran this. (Their agent finds the desktop itself.)
user=${SUDO_USER:-}
if [ -n "$user" ] && [ "$user" != root ]; then
    uid=$(id -u "$user")
    pkill -u "$user" -x vmmc-agent 2>/dev/null || true
    sleep 0.3
    sudo -u "$user" env XDG_RUNTIME_DIR="/run/user/$uid" HOME="$(getent passwd "$user" | cut -d: -f6)" \
        setsid /usr/local/bin/vmmc-agent > /dev/null 2>&1 < /dev/null &
fi
echo "vmmc Tools are installed: the clipboard and drag and drop now work with the host."
