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
Windows 11 from an ISO) is nearly done:
- **Windows installs unattended** from an ISO onto NVMe, with Secure Boot on and a TPM
  2.0, and comes up with its USB tablet and network working (verified with Windows 10
  22H2; a Windows 11 ISO is still to be tried).
- **Ubuntu 26.04 Desktop installs** with autoinstall and boots to GNOME.
- **A console window** shows the screen and takes the keyboard and pointer.

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
cargo xtask ubuntu-desktop-test                 # Ubuntu 26.04 Desktop autoinstall (downloads 6.5 GB)
cargo xtask linux-shell                         # test Linux with a shell on this terminal
```

`cargo xtask ci` needs `cargo-deny` (`cargo install cargo-deny --locked`).
`fetch-native` and `firmware-test` download OVMF and the TPM libraries from this
repository's releases with the GitHub CLI, so they need an account with access to it.
Build after fetching: a `vmmc-vmx` built without the TPM libraries refuses `--tpm`.

## Running a VM

With UEFI firmware, after `cargo xtask fetch-native`:

```powershell
cargo run -p vmmc-vmx -- run --firmware target\native\OVMF_CODE.fd `
    --vm-dir my-vm --disk disk.raw --cpus 4 --memory 4096
```

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
`--enlightenments` exposes Hyper-V's paravirtual interfaces (reference TSC page, synthetic
timers, hypercalls for TLB flushes and IPIs), which the Windows hypervisor implements
itself; Windows guests use them.
`--display window` shows the guest's screen in a window, one guest pixel per screen
pixel, and follows the guest when it changes resolution:
- **Pointer:** the host pointer drives a USB tablet, so it moves freely in and out.
- **Keyboard:** while the window is in front, every key goes to the guest, the Windows
  key and Alt+Tab included.
- **Toolbar:** Ctrl+Alt+Del (which Windows never lets an application intercept),
  the ACPI power button, reset, a screenshot (with `--screenshot`), and About.
- **Closing:** closing the window stops the VM.

`--resolution WxH` sets the resolution the display offers the guest (its EDID), which
the firmware boots in, Windows keeps (it has no driver for this display yet) and Linux
prefers. Without it, a VM with a window gets the largest standard resolution that fits
on your screen, and one without gets 1280x800.
For everyday use, run a release build (`cargo run --release -p vmmc-vmx -- run ...`):
dev builds optimize their dependencies, but vmmc's own device code runs unoptimized.

### A Windows VM from scratch

After `cargo xtask fetch-native` (OVMF and the TPM libraries), from the repository:

```powershell
cargo build --release -p vmmc-vmx
$vmmc = "target\release\vmmc-vmx.exe"
& $vmmc create-disk C:\VMs\windows\disk.raw --size 64G
& $vmmc run --firmware target\native\OVMF_CODE.fd --vm-dir C:\VMs\windows `
    --disk C:\VMs\windows\disk.raw,nvme --cdrom C:\path\to\Windows.iso --tpm `
    --nic e1000e --cpus 4 --memory 8192 --display window
```

1. When the window shows "Press any key to boot from CD or DVD", press a key (in the
   window or the terminal). It waits only a few seconds; if you miss it, press Reset.
2. Install as on a PC. Choose your own keyboard layout: the window sends the keys'
   positions, so the layout set in Windows applies. Ctrl+Alt+Del is on the toolbar.
3. Setup restarts a few times. Don't press a key at the CD prompt then, and it boots
   from the disk.
4. From then on, start the VM with the same command without `--cdrom`.

- `create-disk` makes a sparse image: it takes space only as Windows writes (about
  10 GB after a fresh Windows 10 install). Put it on a fast local drive.
- The VM directory holds the VM's UEFI variables (Secure Boot is on), its TPM and its
  identity. Keep it with the disk, and don't run two VMs on one disk.
- Windows 11's requirements (TPM 2.0, Secure Boot, 4 GB of memory, 64 GB of disk) are
  met. So far Windows 10 22H2 is what's been tested.
- There's no Windows display driver yet. Windows keeps the resolution the VM starts with
  (`--resolution WxH`; by default the largest standard one that fits your screen).

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
and **Ctrl-A Ctrl-A** sends a Ctrl-A to the guest.
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
| `crates/disk` | Disk images: the block backend trait, raw (sparse) image files |
| `crates/net` | User-mode NAT networking (smoltcp): DHCP, DNS, TCP/UDP to host sockets |
| `crates/tpm` | The TPM 2.0 engine: Microsoft's reference implementation, linked from the CI build |
| `crates/machine` | The x86 PC platform: memory map, ACPI, SMBIOS, firmware and Linux boot, vCPU threads |
| `bins/vmmc-vmx` | Runs one VM, with its serial console on the terminal and optionally its screen in a Slint window |
| `xtask` | Developer tasks and test guest artifacts |
| `third_party` | Upstream crates with small Windows-build patches (see each `VMMC-PATCH.md`) |

## License

Apache-2.0, see [LICENSE](LICENSE). See [CONTRIBUTING.md](CONTRIBUTING.md) before sending
changes.

The console window uses [Slint](https://slint.dev) under its royalty-free licence; the
window's About box carries Slint's attribution.

VMware and VMware Workstation are trademarks of Broadcom Inc. This project is not
affiliated with or endorsed by Broadcom.
