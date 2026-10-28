#!/bin/sh
# `cargo xtask ubuntu-gpu-test`, at each logon, from its test disc. vmmc Tools as the
# Tools disc has them: installed if they aren't (then the guest restarts, as the next
# logon is what starts the agent), and their agent must have started with the session.
# What happens goes to COM1.

say() {
    echo "VMMC-GPU: $*" > /dev/ttyS0
}

say "logged-on $XDG_SESSION_TYPE"
# The desktop mounts the Tools disc for the session.
for _ in $(seq 60); do
    tools=$(findmnt -nro TARGET -S LABEL=VMMC_TOOLS | head -n 1)
    [ -n "$tools" ] && break
    sleep 1
done
if [ -z "$tools" ]; then
    say "tools-disc missing"
    exit 1
fi
installed="$HOME/.vmmc-test-installed-from"
if ! cmp -s "$tools/vmmc-agent" /usr/local/bin/vmmc-agent \
    || ! cmp -s "$tools/install-linux.sh" "$installed"; then
    if printf 'vmmc\n' | sudo -S sh "$tools/install-linux.sh" > /dev/null 2>&1; then
        cp "$tools/install-linux.sh" "$installed"
        say "tools-updated"
        printf 'vmmc\n' | sudo -S systemctl reboot
    else
        say "tools-install failed"
    fi
    exit
fi

# The session's autostart programs start around now.
for _ in $(seq 30); do
    pgrep -x vmmc-agent > /dev/null && break
    sleep 1
done
if pgrep -x vmmc-agent > /dev/null; then
    say "installed-agent running"
else
    say "installed-agent not running"
    journalctl --user -b --no-pager -o cat 2> /dev/null | grep -i vmmc-agent | tail -n 5 | while read -r line; do
        say "journal $line"
    done
fi
