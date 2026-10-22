# vmmc's Linux guest tools

**The agent** (`agent`, `vmmc-agent`) does in a Linux guest what `vmmcagent.exe` does in
Windows: it shares the clipboard's text with the host, and drags files between the host
and the guest. It talks to the host over the VM's second serial port (`/dev/ttyS1`), in
the protocol the `agent_proto` crate describes, and a drag's files go over TCP to the
VM's NAT, which vmmc answers itself.

It reaches the desktop through X11: XWayland under a Wayland session (GNOME's included)
bridges the clipboard and drags to Wayland programs.

- **The clipboard:** XFixes says when the CLIPBOARD selection changes hands, and its text
  goes to the host; the host's text makes the agent the selection's owner.
- **Drags from the host:** the agent stages the items in `~/.cache/vmmc`, and an
  invisible window over the screen takes the worker's button press; the agent is then an
  XDND source of their `text/uri-list`.
- **Drags out to the host:** another invisible window over the screen says it takes
  drops (`XdndAware`), catches the guest's drag when the host's pointer leaves the
  console, reads its `text/uri-list` and refuses the drop.

## Building

```powershell
cargo xtask guest-drivers
```

It's a Rust program of its own (not in vmmc's workspace), built static for
`x86_64-unknown-linux-musl` and linked with `rust-lld`, which Rust brings along: no Linux
toolchain is needed, and it runs on any distribution. xtask puts it on the vmmc Tools
disc with `tools/install-linux.sh`.

## Installing

**Install vmmc Tools** in vmmc's toolbar puts the vmmc Tools disc in the running VM's
DVD drive, and the desktop mounts it (`/media/$USER/VMMC_TOOLS`, or
`/run/media/$USER/VMMC_TOOLS` on Ubuntu 26.04). Then, in a terminal:

```sh
sudo sh "$(findmnt -nro TARGET -S LABEL=VMMC_TOOLS)/install-linux.sh"
```

A desktop that runs software from inserted media offers to run the disc's
`autorun.sh` instead, which runs the script through `pkexec` (asking for the user's
password). GNOME has that off by default since GNOME 45 (Ubuntu 24.04 and later); it's
**Removable Media** in Settings.

If the guest has the old disc mounted, it's asked to let it go first, as a drive's eject
button does (vmmc's drive reports an eject request); Ubuntu unmounts and ejects it, and
the new disc goes in.

The script installs the agent in `/usr/local/bin`, lets whoever uses the desktop open
`/dev/ttyS1` (a udev rule), starts the agent with each desktop session
(`/etc/xdg/autostart`), and starts it now for the user who ran it.

The disc's Rock Ridge entries keep the scripts' names and make them runnable.

`cargo xtask ubuntu-dnd-test` installs Ubuntu Desktop with the tools, checks the
clipboard and drags both ways with a GTK 4 (Wayland) program in the guest, then takes
the Tools disc out and puts it in again as the toolbar does: Ubuntu ejects the mounted
disc when asked, mounts the new one, GIO sees software to run on it, and the `sudo`
install above works.
