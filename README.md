# vmmc

A memory-safe desktop hypervisor written in Rust: an open alternative to VMware
Workstation Pro for everyday VM users. `vmmc` is a working codename.

- **No account wall.** Download it and run it.
- **Memory-safe device emulation.** Guest-to-host escapes through virtual hardware, such as
  CVE-2026-59346 in VMware's VMXNET3 NIC, are the bug class Rust rules out.
- **Your VMware VMs, in place.** VMDK is the native disk format and `.vmx` files import
  directly, so moving over (or back) needs no conversion.
- **Open core.** The engine and desktop app are Apache-2.0. A paid Pro tier will cover
  team and remote-host features.

## Status

Phase 1 of [the roadmap](docs/ROADMAP.md) is done: **Linux boots on the Windows
Hypervisor Platform**. Ubuntu 26.04's kernel reaches userspace in about a third of a
second on 4 vCPUs, uses a virtio disk, and powers off cleanly. Phase 2 (installing
Windows from an ISO) is done:
- **Windows installs unattended** from an ISO onto NVMe, with Secure Boot on and a TPM
  2.0, and comes up with its USB tablet and network working (verified with Windows 10
  22H2).
- **Ubuntu 26.04 Desktop installs** with autoinstall and boots to GNOME.
- **A console window** shows the screen and takes the keyboard and pointer. With
  vmmc's Windows display driver, Windows follows the window's size and its cursor is
  drawn by the window.

Phase 3 (the desktop app and moving over from VMware) is under way:
- **VMs are files:** a folder with a `.vmmc` config, made by `vmmc-cli create`, run by
  `vmmc-vmx run --vm`.
- **Disks are VMDKs**, VMware's format: vmmc reads and writes the kinds VMware makes,
  and VMware's own tools accept vmmc's.
- **VMware VMs open in place** (`vmmc-cli import`), behind delta disks that keep
  VMware's files unchanged.
- **VMs run in sandboxed workers** (`vmmc-cli start`): a background process per VM,
  at low integrity with a restricted token, which can write only to its VM's folder.
- **The app** (`vmmc`): the library, a New VM wizard with **Easy Install** (Windows
  and Ubuntu install with no questions), settings, and each running VM's screen in a
  tab.

What a release still needs is listed in [docs/RELEASE-BLOCKERS.md](docs/RELEASE-BLOCKERS.md).

What exists:
- **Platform:** a Q35 PC (MCH host bridge, ICH9 LPC) with ACPI, SMP, an IOAPIC, PIT,
  RTC, 16550 serial console, PCIe (port I/O and ECAM), NVMe and virtio-blk disks, and
  an AHCI controller for SATA disks and DVD drives, a linear-framebuffer display
  adapter, virtio-net or Intel e1000e network adapters on user-mode NAT, a PS/2
  keyboard and mouse, an xHCI controller with a USB tablet and keyboard, and a TPM 2.0.
- **Boot:** UEFI firmware from flash (fw_cfg, ACPI through the table loader, SMBIOS), or
  direct Linux boot (bzImage + initramfs) in 64-bit mode.
- **Worker:** `vmmc-vmx`, which puts the guest's serial console on your terminal and,
  with `--display window`, its screen in a window.
- **Command line:** `vmmc-cli`, which makes VMs, imports VMware's, keeps the library
  and handles disks.

## Requirements

- Windows 11 x64.
- The Windows Hypervisor Platform feature, enabled from an administrator PowerShell, then
  a reboot:

  ```powershell
  Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform
  ```

  This turns on the Hyper-V hypervisor. Other software that drives VT-x directly stops
  working while it is on; `bcdedit /set hypervisorlaunchtype off` (plus a reboot) turns it
  off again.
- Rust via rustup. `rust-toolchain.toml` pins the version and rustup installs it.

## Build and test

```powershell
cargo test --workspace                          # unit and fake-hypervisor tests
cargo xtask ci                                  # fmt, clippy, tests, cargo-deny
cargo xtask fetch-guests                        # test kernel, BusyBox initramfs, disk
cargo xtask fetch-native                        # CI-built OVMF and TPM (needs `gh auth login`)
cargo xtask boot-test                           # every WHP boot test below
cargo xtask linux-test                          # test Linux, booted directly
cargo xtask firmware-test                       # OVMF's shell and variable store
cargo xtask uefi-linux-test                     # test Linux through OVMF, from NVMe
cargo xtask uefi-cd-test                        # test Linux through OVMF, from an ISO
cargo xtask win11-test                          # unattended Windows 11 install (needs VMMC_WIN11_ISO)
cargo xtask win-perf                            # boot timings of that Windows, with and without enlightenments
cargo xtask guest-drivers                       # build and test-sign the Windows guest drivers (needs the WDK)
cargo xtask win-display-test                    # install Windows with the display driver, change resolution
cargo xtask ubuntu-desktop-test                 # Ubuntu 26.04 Desktop autoinstall (downloads 6.5 GB)
cargo xtask vmdk-compat-test                    # vmmc's VMDKs against VMware's vdiskmanager (needs VMware)
cargo xtask vmware-import-test                  # open a VMware VM in place and boot it (VMMC_VMWARE_VMX)
cargo xtask worker-test                         # VMs in sandboxed workers, controlled through their pipes
cargo xtask easy-install-test                   # Easy Install of Windows ($VMMC_WIN11_ISO) and Ubuntu
cargo xtask linux-shell                         # test Linux with a shell on this terminal
```

`cargo xtask ci` needs `cargo-deny` (`cargo install cargo-deny --locked`).
`fetch-native` and `firmware-test` download OVMF and the TPM libraries from this
repository's releases with the GitHub CLI, so they need an account with access to it.
Build after fetching: a `vmmc-vmx` built without the TPM libraries refuses `--tpm`.

## The app

```powershell
cargo build --release -p vmmc -p vmmc-vmx -p vmmc-cli
target\release\vmmc.exe
```

- **New VM** makes a VM from an OS preset (Windows 11, Windows 10, Ubuntu, other Linux,
  other) with its install disc. With **Easy Install** (Windows and Ubuntu) it installs
  with no questions: give a user name and password (and the Windows edition), and it
  starts installing at once. vmmc makes an answer disc (Windows' `autounattend.xml`,
  Ubuntu's autoinstall), types what the installer's first screens need (Windows' "Press
  any key to boot from CD or DVD", Ubuntu's GRUB command line), and when Setup is done,
  takes the install discs out (deleting the answer disc, which holds the password) and
  starts the guest from its disk. The guest gets your time zone, and Ubuntu your
  keyboard layout and locale.
- **Import VMware VM** opens a `.vmx` in place (see below).
- **A VM's tab** shows its hardware and notes and starts it; while it runs, its screen.
  Click the screen to type in the guest: every key goes to it, the Windows key and
  Alt+Tab included, until **Ctrl+Alt** gives the keyboard back. The pointer is a USB
  tablet, so it moves freely in and out. With **Fit guest to window**, a Windows guest
  with vmmc's display driver takes the window's size. **Full screen**, **Ctrl+Alt+Del**,
  shut down (the power button), power off and reset are on the toolbar.
- **The clipboard** (text) is shared with the guest whose console is open, once
  **Install vmmc Tools** has put vmmc's agent in it (Windows, or Linux with
  `sudo sh /media/$USER/VMMC_TOOLS/install-linux.sh`): what you copy on the host can be
  pasted in the guest, and the other way round. The agent needs no driver, so it works
  with Secure Boot on.
- **Drag and drop:** drag files or folders from Explorer onto the console and drop them
  where the pointer is in the guest (a folder, the desktop, a program's window); drag
  them out of the guest past the console's edge and drop them on the host. The files go
  over the VM's network adapter (to vmmc itself, nothing leaves the host), at the
  network's speed; a drop waits until they're all there. In a Linux guest the agent works
  through XWayland, so Wayland desktops (GNOME's) take part.
- VMs keep running when the app closes; open it again to get back to them.
- The library is `%APPDATA%\vmmc\library.toml`; `VMMC_LIBRARY` names another.

## Running a VM

A VM is a folder: its config (`NAME.vmmc`, TOML), its disk (`NAME.vmdk`), its UEFI
variables (`nvram.fd`) and its TPM (`tpm-state.bin`). After `cargo xtask fetch-native`:

```powershell
cargo build --release -p vmmc-vmx -p vmmc-cli
target\release\vmmc-cli.exe create "Ubuntu" --os ubuntu --iso C:\ISOs\ubuntu.iso
target\release\vmmc-vmx.exe run --vm "$HOME\Documents\vmmc VMs\Ubuntu\Ubuntu.vmmc" --display window
```

To run it in the background instead, as the app will:

```powershell
target\release\vmmc-cli.exe start "Ubuntu"     # by name in the library, or its .vmmc file
target\release\vmmc-cli.exe status "Ubuntu"
target\release\vmmc-cli.exe stop "Ubuntu"      # the power button; --hard pulls the plug
```

A started VM runs in its own worker process, `vmmc-vmx serve`, which keeps running
when the command line (or the app) is gone, and is controlled through a named pipe
(`\\.\pipe\vmmc-<uuid>`). The worker runs sandboxed: at low integrity, with a
restricted token and in a job that can't start programs or touch the desktop. It can
read what you can read, but write only to its VM's folder (which vmmc labels for it,
granting you the right to label it first if the drive doesn't), so a disk the VM
writes to must be in its folder. Its output goes to `vmmc-vmx.log` there.
`VMMC_NO_SANDBOX=1` turns the sandbox off, for finding out whether it's in the way.

`vmmc-cli create ... --easy-install --password PASSWORD` installs the OS unattended,
as the app's Easy Install does (`--user`, `--edition` and `--keyboard` change the
defaults).
`create` picks the hardware from `--os` (`windows11`, `windows10`, `ubuntu`, `linux`,
`other`); `--cpus`, `--memory`, `--disk-size` and `--dir` change it, and the config can
be edited afterwards. `vmmc-cli list` shows the library, `vmmc-cli disk` creates,
describes and converts disks, and `vmmc-cli create --from-vm-dir DIR --disk IMAGE` turns
a VM run the old way (below) into one with a config. `vmmc-vmx` finds the firmware
next to itself or, in a build, in `target\native`.

Everything can also be given as options, without a config (the tests do this):

```powershell
cargo run -p vmmc-vmx -- run --firmware target\native\OVMF_CODE.fd `
    --vm-dir my-vm --disk disk.vmdk --cpus 4 --memory 4096
```

With `--vm`, the options add disks, CD-ROMs and NICs to the config's, and override the
rest.
`--disk PATH,nvme` attaches a disk as a namespace of the NVMe controller, which UEFI,
Windows and Linux all have drivers for. `--disk PATH,sata` puts it on the AHCI
controller, and plain `--disk PATH` makes it virtio-blk. `--cdrom ISO` adds a DVD drive
with that disc.
`--vm-dir` holds what the VM keeps between runs: its UEFI variable store (`nvram.fd`,
copied on first use from `OVMF_VARS.secboot.fd` next to the firmware, which has Secure
Boot on with Microsoft's keys, or else `OVMF_VARS.fd`; `--vars-template` picks another)
and its SMBIOS UUID. `--tpm` gives the VM a TPM 2.0 whose state lives there too
(`tpm-state.bin`).
`--firmware-log FILE` saves the firmware's debug output when it's a debug build.
`--nic e1000e` (or `virtio`) adds a network adapter behind a user-mode NAT: DHCP gives the
guest 10.0.2.15, DNS uses the host's resolver, and 10.0.2.2 reaches the host itself.
`--screenshot DIR` saves the screen as PNG when the VM stops and on each Ctrl-A s;
add `--screenshot-every SECS` for a timed series.
Guests see Hyper-V's paravirtual interfaces (reference TSC page, synthetic timers,
hypercalls for TLB flushes and IPIs), which the Windows hypervisor implements itself.
Windows and Linux use them; an idle Windows then costs the host about 2% of a CPU
rather than 10-25%. `--no-enlightenments` hides them.
`--display window` shows the guest's screen in a window, one guest pixel per screen
pixel, and follows the guest when it changes resolution:
- **Size:** resizing the window asks the guest for that resolution. Windows with
  vmmc's display driver switches to it (see [guest drivers](guest-drivers/windows/README.md));
  other guests get it as their monitor's preferred resolution from their next boot.
- **Pointer:** the host pointer drives a USB tablet, so it moves freely in and out.
  With vmmc's display driver, Windows' cursor is a hardware one: the window draws it
  at your pointer, so it doesn't lag behind.
- **Keyboard:** while the window is in front, every key goes to the guest, the Windows
  key and Alt+Tab included.
- **Toolbar:** Ctrl+Alt+Del (which Windows never lets an application intercept),
  the ACPI power button, reset, a screenshot (with `--screenshot`), and About.
- **Closing:** closing the window stops the VM.

`--resolution WxH` sets the resolution the display offers the guest (its EDID), which
the firmware boots in and Windows and Linux prefer. Without it, a VM with a window gets
the largest standard resolution that fits on your screen, and one without gets
1280x800. Linux's own `bochs-drm` driver runs the display: resolutions are chosen in
the guest, and don't follow the window.
For everyday use, run a release build (`cargo run --release -p vmmc-vmx -- run ...`):
dev builds optimize their dependencies, but vmmc's own device code runs unoptimized.

### A Windows VM from scratch

After `cargo xtask fetch-native` (OVMF and the TPM libraries), from the repository:

```powershell
cargo build --release -p vmmc-vmx -p vmmc-cli
target\release\vmmc-cli.exe create "Windows 11" --os windows11 --dir C:\VMs `
    --iso C:\path\to\Windows.iso
target\release\vmmc-vmx.exe run --vm "C:\VMs\Windows 11\Windows 11.vmmc" --display window
```

The `windows11` preset is 4 vCPUs, 8 GB of memory, a 64 GB NVMe disk, an e1000e NIC,
Secure Boot and a TPM.

1. When the window shows "Press any key to boot from CD or DVD", press a key (in the
   window or the terminal). It waits only a few seconds; if you miss it, press Reset.
2. Install as on a PC. Choose your own keyboard layout: the window sends the keys'
   positions, so the layout set in Windows applies. Ctrl+Alt+Del is on the toolbar.
3. Setup restarts a few times. Don't press a key at the CD prompt then, and it boots
   from the disk.
4. Once installed, take the ISO out: delete the `[[cdrom]]` entry from the `.vmmc`.

- The disk is a sparse VMDK: it takes space only as Windows writes (about 10 GB after
  a fresh Windows 10 install). Put the VM on a fast local drive.
- The VM's folder holds its UEFI variables (Secure Boot is on), its TPM and, in the
  config, its identity (UUID and MAC). Move it as a whole. A disk in use by one VM
  can't be opened by another (or by VMware).
- Windows 11's requirements (TPM 2.0, Secure Boot, 4 GB of memory, 64 GB of disk) are
  met. So far Windows 10 22H2 is what's been tested.
- Without vmmc's display driver, Windows keeps the resolution the VM starts with
  (`--resolution WxH`; by default the largest standard one that fits your screen). The
  driver is test-signed for now, so it needs a VM with Secure Boot off; see
  [guest drivers](guest-drivers/windows/README.md).

### Opening a VMware VM

```powershell
target\release\vmmc-cli.exe import "D:\VMs\Windows 10\Windows 10.vmx"
target\release\vmmc-vmx.exe run --vm "D:\VMs\Windows 10\vmmc\Windows 10.vmmc" --display window
```

The import reads the `.vmx` and puts vmmc's files in a `vmmc` folder beside it: the
config, fresh UEFI variables, and a delta disk on top of each of VMware's disks, which
takes everything the guest writes. VMware's disks, `.vmx` and `.nvram` are never
written, so the VM still runs in VMware as before, and deleting the `vmmc` folder undoes
the import. (If the VM runs in VMware again afterwards, its disks change, and vmmc
refuses the deltas it made on the old contents, as VMware would.)

- **What carries over:** CPUs, memory, NVMe and SATA disks, the ISO in the DVD drive,
  the NICs' MAC addresses (as e1000e on NAT), Secure Boot, and the system UUID.
- **What doesn't:** sound, 3D, shared folders, bridged networking, nested
  virtualization; the import lists what it left out. Windows sees new hardware and
  may want to be activated again; a VM with a TPM gets a new one, so BitLocker asks
  for its recovery key (the import warns when it finds a BitLocker volume).
- **What it refuses:** encrypted VMs (remove the encryption in VMware first), VMs that
  boot with BIOS, suspended VMs, and VMs VMware has open.

Or a Linux kernel, booted directly:

```powershell
cargo run -p vmmc-vmx -- run --kernel bzImage --initrd initramfs.cpio `
    --disk disk.raw --cpus 4 --memory 1024 --cmdline "console=ttyS0"
```

The guest's serial console is the terminal. **Ctrl-A x** quits, **Ctrl-A p** presses the
ACPI power button, **Ctrl-A r** prints each vCPU's registers and the code at its RIP
(for finding where a hung guest is stuck), **Ctrl-A m** saves guest RAM and every vCPU's
state to a new `vmmc-memory-*` directory in the temp directory, **Ctrl-A k** switches
between typing on the serial port and typing on the guest's PS/2 keyboard (US layout),
**Ctrl-A z** followed by a size such as `1500x844` and Enter asks the guest for that
resolution (as resizing the window does), and **Ctrl-A Ctrl-A** sends a Ctrl-A to the
guest.
`VMMC_LOG=debug` turns on logging to stderr.

When the guest resets, the VM restarts; `--no-reboot` makes it exit instead. Exit
status: 0 when the guest powers off (or you quit), 2 when it resets with `--no-reboot`,
3 on a triple fault, 1 on an error.

## Layout

| Path | Purpose |
|---|---|
| `crates/hv` | Backend-neutral hypervisor traits: partition, vCPU, exits, interrupts |
| `crates/hv-whp` | Windows Hypervisor Platform backend, including its instruction emulator |
| `crates/vm-core` | Guest memory, port/MMIO buses and the vCPU run loop |
| `crates/devices` | Device models: chipset, firmware interfaces (fw_cfg, flash), PCI, NVMe, AHCI/ATAPI, e1000e, xHCI and USB HID, PS/2, TPM CRB, virtio |
| `crates/disk` | Disk images: the block backend trait, VMDK (sparse, split, flat, stream-optimized, delta chains) and raw images |
| `crates/vmconfig` | VM files: the `.vmmc` config, the library, OS presets, and VMware `.vmx` import |
| `crates/ipc` | The worker's pipe protocol and its shared screen |
| `crates/client` | Starting workers in their sandbox, and talking to them (`vmmc-client`) |
| `crates/media` | ISO images, and Easy Install's answer files (Windows `autounattend.xml`, Ubuntu autoinstall) |
| `crates/keyhook` | The console's keyboard on Windows: a low-level hook giving PS/2 scan codes |
| `crates/net` | User-mode NAT networking (smoltcp): DHCP, DNS, TCP/UDP to host sockets |
| `crates/tpm` | The TPM 2.0 engine: Microsoft's reference implementation, linked from the CI build |
| `crates/machine` | The x86 PC platform: memory map, ACPI, SMBIOS, firmware and Linux boot, vCPU threads |
| `bins/vmmc-vmx` | Runs one VM, with its serial console on the terminal and optionally its screen in a Slint window |
| `bins/vmmc` | The desktop app (Slint): the library, the New VM wizard, settings, consoles |
| `bins/vmmc-cli` | The command line: create, import, start and stop VMs, the library, disks |
| `guest-drivers/windows` | Windows drivers for vmmc's devices: the display (resolution changes); see its README |
| `xtask` | Developer tasks and test guest artifacts |
| `third_party` | Upstream crates with small Windows-build patches (see each `VMMC-PATCH.md`) |

## License

Apache-2.0, see [LICENSE](LICENSE). See [CONTRIBUTING.md](CONTRIBUTING.md) before sending
changes.

The console window uses [Slint](https://slint.dev) under its royalty-free licence; the
window's About box carries Slint's attribution.

VMware and VMware Workstation are trademarks of Broadcom Inc. This project is not
affiliated with or endorsed by Broadcom.
