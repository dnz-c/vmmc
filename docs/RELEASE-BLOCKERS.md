# Before a release

What has to change before vmmc ships to users, collected from the roadmap's notes.
Each item says where it comes from and what doing it takes. Strike items off here when
they're done.

## Identity: IDs that aren't vmmc's own

vmmc's devices borrow IDs that belong to others. Fine for development, not for a
product: drivers match on them, and they say who made the device.

| What | Now | Where |
|---|---|---|
| Display adapter PCI ID | 1234:1111 (QEMU/Bochs), subsystem 1af4:1100 (Red Hat) | `crates/devices/src/display/bochs.rs` |
| NVMe controller PCI ID | 1b36:0010 (QEMU), subsystem 1af4 | `crates/devices/src/storage/nvme.rs` |
| xHCI controller PCI ID | 1b36:000d (QEMU), subsystem 1af4 | `crates/devices/src/usb/xhci.rs` |
| USB tablet and keyboard | 1209:0001 and 1209:0002 (pid.codes test IDs) | `crates/devices/src/usb/hid.rs` |
| Monitor EDID manufacturer | "VMC" (unregistered) | `crates/devices/src/display/edid.rs` |

To do: register a PCI-SIG vendor ID (or a sub-allocation) and a USB vendor ID, and a
three-letter PNP ID for the EDID. Changing the display adapter's ID also changes the
display driver's INF (`guest-drivers/windows/display/vmmcdisp.inf`) and its helper,
which looks for `VEN_1234&DEV_1111`.

## Signing

- **The Windows display driver** (`guest-drivers/windows`) is test-signed, so it loads
  only with test signing on, which Secure Boot rules out. Ordinary VMs need Microsoft's
  attestation signature: an EV code-signing certificate in the company's name, a
  Windows Hardware Dev Center (Partner Center) account, and submitting each build.
  The same goes for every future kernel driver (Phase 5's networking ones too). The
  package itself is ready for that: its INF passes `InfVerif /w`, which every build
  runs.
- **vmmc's own programs** (`vmmc-vmx.exe`, the app to come) should be Authenticode
  signed with the company's certificate, or SmartScreen warns on every download.

## Decisions taken as stopgaps

- **The TPM's manufacturer ID is "MSFT"** (Microsoft's reference TPM code, as
  Hyper-V's vTPM reports it). Windows' TPM tools fail on IDs they don't know, so
  "VMMC" didn't work. Revisit: check what Windows needs, or keep "MSFT" with a legal
  check. To change it: the "Set the TPM's manufacturer ID" step in
  `.github/workflows/native-deps.yml`, a new release, and its pin in
  `xtask/src/native.rs`. TPMs made earlier keep the old ID in their EK certificate.

- **The Linux agent's drags into Wayland programs lean on mutter's insides.** mutter
  shows its XDND window for a Wayland program only when a drag goes onto another
  window than the last drag did, so the agent sends to that window, hidden, when it
  sees nothing under the pointer (`guest-drivers/linux/agent/src/x11.rs`,
  `stand_in`). A mutter that changes how it names or finds those windows breaks
  drags in. Better: a fix in mutter (its `focus_surface` reset at the end of a drag),
  or a native Wayland source once GNOME lets one start a drag without a press.
- **Starting VMs unsandboxed is an environment variable.** Where the sandbox can't
  start (software that loads itself into every program and fails there, a desktop
  that doesn't admit sandboxed programs), the only way out is `VMMC_NO_SANDBOX=1`.
  The app should say what failed and offer to start that VM unsandboxed, with a
  warning.

## Compatibility

- **Grain tables vmmc allocates on demand** go at the end of the file, which VMware's
  checker calls corrupt (it wants all of a sparse disk's tables ahead of its grains).
  Neither VMware nor vmmc makes disks whose directory has empty entries, so it only
  happens with disks other tools made. Fix: allocate a missing table where VMware's
  layout puts it, or rewrite the disk, as expanding does (`crates/disk`).

## Security

- **UEFI variables aren't protected from the guest OS.** WHP has no SMM, so OVMF is
  built without it, and a guest kernel can write the variable store directly,
  Secure Boot's keys included. A host-side variable store would fix it (the roadmap's
  hardening item).
- **The worker sandbox can read what the user can.** Its restricting SIDs include
  the user, so a compromised worker could read the user's files (it can write only to
  its VM's folder). Tighter: drop the user from the restricting SIDs and have the
  launcher open the files a VM reads (ISOs, firmware, VMware's base disks) and pass
  their handles. AppContainer is the other option.
- **The TPM's state file isn't encrypted** (`tpm-state.bin` in the VM directory).
  Anyone who can read the VM's files can read its TPM secrets, such as BitLocker's.

## Housekeeping

- `deny.toml` ignores RUSTSEC-2026-0192 (ttf-parser unmaintained, reached only through
  winit's Wayland decorations) and RUSTSEC-2026-0370 to -0372 (zbus's crates
  unmaintained, reached only through Slint's winit backend on Linux). Recheck before
  release.
- The GitHub release `native-deps-3` is superseded and can be deleted.
