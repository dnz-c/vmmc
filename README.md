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
Windows 11 from an ISO) is next.

What exists:
- **Platform:** a Q35-like PC with ACPI, SMP, an IOAPIC, PIT, RTC, 16550 serial console,
  PCIe (port I/O and ECAM) and virtio-blk with MSI-X.
- **Boot:** direct Linux boot (bzImage + initramfs) in 64-bit mode.
- **Worker:** `vmmc-vmx`, which puts the guest's serial console on your terminal.

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
cargo xtask boot-test                           # boots the test guests on WHP
cargo xtask linux-shell                         # test Linux with a shell on this terminal
```

`cargo xtask ci` needs `cargo-deny` (`cargo install cargo-deny --locked`).

## Running a VM

```powershell
cargo run -p vmmc-vmx -- run --kernel bzImage --initrd initramfs.cpio `
    --disk disk.raw --cpus 4 --memory 1024 --cmdline "console=ttyS0"
```

The guest's serial console is the terminal. **Ctrl-A x** quits, **Ctrl-A p** presses the
ACPI power button, and **Ctrl-A Ctrl-A** sends a Ctrl-A to the guest.
`VMMC_LOG=debug` turns on logging to stderr.

Exit status: 0 when the guest powers off (or you quit), 2 when it resets, 3 on a triple
fault, 1 on an error.

## Layout

| Path | Purpose |
|---|---|
| `crates/hv` | Backend-neutral hypervisor traits: partition, vCPU, exits, interrupts |
| `crates/hv-whp` | Windows Hypervisor Platform backend, including its instruction emulator |
| `crates/vm-core` | Guest memory, port/MMIO buses and the vCPU run loop |
| `crates/devices` | Device models: chipset, PCI, virtio |
| `crates/machine` | The x86 PC platform: memory map, ACPI, Linux boot, vCPU threads |
| `bins/vmmc-vmx` | Runs one VM with its console on the terminal |
| `xtask` | Developer tasks and test guest artifacts |
| `third_party` | Upstream crates with small Windows-build patches (see each `VMMC-PATCH.md`) |

## License

Apache-2.0, see [LICENSE](LICENSE). See [CONTRIBUTING.md](CONTRIBUTING.md) before sending
changes.

VMware and VMware Workstation are trademarks of Broadcom Inc. This project is not
affiliated with or endorsed by Broadcom.
