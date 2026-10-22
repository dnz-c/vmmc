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

exec gjs -m "$disc/dnd-peer.js" "$out" 2> /tmp/vmmc-peer.log
