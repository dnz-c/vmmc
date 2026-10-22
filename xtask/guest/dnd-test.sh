#!/bin/sh
# `cargo xtask ubuntu-dnd-test`, at each logon, from its test disc: runs the disc's agent
# rather than the installed one, makes the files the guest drags out, and starts the peer
# (dnd-peer.js): a GTK 4 window over the screen, a native Wayland program, that takes
# drops, starts drags and uses the clipboard. What happens goes to COM1.

disc=$(dirname "$0")
say() {
    echo "VMMC-DND: $*" > /dev/ttyS0
}

say "logged-on $XDG_SESSION_TYPE"
say "tools: $(for tool in gjs python3 wl-copy xclip; do command -v $tool > /dev/null && printf '%s ' $tool; done)"
say "typelibs: $(ls /usr/lib/x86_64-linux-gnu/girepository-1.0 2> /dev/null | grep -E '^(Gtk|Gdk)-' | tr '\n' ' ')"

# Ubuntu's welcome wizard comes up at each logon until it's gone through, over the
# screen and with the focus: closed, as soon as it shows up.
(
    for _ in $(seq 120); do
        for pid in $(pgrep -u "$(id -u)" -f 'welcome|initial-setup|desktop-provision|desktop-init|provd'); do
            say "closing $(tr '\0' ' ' < /proc/$pid/cmdline | cut -c1-100)"
            kill "$pid"
        done
        sleep 0.5
    done
) &

pkill -x vmmc-agent
sleep 0.5
cp "$disc/vmmc-agent" /tmp/vmmc-agent
chmod 755 /tmp/vmmc-agent
/tmp/vmmc-agent 2> /tmp/vmmc-agent.log &
sleep 0.5
if pgrep -x vmmc-agent > /dev/null; then say "agent=True"; else say "agent=False"; fi

# What the guest drags out: a file, and a folder with a folder in it and an empty one.
out="$HOME/vmmc-from-guest"
rm -rf "$out"
mkdir -p "$out/Ordner/tief" "$out/Ordner/leer"
head -c 3145745 /dev/urandom > "$out/guest file.bin"
printf 'aus dem Gast' > "$out/Ordner/tief/ärger.txt"
(cd "$out" && find . -type f | sort | while read -r file; do
    say "guest-file $(sha256sum "$file" | cut -d' ' -f1) ${file#./}"
done)

# After the drags, the host takes the Tools disc out and puts it in again, as the app's
# Install vmmc Tools does: Ubuntu ejects it when asked, the desktop mounts the new one,
# GIO finds software to run on it (autorun.sh), and the install the app tells of works
# (with sudo, as in a terminal).
(
    # Where the desktop mounted it: /media/$USER/VMMC_TOOLS, or /run/media/$USER/...
    # (Ubuntu 26.04).
    tools_mount() {
        findmnt -nro TARGET -S LABEL=VMMC_TOOLS | head -n 1
    }
    # It's in from the start, mounted for the session.
    for _ in $(seq 60); do [ -n "$(tools_mount)" ] && break; sleep 1; done
    tools=$(tools_mount)
    if [ -n "$tools" ]; then say "tools-disc mounted at $tools"; else say "tools-disc missing"; fi
    while [ -n "$(tools_mount)" ]; do sleep 0.5; done
    say "tools-disc ejected"
    # The drags are over: the peer goes, so the screenshot shows the desktop.
    pkill -f dnd-peer.js
    for _ in $(seq 120); do [ -n "$(tools_mount)" ] && break; sleep 0.5; done
    tools=$(tools_mount)
    [ -n "$tools" ] || { say "tools-disc not-back"; exit; }
    say "tools-disc back at $tools"
    say "tools-autorun $(ls -l "$tools/autorun.sh" 2>&1)"
    sleep 2
    say "tools-content $(gio mount -li 2> /dev/null | grep -A30 "Mount.*VMMC_TOOLS" | grep -m1 x_content_types)"
    say "tools-handler $(gio mime x-content/unix-software 2>&1 | tr '\n' ' ')"
    sleep 8
    # As the app says to, in a terminal.
    printf 'vmmc\n' | sudo -S sh "$(findmnt -nro TARGET -S LABEL=VMMC_TOOLS)/install-linux.sh" 2>&1 |
        while read -r line; do say "tools-install: $line"; done
    sleep 3
    say "tools-installed agent=$(pgrep -a -x vmmc-agent | tr '\n' ' ')"
) &

# Later, for the log: what the peer and the session warned about.
(
    sleep 75
    tail -n 20 /tmp/vmmc-peer.log | while read -r line; do say "peer: $line"; done
    journalctl --user -b -p warning --no-pager -o cat 2> /dev/null | tail -n 15 |
        while read -r line; do say "journal: $line"; done
) &

exec gjs -m "$disc/dnd-peer.js" "$out" 2> /tmp/vmmc-peer.log
