# Roadmap: `vmmc`, an open-core Rust desktop hypervisor to replace VMware Workstation Pro

> `vmmc` is a working codename taken from the folder name. The final product name must not use "VMware" or "Workstation" (trademarks). Saying "opens VMware® VMs" is fine as nominative use.

## Context

**Goal:** Build a sellable, memory-safe replacement for VMware Workstation Pro, written in Rust, that gets everyday VM users away from Broadcom.

**VMware Workstation Pro today (Oct 2026):**
- **Versions:** The latest release is 26H1u1 (3 Sep 2026, build 25688693). 26H1 (May 2026) made the Windows build 64-bit and added VM creation and last-power-on timestamps, notes in folder tabs, and a Tech Preview for remote ARM ESXi hosts. 25H2 (Oct 2025) moved to calendar versioning and added virtual hardware v22 (NVMe default, USB 3.2, D3D11/GL4), `dictTool` and Hyper-V/WHP detection. It also removed `vctl`.
- **Price and access:** It is free for personal, commercial and education use. The installer is only available behind a Broadcom Support Portal account.
- **Security:** CVE-2026-59346 is a guest-to-host escape in the VMXNET3 virtual NIC, and a public PoC exists. This is exactly the bug class Rust device emulation prevents.
- **What this means for the product:** We cannot win on price, because VMware is already free. We win on trust and openness: no account wall, open-source core, memory-safe device models, and opening existing VMware VMs in place with no lock-in. Revenue comes from a Pro tier.

**Decisions already made:**
- **Engine:** our own VMM, built on rust-vmm crates.
- **Hypervisor backend:** Windows 11 x64 host first, using the OS hypervisor API (WHP). We will not write our own VT-x driver. With VBS/HVCI on, Hyper-V already owns VT-x, which is why VMware and VirtualBox use WHP as well.
- **Business model:** open-core.
- **GUI:** Slint.

**Scope reality:** VMware Workstation is about 25 years of work by a large team. This plan is phased so that each phase produces something runnable. Full parity, especially Windows-guest 3D, is explicitly post-1.0.

---

## VMware feature inventory → target phase

| Area | VMware Workstation Pro feature | Phase |
|---|---|---|
| Engine | SMP guests (up to 32 vCPU), large RAM, coexistence with Hyper-V/VBS | 1–2 |
| Firmware/security | BIOS + UEFI, Secure Boot, vTPM 2.0, VM encryption (XTS) | 2 (UEFI/SB/TPM), 6 (encryption) |
| Storage | NVMe (default), SATA/AHCI, SCSI (LSI/PVSCSI), IDE, CD/DVD ISO; VMDK sparse/flat/split; expand/compact/defrag; mount disk | 2 (NVMe/AHCI/ATAPI/VMDK), 3 (compact/expand), 5 (LSI SAS) |
| Snapshots/clones | Snapshot tree + manager, AutoProtect, suspend/resume, linked + full clones | 3 |
| Networking | NAT (DHCP, port forwarding), bridged, host-only, custom VMnets, LAN segments, Virtual Network Editor, network simulation | 2 (NAT), 5 (all the others) |
| Devices | USB 3.2 passthrough, sound, serial/parallel ports, Bluetooth sharing, smart card, host CD/DVD drive | 3 (audio), 5 (USB, serial, CD), later (BT, smart card) |
| Guest integration | VMware Tools: clipboard, drag & drop, shared folders (HGFS), display autofit, time sync, graceful power ops, Unity | 4 (Unity: post-1.0) |
| Graphics | SVGA3D, DX11 / OpenGL 4.x, up to 8 GB vGPU memory, multi-monitor | 2 (2D GOP), 4 (virtio-gpu 2D + resize), post-1.0 (3D) |
| Management UI | Library with folders and tabs, New VM wizard + Easy Install, settings editor, fullscreen, multi-monitor, VM notes, timestamps | 3 |
| Import/export | OVF/OVA, open VMs in place | 3 (our key differentiator: native .vmx + VMDK) |
| Automation | `vmrun`, `vmcli`, REST API (`vmrest`), `vmware-vdiskmanager`, `dictTool` | 3 (CLI), 6 (REST) |
| Remote | Connect to ESXi/vCenter, upload VMs | Pro, post-1.0 |
| Advanced | Nested VT-x/EPT, virtual IOMMU, perf counters | post-1.0 (if WHP nested support allows) |
| Hosts | Windows x64, Linux x64 (Fusion on macOS) | Windows: 1–5; Linux/KVM: 6; macOS: post-1.0 |

**Open-core split:**
- **Apache-2.0 (free):** engine, GUI, NAT, snapshots, clones, VMware/OVF import, vTPM, Secure Boot, CLI, guest tools. Windows 11 must work on the free tier.
- **Proprietary (paid "Pro"):**
  - remote/headless host management (`vmmcd` + REST, and later ESXi/Proxmox);
  - fleet management: policy-managed/restricted VMs, central licensing, SSO;
  - template library and provisioning;
  - LTS builds and support.
- **Contributions:** require a CLA so that relicensing for the Pro tier stays possible.

---

## Architecture

### Process model (mirrors VMware's `vmware.exe` / `vmware-vmx.exe` split)

- **`vmmc` (GUI, Slint):** VM library, wizard, settings and console view.
  - Talks to workers over named pipes with versioned `serde`/`postcard` messages.
  - The console framebuffer arrives through a shared-memory section with dirty rectangles, and is uploaded straight to a GPU texture.
- **`vmmc-vmx` (one worker process per running VM):** owns the WHP partition, vCPU threads, device model, disks and NAT stack.
  - Runs with a restricted token at low integrity, so a guest escape lands in a sandbox rather than the user session.
- **`vmmc` CLI:** the `vmrun` equivalent, using the same IPC.
- **Later, a Windows service:** only for things that need privilege, such as bridged networking, USB arbitration and installing the WHP feature.

### Cargo workspace layout (target; Phase 0 has `hv`, `hv-whp`, `vm-core` and `xtask`)

```
Cargo.toml (workspace)   rust-toolchain.toml   deny.toml   LICENSE (Apache-2.0)
crates/
  hv/          backend-neutral traits: Partition, Vcpu, VcpuExit, memory mapping, interrupts
  hv-whp/      WHP backend via the `windows` crate (Win32_System_Hypervisor) + WinHvEmulation for MMIO/PIO instruction decode
  hv-kvm/      (phase 6) kvm-ioctls backend
  vm-core/     guest memory (vm-memory; VirtualAlloc adapter if needed), PIO/MMIO buses, vCPU run loop, IRQ routing, device-state save/restore traits
  devices/     chipset (Q35 MCH, ICH9 LPC, 8259 PIC, IOAPIC, 8254 PIT, CMOS RTC, ACPI PM, HPET, i8042, 16550 via vm-superio),
               pci (PCIe ECAM, MSI/MSI-X), fw_cfg (+ACPI table-loader), storage (AHCI, ATAPI, NVMe, virtio-blk),
               net (e1000e, virtio-net), display (bochs-display/VBE → virtio-gpu), usb (xHCI, HID tablet/keyboard),
               audio (Intel HDA + codec → WASAPI), tpm (CRB front end + ms-tpm-20-ref back end), virtio (virtio-pci + virtio-queue)
  acpi/        ACPI/AML generation (rust-vmm `acpi_tables`)
  disk/        VMDK (hosted sparse, monolithic flat, 2GB split, streamOptimized), raw, delta chains; async I/O via IOCP
  vmconfig/    native TOML VM config + .vmx parser/importer + OVF
  net-nat/     user-mode NAT on smoltcp: DHCP, DNS proxy, TCP/UDP proxying, port forwarding
  snapshot/    snapshot tree, RAM save (WHvQueryGpaRangeDirtyBitmap for live), device state
  ipc/         GUI/CLI ↔ worker protocol
bins/ vmmc-vmx (worker), vmmc (GUI), vmmc-cli
firmware/      pinned edk2 OVMF build script (BSD-2-Clause-Patent), SB-enabled, MS keys pre-enrolled VARS template
xtask/         build OVMF, boot tests, packaging
tools/         (phase 4) guest agent for Windows and Linux
```

### Key technical choices

- **Firmware:** stock OVMF (Q35/QEMU platform via `fw_cfg`). It is the best-tested UEFI for Windows guests and already has drivers for AHCI, NVMe, xHCI, bochs GOP and virtio. We emulate the QEMU-compatible platform it expects. Secure Boot runs without SMM in v1.
- **WHP specifics:**
  - Use WHP's built-in xAPIC/x2APIC emulation (`WHvPartitionPropertyCodeLocalApicEmulationMode`). We emulate only the IOAPIC, PIC and timers in user mode.
  - CPUID and MSR exits are handled through partition properties and exits.
  - Paravirtual devices use doorbell events (`WHvRegisterPartitionDoorbellEvent`) to avoid the cost of MMIO exits.
- **Guest devices, "inbox-driver first":** Windows 10/11 and modern Linux install with no extra drivers: NVMe/AHCI, e1000e, the GOP framebuffer (Microsoft Basic Display), PS/2 + USB tablet, HDA and TPM CRB. Paravirtual virtio devices plus our Tools come in Phase 4 for speed and integration.
- **VMDK as the native disk format, with VMware-style delta-disk snapshots:**
  - VMware VMs open in place, and our VMs can move back to VMware, so there is no lock-in in either direction.
  - VMDK is implemented clean-room from VMware's published VMDK spec.
  - QCOW2 comes later for KVM/Proxmox interop.
- **Code reuse (permissive licenses only, with attribution):**
  - rust-vmm crates: `vm-memory`, `virtio-queue`, `virtio-bindings`, `vm-superio`, `acpi_tables`, `linux-loader` (direct kernel boot for tests).
  - Reference implementations we may borrow from: crosvm (BSD-3: WHPX backend, xHCI, virtio-gpu), cloud-hypervisor (Apache/BSD: PCI, virtio, ACPI, snapshot), OpenVMM (MIT: `virt_whp`, NVMe, chipset), Firecracker.
- **Forbidden:**
  - copying QEMU or VirtualBox code (GPL would break open-core); reading their docs is fine;
  - reverse-engineering VMware binaries (EULA and legal risk); use only public specs: the VMDK spec, DMTF OVF DSP0243 and community .vmx references.
  - `cargo-deny` enforces the license rules in CI.
- **Slint:** use the royalty-free license and ship the `AboutSlint` widget in the About dialog.

---

## Phased roadmap (each phase ends in something runnable)

Rough effort estimates assume 1–2 strong developers and are expected to slip.

### Phase 0: Foundations + WHP spike (about 2–3 weeks): code complete, boot test pending

1. Done: workspace skeleton (`hv`, `hv-whp`, `vm-core`, `xtask`), pinned toolchain, `deny.toml` (permissive licences only), Apache-2.0 LICENSE, README, CONTRIBUTING and CI.
2. Done: **`hv-whp` spike.**
   - WinHvPlatform.dll is loaded at runtime from System32 only, and `WHvGetCapability(HypervisorPresent)` is checked, so a host without WHP gets instructions instead of a loader error.
   - Partition, memory mapping, vCPU and register get/set work. The run loop handles port I/O (IN/OUT), halt, cancel and triple fault.
   - The spike runs without the in-hypervisor local APIC, so `HLT` always exits to the VMM. xAPIC mode is supported by `PartitionConfig` and is needed from Phase 1.
3. Done: `examples/hello_real_mode.rs` and the `hello` integration test. The guest writes `"Hello from vmmc\n"` to port `0xE9`, then stops the VM through exit port `0xF4`. A watchdog cancels it after 10 s.
   - **Pending:** the test has not run on a host with the hypervisor enabled yet. `cargo xtask boot-test` is the gate for closing Phase 0.
4. Done: `vm-memory` 0.18 works on Windows, but only with `default-features = false`, because its default `rawfd` feature is Unix-only.
5. Done: the `hv` traits (`Hypervisor`, `Partition`, `Vcpu`, `VcpuKicker`, `VcpuExit`). `vm-core` holds no WHP types, and its run loop is tested against a fake backend.

### Phase 1: Boots Linux: done (verified on WHP, 2026-10-11)

What was built:

- **`hv` API:** run with callbacks (`CpuIo`), interrupt injection, wait-for-SIPI, and weak kicker and interrupt handles.
- **`hv-whp`:**
  - WinHvEmulation instruction emulator for MMIO and INS/OUTS;
  - `WHvRequestInterrupt`, APIC EOI exits, and xAPIC emulation;
  - APs put in StartupSuspend.
- **Devices:**
  - IOAPIC, 8254 PIT (+ port 0x61), 8259 PIC (register model), CMOS RTC, ACPI PM (PM timer, S5, power button), i8042, 16550 (vm-superio) and POST port.
  - PCI root complex (CF8/CFC, ECAM, BAR remapping), Q35 host bridge, MSI-X, virtio-pci (modern) and virtio-blk.
- **Machine:**
  - Q35-like memory map, e820, and ACPI (RSDP/XSDT/FADT/FACS/MADT/MCFG/DSDT);
  - direct 64-bit Linux boot (`linux-loader`);
  - vCPU threads with stop, reset, triple-fault and error handling.
- **`vmmc-vmx`:** CLI, raw Windows console, Ctrl-A x to quit, Ctrl-A p for the ACPI power button.
- **Test guest:** Ubuntu 26.04 generic kernel + BusyBox initramfs (`cargo xtask fetch-guests`, pinned by SHA-256), and `cargo xtask boot-test` / `linux-shell`.

Scope moved out of Phase 1:
- virtio-console moves to Phase 4.
- PIC interrupt delivery (ExtINT), PCI INTx, HPET, Hyper-V enlightenments and WHP doorbells wait for Phase 2 or a WHP host.

**Exit criterion: met.** `cargo xtask boot-test` passes on a WHP host (Windows 11, i7-13700K), 5 runs out of 5. Ubuntu 26.04's 7.0 kernel reaches `/init` in about 0.33 s on 4 vCPUs, formats, mounts and verifies `/dev/vda`, and powers off through ACPI S5.

**First-run checklist results:**

| # | Assumption | Result |
|---|---|---|
| A1 | APs in StartupSuspend come up through hypervisor-handled INIT/SIPI | ✅ `smp: Brought up 1 node, 4 CPUs` |
| A2 | With xAPIC emulation, HLT with interrupts on is handled inside the hypervisor | ✅ an idle 4-vCPU guest uses 0.16% of one host core |
| A3 | `ApicEoi` exits arrive for level-triggered requests | ✅ two power-button presses raise the SCI (level-triggered) exactly twice |
| A4 | The WHP emulator handles Linux's MMIO instruction mix | ✅ IOAPIC, ECAM, virtio and MSI-X MMIO all work |
| A5 | IRQ0 through IOAPIC GSI2 passes `check_timer`; the PM timer keeps calibration sane | ✅ timer check passes; "PIT calibration matches PMTIMER"; the guest switches to the TSC clocksource |
| A6 | CPUID topology on hybrid Intel CPUs looks sane in the guest | ✅ with harmless noise: no PMU ("software events only"), and one unchecked uncore MSR read (0x396) |
| A7 | Ubuntu's generic kernel has virtio-pci and virtio-blk built in | ✅ |

**What the first real run found and fixed:**
- **Register buffer alignment:** WHP accesses `WHV_REGISTER_VALUE` arrays with `movaps`, which needs 16-byte alignment, but the `windows` crate only guarantees 8. All register buffers and the exit context now use a 16-byte-aligned wrapper. This was an access-violation crash.
- **One VM per process:** Windows lets only one partition per process have guest memory mapped at a time. `WHvMapGpaRange` fails with a misleading status, 0xC0370008, which `hv-whp` now turns into a clear error. One VM per worker process was already the design; tests that boot VMs take turns.
- **ACPI PM1 enable register:** it now keeps the Global Lock and RTC enable bits. ACPI checks they stick and had disabled the Global Lock otherwise.
- **FADT:** `X_FIRMWARE_CTRL` is now 0, as the spec requires when `FIRMWARE_CTRL` is set.

**Known guest-log noise to address with firmware boot in Phase 2:** no DMI/SMBIOS, MTRRs not set up, no `_OSC` on the PCI root, and no PS/2 mouse. (`_OSC` is done in M1; SMBIOS and MTRRs come with firmware boot.)

### Phase 2: Installs Windows 11 from ISO (MVP engine, about 3–5 months): in progress

- **Firmware:** `fw_cfg` + table-loader, ACPI (DSDT/MADT/FADT/MCFG/HPET), and the pinned OVMF build with Secure Boot and MS keys enrolled. UEFI variables persist per VM.
- **Devices:** AHCI (HDD + ATAPI CD-ROM for ISOs), NVMe, bochs-display (GOP), PS/2, xHCI + USB tablet, e1000e and TPM 2.0 CRB. HDA audio moved to Phase 3.
- **Disk and network:** `disk` crate with raw images and sparse files; `net-nat` with DHCP and DNS. VMDK moved to Phase 3.
- **Console:** a minimal Slint console window (framebuffer + input) as the dev UI.
- **Exit criteria:**
  - Windows 11 (current ISO) installs unattended onto NVMe and reaches the desktop with NAT internet. `msinfo32` shows Secure Boot On and TPM 2.0.
  - Ubuntu 26.04 Desktop installs from ISO.

**Where Phase 2 stands (2026-10-13):** both exit criteria are met. Ubuntu Desktop installs and runs (M10). The Windows test passes in full with Windows 10 22H2: Secure Boot on, `Get-Tpm` reporting a ready TPM 2.0 (with the temporary MSFT manufacturer ID, see M7), the USB tablet, the PS/2 keyboard, the EDID's resolution and the network. Decided 2026-10-13: the Windows 10 install counts for the Windows criterion, and the Windows 11 ISO run is skipped. Since then (2026-10-14): Hyper-V enlightenments are on by default after the performance pass (M10), and a Windows display driver lets Windows change resolution (M11). It's test-signed, so for now it loads only in VMs with Secure Boot off; shipping it needs Microsoft's attestation signature.

**Milestones done so far (verified on WHP, 2026-10-12):**

- **M0, repo and native dependencies:** the private GitHub repo, and the `native-deps` workflow. It builds OVMF from `edk2-stable202608` (SMM off, TPM 2.0) as three variants: release with Secure Boot (what VMs run), the same as a debug build, and a debug test build without Secure Boot. The test build exists because edk2 leaves the UEFI shell out of Secure Boot builds. Key enrollment uses `virt-fw-vars`: a generated vmmc platform key plus Microsoft's 2011 and 2023 KEK and db certificates. `cargo xtask fetch-native` downloads the release through `gh` and checks pinned SHA-256s. The builds are reproducible: a rebuild gave byte-identical code volumes.
- **M1, OVMF boots to the UEFI shell:** `cargo xtask firmware-test` passes in about 13 s. It boots to the shell, writes a non-volatile variable, and warm-resets, which rebuilds the machine. The second boot reads the variable back from `nvram.fd`, then `reset -s` powers the VM off through ACPI. The debug log has no ASSERT.
  - **New devices:**
    - `fw_cfg` with DMA and a file directory;
    - the Q35 MCH, where PCIEXBAR moves the ECAM window (OVMF puts it at 0xE0000000);
    - the ICH9 LPC, where PMBASE and ACPI_CNTL move the ACPI PM block;
    - CFI pflash for the variable store, mapped as read-only RAM in read-array mode and written through to `nvram.fd`. The code flash is read-only, and OVMF's HSTI check confirms it.
    - the debug port 0x402.
  - PCI functions 1 to 7 with the multifunction bit.
  - **ACPI:** one table model, either placed at a fixed address (direct Linux boot) or handed over as `etc/acpi/*` files with a QEMU-style table-loader script. It is generated when the firmware reads it, so the ECAM, PM-block and PCI windows match what the firmware set up. New content:
    - processor objects;
    - `_PRT` routing INTx to GSIs 16 to 23;
    - an `_OSC` that grants native PCIe control (Linux now takes PCIe hotplug, PME, AER and the capability structure);
    - S5 as SLP_TYP 0, as on ICH9 and in OVMF's reset library.
  - **SMBIOS 3.0** types 1, 3, 4, 16, 17, 19, 32 and 127, with a UUID kept per VM. OVMF adds type 0.
  - **Firmware mode:** the CMOS memory-size bytes and `etc/e820`.
  - **vmmc-vmx:** `run --firmware --vm-dir` (the VM directory holds `nvram.fd` and `uuid`), `--firmware-log`, and `--no-reboot`. A guest reset now rebuilds the machine and keeps the console attached.
  - **Bugs found on WHP:**
    - WHP creates vCPUs with CS base 0 rather than the architectural 0xFFFF0000, so the boot vCPU is now put in the reset state explicitly.
    - OVMF only treats the flash as writable if "clear status" leaves the status register at 0, as QEMU's does.
    - The PCI ECAM window held its root strongly, a reference cycle that would have leaked every machine on reset.
- **M2, NVMe and Linux through UEFI:** `cargo xtask uefi-linux-test` passes.
  - **The test:** OVMF reads a FAT EFI system partition (built by xtask with `fatfs`) over NVMe. The shell's `startup.nsh` starts the EFI-stub kernel with its initramfs. Linux then:
    - reports EFI, and the DMI product name from our SMBIOS;
    - does the disk round trip on the second NVMe namespace;
    - powers off through S5.
  - It also checks the firmware-placed resources: ECAM at 0xE0000000, a 32-bit window from 0xF0000000, and the 64-bit BAR high in the dynamic window. All of them show up correctly in the root bridge's `_CRS`.
  - **The NVMe controller** (`devices::storage::nvme`) is written from the NVMe 1.4 base specification:
    - admin queue on the vCPU, one worker thread per I/O submission queue;
    - shared completion queues, each with its own MSI-X vector;
    - PRP lists, including chained list pages, with loop protection;
    - Identify (controller, namespace, active list, descriptors);
    - queue creation and deletion with the spec's error codes;
    - Features: number of queues, volatile write cache, plus storage for the rest;
    - log pages: error, SMART and firmware slot;
    - asynchronous event requests held outstanding, and Abort;
    - Read, Write (with FUA), Flush, Write Zeroes and Dataset Management;
    - shutdown notifications flush the disks.

    It passes 11 unit tests and Linux's own driver: 4 I/O queues, two namespaces.
  - **Disks:** the `disk` crate now holds the block backend that virtio-blk and NVMe share. New raw images are NTFS sparse files. `vmmc-vmx run --disk PATH,nvme` attaches a disk to the NVMe controller.
  - **Test guest:** this Ubuntu kernel builds NVMe as a module. xtask takes it (and its four dependencies, in `modules.dep` order) from the same release's initrd, which is SHA-256 pinned and decompressed with `ruzstd`. The init script loads them with `insmod`.
  - **Known:** we borrow QEMU's NVMe PCI IDs (1b36:0010). Linux then applies its QEMU quirk and ignores our namespace GUIDs. That's harmless, but vmmc needs a PCI vendor ID of its own before release.
- **M3, AHCI, ATAPI and booting from an ISO:** `cargo xtask uefi-cd-test` passes.
  - **The test:** xtask writes an ISO (ISO 9660 with Joliet names and an El Torito EFI entry) holding a FAT boot image with the kernel, initramfs and `startup.nsh`. OVMF boots it from the AHCI DVD drive. Linux's `ahci` driver finds the DVD drive and a SATA disk. It mounts the disc as ISO 9660 and reads a file back (`cd=ok`), and does the round trip on `sda`.
  - **AHCI** (`devices::storage::ahci`, ICH9 8086:2922 at 00:1f.2, six ports): written from the AHCI 1.3.1 specification.
    - command lists and tables, with the PRDT;
    - one worker thread per port;
    - received-FIS area with D2H and PIO setup FISes;
    - TFES halting the port until it's restarted;
    - COMRESET and HBA reset;
    - MSI (a new plain MSI capability, `pci::msi`) or INTA through the `_PRT`.
  - **ATA disk** (`storage::ata`): IDENTIFY, which reports a solid-state disk with TRIM; DMA, PIO and multiple reads and writes, in 28- and 48-bit forms; FLUSH; DATA SET MANAGEMENT (TRIM); and the power commands.
  - **ATAPI DVD-ROM** (`storage::atapi`): the MMC commands Windows' cdrom.sys and Linux's sr use, with sense data and unit attentions:
    - INQUIRY, TEST UNIT READY, REQUEST SENSE;
    - READ CAPACITY, READ(10)/(12);
    - READ TOC (LBA and MSF, sessions);
    - GET CONFIGURATION (the CD or DVD profile by disc size);
    - GET EVENT STATUS NOTIFICATION, MODE SENSE (capabilities page);
    - disc and track information, and the rest.
  - **vmmc-vmx:** `--cdrom ISO` and `--disk PATH,sata`.
  - **Tests:** 30 storage unit tests, and the test initramfs now carries `ahci`, `libahci` and `isofs` (skipping drivers the kernel has built in).
- **M4, display and screenshots:**
  - **The adapter:** bochs-display (`devices::display::bochs`, 1234:1111, class "other display"). Its 16 MiB framebuffer BAR is mapped into the guest as RAM, and remapped whenever the BAR moves. The DISPI registers sit at BAR2+0x500, with QEMU's byte-order register at 0x600. OVMF's QemuVideoDxe drives it and sets 1280x800, which becomes the GOP framebuffer Windows' basic display driver uses.
  - **Screenshots:** `vmmc-vmx run --screenshot DIR` saves PNGs: `exit.png` when the VM stops, one per Ctrl-A s, and one every N seconds with `--screenshot-every N`. The firmware test checks the shell's last screen: 1280x800, with text on it.
  - **Platform fix:** direct boot now uses OVMF's Q35 layout as well:
    - ECAM at 0xE0000000;
    - 32-bit BARs from max(low RAM, 2 GiB) up to the ECAM, with low RAM capped at 0xB0000000.

    Before this, the ACPI 32-bit window started above the ECAM while OVMF put its BARs below it. Both boot paths now report 0x80000000-0xDFFFFFFF, and the window stretches over any BAR the firmware puts elsewhere.
- **M5, the unattended Windows install (passes with Windows 10 22H2; a Windows 11 ISO is still to be tried):** `cargo xtask win11-test` needs `VMMC_WIN11_ISO`, and `VMMC_WIN11_IMAGE` for an edition other than Windows 11 Pro.
  - **Answer disc:** an ISO with `autounattend.xml` (GPT on the NVMe disk, the edition's public generic key, a local administrator, no online-account OOBE, and LabConfig bypasses until M7) and a first-logon PowerShell script. The script reports to COM1: CPUs, Secure Boot, TPM, disk bus, network, and any device without a working driver. Then it shuts down.
  - **The run:** a 64 GiB sparse disk on NVMe, an e1000e NIC, a screenshot every 30 s, and a key press for the disc's "Press any key to boot from CD or DVD" prompt. Setup runs in the ISO's language, read from its `sources/lang.ini` (or `VMMC_WIN11_LANG`).
  - **Result:** Windows 10 Pro 22H2 (German ISO) installs in 3 to 6 minutes, with Secure Boot on and a TPM 2.0 since M7, and reports 4 CPUs, 8 GB, NVMe, the vmmc SMBIOS model, the USB tablet, and a DHCP address; it fetches a page from xtask through the NAT gateway (`net=ok`, a required marker). An internet check (www.microsoft.com:443) is reported too but not required: it depends on the host. Open item: the display adapter has no Windows driver (Windows draws through the firmware framebuffer instead).
  - **Platform fixes Windows needed:**
    - **RTC periodic interrupt on IRQ 8.** With no HPET, Windows builds its clock on it; without it the HAL stopped with HAL_INITIALIZATION_FAILED (0x5C).
    - **Legacy devices behind the LPC bridge.** COM1, the keyboard controller and the RTC moved to `\_SB.PCI0.ISA_`. As siblings of PCI0 their ports overlapped its I/O window, and Windows refused to start the root bridge (problem 12), so no PCI device got a driver.
    - **CPU topology.** WHP's CPUID makes every vCPU its own package, and Windows Pro licenses two sockets. vmmc now answers leaves 1, 4, 0xB and 0x1F itself: one package with every vCPU a core. It also sets ARAT in leaf 6.
    - **e1000e IP checksum offload.** Windows asks for the IPv4 header checksum (POPTS.IXSM) with the context's IPv4 bit clear; the device only honored IXSM with that bit set, so every packet Windows sent had a bad checksum and the NAT dropped it, DHCP included.
    - **TCP to the DNS address.** Windows also tries DNS over TCP. The NAT used to forward that to the real 10.0.2.3; it now resets it at once, and the guest stays with UDP.
  - **Debugging aids:** Ctrl-A r prints each vCPU's registers, local APIC and code around RIP; Ctrl-A m saves guest RAM and vCPU state for offline analysis.
- **M6, networking (verified with Linux and Windows):**
  - **NAT** (`crates/net`, on smoltcp, 0BSD):
    - the guest is 10.0.2.15 behind gateway .2, with DNS at .3;
    - a DHCP server, and DNS answered with the host's own resolver;
    - TCP terminated in smoltcp (AnyIP) and proxied to host sockets, with backpressure;
    - UDP flows through host sockets;
    - the gateway address reaches the host's loopback.

    Tests run with a second smoltcp stack as the guest: DHCP, a 300 KB TCP echo, 2 MB into a NIC that holds 16 frames (none may be dropped), connection refused, DNS, and UDP.
  - **virtio-net**, with no offloads.
  - **e1000e (82574L)**, written from the datasheet:
    - EEPROM with a valid checksum, PHY over MDIC;
    - legacy and extended receive descriptors;
    - both transmit queues, with context descriptors, checksum insertion and TSO in software;
    - MSI or INTA, with ICR auto-mask.

    Linux's own `e1000e` driver identifies it ("MAC: 3, PHY: 8"), brings the link up at 1 Gb/s, and passes its MSI self-test.
  - **vmmc-vmx:** `--nic virtio|e1000e[,mac=...]`. The MAC is kept in the VM directory.
  - **Tests:** the direct Linux test runs DHCP, HTTP and DNS over virtio-net; the UEFI Linux test runs the same over e1000e.
  - **Bugs found:**
    - The NAT treated SYN-RECEIVED as "guest closed" and shut the host side.
    - The e1000e raised a link change when the driver unmasked it, rather than latching it at link-up. That broke Linux's MSI self-test half the time.
    - **Windows' link went down every 10.7 s.** The 82574 has two transmit queues and Windows' driver uses both; the e1000e only ran the first. Packets on the second were never sent, so the driver took the queue for hung and reset the adapter. Each reset took the link down and up. Browsers then failed with `ERR_NETWORK_CHANGED` and pages loaded without their CSS. Found with the register trace (`VMMC_LOG=devices::nic::e1000e=trace`): the driver wrote TDT1. With both queues the link stays up, and a 50 MB download runs at the host's speed.
    - The PHY's autonegotiation expansion register read 0, so Windows' driver warned at every link-up that the link partner didn't autonegotiate.
    - **Every guest reset leaked the guest's RAM.** The e1000e kept its NAT in the state that the NAT's own thread holds, so after a reset rebuilt the machine, neither could be freed, and the old guest memory went with them. That's 8 GB per reboot for an 8 GB Windows, and Setup reboots several times. The NAT now hangs off the device itself. Measured across three resets from the UEFI shell with 2 GB of RAM: 2 GB, 4 GB, 6 GB, 8 GB committed before; 2 GB throughout after.
    - **Flow control from the NIC to the NAT.** The NAT sent TCP data to the guest as fast as smoltcp allowed, and a NIC with no free receive buffers dropped what didn't fit. Each loss stalls the connection for smoltcp's 1 s minimum retransmission timeout. Now a NIC with frames waiting tells the NAT it's full (`FrameSink::has_room`), and the NAT holds data in its TCP buffers until the NIC wakes it.
  - **Host note:** the workspace's A: volume rescans a whole file on its first write after it's opened, at about 1 MB/s. Guest disk writes stalled 30-60 s and looked like lost interrupts. xtask now keeps guest-written images in `%TEMP%\vmmc-test-images` (or `VMMC_TEST_IMAGES`).
- **M7, TPM 2.0 and Secure Boot:**
  - **The TPM engine** (`crates/tpm`): Microsoft's reference implementation (ms-tpm-20-ref v1.83r1) with OpenSSL 3.6, built by the `native-deps` workflow as static MSVC libraries and linked when `fetch-native` has fetched them (a stub otherwise).
    - Built as a device, not a simulator: `SIMULATION`, forced failure mode and crypto-library reporting are off.
    - Its entropy comes from `BCryptGenRandom`. The reference platform's own source is `rand()` seeded with the time and process ID, which would make every seed and key predictable.
    - Its NV memory (16 KiB) lives in the VM directory as `tpm-state.bin`, written through a temporary file whenever a command changes it. A missing file means a newly manufactured TPM.
    - **An endorsement key certificate**, as a TPM vendor provides. At manufacture, vmmc creates TCG's default RSA 2048 EK, builds an X.509 certificate for it (EK Credential Profile: the TPM's manufacturer, model and version in a critical alternative name, and the EK certificate purpose), signs it with an ECDSA P-256 key the TPM derives from its platform seed, and writes it to NV index 0x01C00002 under platform authorization, locked. The DER is written by hand and the TPM does the signing, so there's no new crypto dependency; OpenSSL parses the result. Without it, Windows calls the TPM not ready for attestation.
    - The OpenSSL glue reads OpenSSL's private bignum layout and only allows versions it was tested with. CI checks the layout in the OpenSSL source it built before letting it through.
  - **The device** (`devices::tpm`): a CRB interface at 0xFED40000 (locality 0), described by an ACPI TPM2 table (start method 7) and an MSFT0101 device. Each machine build is a platform reset (`_TPM_Init`), so the firmware's TPM2_Startup works after a guest reboot.
  - **Physical presence:** the device's `_DSM` implements the TCG Physical Presence Interface 1.3 and the memory-clear interface with nothing behind them: no request is ever pending and every operation is reported as not implemented, since the firmware doesn't carry them out. Windows' TPM WMI provider fails its physical-presence queries without them; Linux reports the version (`ppi=1.3` in the UEFI Linux test).
  - **Secure Boot:** new VMs start from `OVMF_VARS.secboot.fd` (Microsoft's 2011 and 2023 keys), so Secure Boot is on by default. The Windows answer file lost its LabConfig bypasses.
  - **vmmc-vmx:** `--tpm`.
  - **Temporary choice: the manufacturer ID is "MSFT"; revisit before release.** The reference implementation reports "XYZ ", a placeholder. Windows' TPM WMI provider can't format a firmware version for a manufacturer it doesn't know, so `Get-Tpm` fails (`Win32_Tpm.get_ManufacturerVersion`, NTE_NOT_FOUND) while BitLocker-style use works. In the Windows test, "MSFT" passes everything and an ID of vmmc's own ("VMMC") fails like "XYZ ". The code is Microsoft's reference TPM, which Hyper-V's vTPM also reports as MSFT; the EK certificate's issuer still names vmmc. Decided 2026-10-13 as a stopgap. To change it, edit the "Set the TPM's manufacturer ID" step in `.github/workflows/native-deps.yml` (it patches `VendorInfo.c`), publish a new release, and pin it in `xtask/src/native.rs`. TPMs made earlier keep the old ID in their EK certificate.
  - **Tests:** the TPM crate manufactures a TPM (and checks its EK certificate against the EK the guest makes), runs Startup and GetRandom, resets, and reopens it from its state file. The UEFI Linux test passes `--tpm` and checks that the kernel finds a TPM 2.0 and that PCR 0 holds OVMF's measurements (`tpm=2 pcr0=measured`). The Ubuntu install boots shim, GRUB and the kernel with Secure Boot on.
- **M8, input:**
  - **i8042** (`devices::legacy::i8042`), rewritten as a controller with two ports:
    - a keyboard that takes keys from the host as scan code set 1 and sends set 1 or set 2, with the controller's set 2 to set 1 translation, LEDs, typematic and identify;
    - a PS/2 mouse on IRQ 12 (`\_SB.PCI0.ISA_.PS2M`, PNP0F13) that becomes an IntelliMouse with a wheel when the guest knocks (sample rates 200, 100, 80);
    - a disabled port keeps its bytes until it's enabled again.

    9 unit tests.
  - **xHCI** (`devices::usb::xhci`, 1b36:000d at 00:04.0), written from Intel's xHCI specification:
    - command, event and transfer rings, with slot and endpoint contexts;
    - two USB 2 ports and two USB 3 ports, with their protocol capabilities;
    - 16 device slots, and one MSI-X vector.
  - **USB HID devices** (`devices::usb::hid`): a tablet (absolute pointer, 0 to 32767 on both axes, three buttons and a wheel) and a boot-protocol keyboard, behind the `UsbDevice` trait.
  - **Bugs found with Windows** (its driver set the controller up in an order Linux doesn't):
    - **A controller reset (HCRST) also reset MSI-X**, turning it off and masking the vector. Windows enables MSI-X first and resets the controller after, so its interrupts went nowhere: every port reset timed out and the devices showed as "Unknown USB device (port reset failed)". MSI-X is PCI state and now survives a controller reset.
    - **No INTx.** The controller only signalled through MSI-X. It now has INTA too: with MSI-X off, an event sets IMAN.IP and the line stays asserted while the interrupt is pending and enabled.
  - **Tests:** 11 USB unit tests drive the controller the way a driver does, from reset to interrupt transfers, including INTA and Windows' MSI-X order. OVMF enumerates both devices. The direct Linux test loads `xhci_pci` and `usbhid`, and checks that both appear (`usb=tablet,keyboard`). The Windows test checks that Windows' own driver has the tablet working (`usb-tablet=True`).
  - **Known:** the USB IDs (QEMU's xHCI ID, and pid.codes test IDs 1209:0001 and 1209:0002 for the HID devices) need replacing with vmmc's own before release, as NVMe's do; so does the EDID's manufacturer ID ("VMC", unregistered).
- **M9, the console window:** `vmmc-vmx run --display window`.
  - **Screen:** Slint with its software renderer, on winit. A capture thread checks the framebuffer up to 60 times a second and converts and hands over a picture only when it changed. The window opens at the guest's resolution, one guest pixel per screen pixel, follows the guest's mode changes, and scales down if made smaller.
  - **Resolution:** the display adapter now has an EDID (`devices::display::edid`, written from VESA's E-EDID 1.4) whose preferred mode is the VM's resolution, with a CVT reduced-blanking timing. OVMF boots in it, Linux's bochs-drm prefers it, and Windows, which has no driver for the adapter yet, keeps the firmware's mode. `--resolution WxH` sets it; with a window the default is the largest standard resolution that fits on the screen, without one 1280x800. The framebuffer grew to 32 MiB, enough for 3840x2160.
  - **Pointer:** the USB tablet. Clicks and the wheel go through; the position maps through the scaled picture, bands included. Nothing is captured.
  - **Keyboard:** a low-level keyboard hook on its own thread sends physical scan codes to the PS/2 keyboard while a window of the process is in front. Windows, Alt+Tab and the like reach the guest. The hook only queues keys and another thread delivers them, because Windows silently removes a hook that's slow to answer. When another window takes the keyboard, the guest gets key-ups for every key it still holds. Ctrl+Alt+Del never reaches a hook, so the toolbar sends it.
    - Two Windows behaviours stopped the hook. First, while a process reads the keyboard as raw input, Windows doesn't call that process's low-level hooks for keys typed into its own windows. winit registers raw keyboard input at start-up, so the window takes that registration back: we don't use winit's device events. Second, a read left pending on a console in raw mode stops the hook too, so the console is read by waiting on its handle.
  - **Toolbar:** Ctrl+Alt+Del, Power button (ACPI), Reset, Screenshot (with `--screenshot`), and About with the `AboutSlint` widget that Slint's royalty-free licence asks for.
  - **Lifetime:** closing the window stops the VM, and a VM that stops closes the window. The terminal remains the serial console.
  - **Verified:** OVMF's screen in the window, and an Enter typed into the window opens OVMF's boot manager. The hook's routing has unit tests. A line typed on the PS/2 keyboard reaches Linux's console (the direct Linux test, through the console's Ctrl-A k typing mode). OVMF and Ubuntu's login screen come up at the EDID's resolution (1920x1080 tried).
  - **Fixed after first use (2026-10-13):** the window was very laggy in dev builds. Converting a 1280x800 frame takes 36 ms unoptimized, and the window did that on its UI thread every 16 ms whether or not the picture had changed, so input waited behind it; Slint's renderer was unoptimized too. Capture moved to its own thread and only converts changes, and dev builds now optimize dependencies. Guests also got no EDID, so Windows stayed in the firmware's 1280x800 and the window scaled that. And typed keys never reached the guest: while the terminal's console is in raw mode with a thread blocked reading it (vmmc-vmx's serial console input), Windows doesn't call the process's low-level keyboard hook for input from outside the process. Bisected with keys injected from another process: 0 of 6 runs reached the hook, 4 of 4 without the console reader or without raw mode. The console is now read as input events after waiting on its handle (`ReadConsoleInputW`), so no read is left pending, and every key reaches the hook. When the window closes, vmmc-vmx prints what the hook saw (key events, how many with the window in front, how many delivered) to make the next such problem quick to place.
  - **Licences:** `deny.toml` allows `LicenseRef-Slint-Royalty-free-2.0` (Slint is GPL-3.0 or royalty-free; vmmc takes the royalty-free licence) and BSL-1.0 (the clipboard crates under winit). It ignores RUSTSEC-2026-0192 (ttf-parser unmaintained), which only reaches winit's Wayland title bars.
- **M10, Ubuntu Desktop install (passes with Ubuntu 26.04.1):** `cargo xtask ubuntu-desktop-test`.
  - **Inputs:** the 26.04.1 desktop ISO (6.5 GB), downloaded once into the test image directory and checked against Ubuntu's published SHA-256 (or `VMMC_UBUNTU_ISO`).
  - **Autoinstall:** a second disc labelled CIDATA holds the configuration, which cloud-init reads as a NoCloud source. xtask opens GRUB's command line over the serial console and boots the ISO's kernel with `autoinstall` (no confirmation prompt) and a serial console added.
  - **Install:** offline, onto a 64 GiB NVMe disk, with Secure Boot on. The late commands install a first-boot service, then the installer powers off. It takes about 4 minutes.
  - **First boot:** from the disk alone, with an e1000e adapter added. The service reports kernel 7.0, 4 CPUs, Secure Boot on, root on NVMe, the `bochs-drm` display driver, GNOME's display manager running, a page fetched from the host through the NAT, and no failed units. Then it powers off.
  - **Bug found: ARP answers for the guest's own address.** smoltcp's AnyIP mode, which the NAT uses to terminate guest TCP, answers ARP for any address. NetworkManager checks a new DHCP address with ARP (RFC 5227), took the answer for a conflict, and left the interface without routes. The NAT now passes ARP to smoltcp only for its own addresses. The earlier tests missed it: busybox's `udhcpc` doesn't check its address, and Windows' check wasn't tripped by it.
- **M10, Hyper-V enlightenments (on by default since 2026-10-14; `--no-enlightenments` hides them):** WHP exposes Hyper-V's paravirtual interfaces, which the Windows hypervisor implements itself, so they cost no exits: the hypervisor CPUID leaves, hypercalls, VP index and runtime, the reference counter and TSC page, frequency MSRs, the guest idle MSR, the synthetic interrupt controller with direct-mode synthetic timers, the APIC-access MSRs, and the TLB-flush and IPI hypercalls.
  - **Found:** Windows needs the guest idle MSR whenever it sees the hypervisor. Without it, Windows 10 stops with KMODE_EXCEPTION_NOT_HANDLED during boot; bisecting the feature set over Windows Setup boots found it.
  - **Verified:** a WHP test checks that the guest sees "Microsoft Hv" only with the option. The Windows install test passes with it on (apart from the `Get-Tpm` check), and Windows reports a hypervisor.
  - **Measured (`cargo xtask win-perf`, 2026-10-14):** installs were too noisy to compare, so the comparison boots an installed Windows 10 three times each way (release build, 4 vCPUs, 8 GB). A task the test's first logon registers reports at every boot to COM1: uptime, and timings of counter reads, `Sleep(1)`, wake-ups between threads, hashing and touching new memory. The host adds boot time, the VM's CPU use over a minute at the logon screen, and shutdown time. Medians:

    | | without | with |
    |---|---|---|
    | host CPU, idle Windows | 25.8 % of a core | 1.9 % |
    | `Sleep(1)` | 22.3 ms | 15.6 ms (Windows' tick) |
    | boot to the report | 8.2 s | 7.7 s |
    | shutdown | 3.3 s | 3.1 s |
    | counter read, wake-up, hashing, new memory | the same | the same |

    Without them, Windows builds its clock on the RTC's periodic interrupt: each tick goes through a host thread and costs vCPU 0 several exits (a thread profile of an idle VM showed vCPU 0 and `rtc-irq8` as nearly all its CPU use), and the host thread's timing makes the tick late. With them, the synthetic timers run in the hypervisor. Windows 10 passes the full install test with them (Secure Boot, TPM, input, network), as do the Linux tests, where Linux switches to the TSC page clock, IPI hypercalls and paravirtual spinlocks. So they're on by default.
  - **Still worth doing without them:** the RTC's periodic interrupt is costly and late. A guest that doesn't use Hyper-V's interfaces (an older OS, or `--no-enlightenments`) would gain from an HPET, or from timing the RTC with a high-resolution host timer.
- **M11, a Windows display driver (works; test-signed):** `guest-drivers/windows/display`, a WDDM display-only driver (KMDOD) for the display adapter, written from Microsoft's documentation of the interface (not from its MS-PL sample).
  - **What it does:** offers the standard resolutions that fit the 32 MiB framebuffer (640x480 to 3840x2160) plus the EDID's preferred one, reports the EDID as the monitor's, sets the mode Windows commits through DISPI, and copies each present's changed rectangles from Windows' system-memory picture into the framebuffer. No hardware cursor: Windows draws it. For the blue screen, the system-display DDIs write straight to the framebuffer, and on a driver stop it hands the current mode to Windows' basic display driver.
  - **Build:** `cargo xtask guest-drivers` calls MSVC's compiler and linker and the WDK's `stampinf`, `Inf2Cat` and `signtool` directly. The installed WDK (10.0.28000) ships MSBuild tasks only for Visual Studio 2026, while its Visual Studio integration sits in 2022, so its MSBuild route failed. Signed with a self-signed test certificate kept in the user's store.
  - **Test (`cargo xtask win-display-test`, passes in about 4 minutes):** installs Windows 10 with Secure Boot off; Setup's specialize pass turns test signing on and trusts the certificate; the first logon installs the driver with `pnputil`, lists the modes, switches to 1600x900, and the host checks its screenshot is 1600x900 with a picture.
  - **Found on the way:**
    - Windows refuses target modes whose frequencies a display-only driver specifies (`STATUS_GRAPHICS_INVALID_FREQUENCY`), so the VidPN never had a working mode: no commit, no present. Display-only drivers leave the signal's frequencies unspecified.
    - One refused mode used to fail the whole enumeration; refused modes are now skipped.
    - The first-logon script was denied importing the certificate, and `pnputil` in the specialize pass ran without installing anything. Each step now runs where it works.
    - Test builds trace every DDI call to the debug console (`--firmware-log`), which found the above.
  - **The desktop follows the window (2026-10-14):** resizing vmmc's window (or Ctrl-A z WxH on the console) asks the guest for that size through vmmc's registers in the adapter's BAR 2 (0x700: "VMMC", the requested size and a count of requests); the EDID prefers it from the next boot. In Windows, only a program in the user's session can change the desktop's resolution, so the driver comes with a helper, `vmmcdisplay.exe`. The INF installs it as a service too, which starts it in each user's session at logon (a Run key would have been simpler, but the INF rules for drivers Microsoft signs, which `InfVerif /w` checks in every build, allow a package only its own registry keys). It asks the driver for the host's request through a private escape and calls `ChangeDisplaySettingsEx`. A size Windows hasn't offered yet fails with DISP_CHANGE_BADMODE until the driver reconnects the monitor (`DxgkCbIndicateChildStatus`), which makes Windows read the modes again, the requested size among them: one flicker.
  - **A hardware cursor (2026-10-14):** the driver reports pointer support, converts Windows' cursor images (monochrome, colour and masked colour) to straight-alpha BGRA at the end of the framebuffer, and tells the adapter its size, hot spot and whether it shows. vmmc's window hides the host's arrow over the screen and draws the guest's image at the host's pointer, scaled with the picture, so the cursor no longer lags behind the guest's frames.
  - **Test:** the display test also asks for 1500x844 through the console and checks the desktop follows, then boots a second time, logged on by itself, moves the mouse from a logon script and checks the driver got Windows' pointer and the service started the helper. (Windows hides the pointer, and hands the driver nothing, until the mouse first moves.)
  - **Linux:** the kernel's `bochs-drm` runs the adapter. Resolutions are chosen in the guest; following the window would need a hot-plug signal bochs-drm doesn't have (virtio-gpu has one), so the window's size reaches Linux only as the EDID's preference at the next boot. Phase 4's virtio-gpu, which Ubuntu VMs get, follows the window.
  - **Before release:** see [RELEASE-BLOCKERS.md](RELEASE-BLOCKERS.md): Microsoft's attestation signature, and vmmc's own PCI IDs in place of 1234:1111.

### Phase 3: Desktop app alpha + VMware migration (about 3–4 months): done, but for the run-through by hand (2026-10-24)

Plan (2026-10-14), in milestones that each end verified on WHP: VM files and VMDK (M1), VMware import (M2), the worker process with IPC and a sandbox (M3), the desktop app (M4), snapshots and suspend/resume (M5), clones, disk maintenance and OVF/OVA (M6), the CLI and docs (M7). Decisions: migration first; VMware VMs open in place behind a protective snapshot (vmmc never writes VMware's files); full snapshots with memory.

- **M1, VM files and VMDK: done (2026-10-14).**
  - `crates/vmconfig`: a VM is a folder with a `.vmmc` file (TOML: name, UUID, CPUs, memory, firmware, TPM, disks, CD-ROMs, NICs, display, notes, times), `nvram.fd`, `tpm-state.bin` and its disks; the library (`%APPDATA%\vmmc\library.toml`) lists VMs in folders; OS presets; creating a VM, or adopting an old `--vm-dir`.
  - `crates/disk` reads and writes VMDKs, written from VMware's Virtual Disk Format spec: descriptors (UTF-8 or Windows-1252), hosted sparse extents (grain directory and tables, the redundant copy kept in step, grains allocated as written), split, flat and zero extents, stream-optimized (read), and delta chains checked by the parent's CID. The CID changes on the first write. Extents are opened without shared write access, so VMware can't open a disk vmmc has open. New disks are monolithic sparse, laid out as VMware lays them out.
  - A disk's format comes from its name (`.vmdk`), never its contents: a guest can write a VMDK header into a raw image, whose extents could then name host files.
  - `vmmc-vmx run --vm FILE`; `vmmc-cli create | list | add | remove | disk create | info | convert`.
  - **Tests:** unit tests for parsing, allocation, both tables, deltas, CIDs, concurrency, and hostile headers. `cargo xtask vmdk-compat-test` has VMware's `vmware-vdiskmanager` make every disk type, vmmc writes to them, and VMware finds them consistent (`-R`) and converts them to disks holding exactly vmmc's data; the same for vmmc's own disks and deltas (`-e` on the chain), and vmmc reads VMware's stream-optimized copies. The Linux, UEFI and Windows install tests run on VMDKs.
- **M2, VMware import: done (2026-10-14).**
  - `vmmc-cli import X.vmx`: the `.vmx` (with `|XX` escapes) becomes a `.vmmc` in a `vmmc` folder beside it, with a delta on each of VMware's disks. It maps CPUs, memory, NVMe/SATA (SCSI and IDE onto SATA, with a warning) disks, CD images, NICs (as e1000e, keeping their MACs), Secure Boot, the TPM (a new one) and the SMBIOS UUID (VMware keeps it in SMBIOS byte order). It refuses encrypted, BIOS, suspended and open VMs, and warns about what doesn't carry over (sound, 3D, shared folders, bridged networking, nested virtualization), re-activation, and BitLocker volumes it finds.
  - **Test:** `cargo xtask vmware-import-test` imports a real VMware Workstation VM (Windows 10 22H2, hardware version 22, two 64 GB split sparse disks on NVMe and SATA, EFI, e1000e) with vmmc's files outside the VMware folder, boots it headless, and waits for the guest to take a DHCP address from vmmc's NAT: 78 s, at its desktop. The power button shuts it down, and afterwards every file in the VMware folder has its size and time (and the small ones their contents) unchanged.
  - Found on the way: VMware Tools in the guest stops with an error at logon (it uses VMware's private hypervisor interface), so the import says to uninstall it after moving; and vmmc-vmx wrote colour codes into redirected logs, which it no longer does off a terminal.
- **M3, worker process, IPC and sandbox: done (2026-10-14).**
  - **Spike first:** WHP partitions work in a sandboxed process: at low integrity, and with a restricted token (administrator groups deny-only, restricting SIDs) at low integrity, the test Linux booted and powered off as usual.
  - `vmmc-vmx serve --vm FILE` runs a VM headless and serves a named pipe, `\\.\pipe\vmmc-<uuid>` (first instance only, so a squatter is refused, and no remote clients). `crates/ipc`: requests (power button, stop, reset, PS/2 keys, tablet pointer, size requests, status, the screen) as postcard messages with a version handshake; the guest's picture and hardware cursor in a shared section with seqlocks, which a client maps by duplicating its handle out of the worker.
  - `vmmc-client` starts workers sandboxed: the user's token, restricted (administrator groups deny-only, all privileges but traversal dropped, restricting SIDs: the user, Everyone, Users, Authenticated Users, the logon session) at low integrity; in a job allowing no child processes and no desktop, clipboard or other windows' handles; inheriting exactly its log and a null stdin. The VM's folder is labeled low integrity so the worker can write there and nowhere else; on drives where the user has Modify but not Full Control, vmmc first grants the user WRITE_OWNER on the folder (which labeling needs; the owner may). Writable disks outside the folder are refused while sandboxed, since labeling them would let a compromised worker that edited its config get write access to any file.
  - `vmmc-cli start | stop [--hard] | reset | status`, and `list` shows which VMs run.
  - **Test:** `cargo xtask worker-test` starts the test Linux through `vmmc_client::start`, checks the worker reports low integrity and a restricted token, maps its shared screen, opens the firmware's boot menu and starts the shell with key presses through the pipe, and waits for the guest's checks (NVMe round trip on the VM's own VMDK, TPM, network, DNS) and its power-off, which ends the worker. A second VM idles in the firmware; a second start is refused and a hard stop ends it.
  - Found on the way: Windows refuses to flush a file handle opened for reading, so a read-only disk on NVMe or virtio failed the firmware's FLUSH commands; flushing a read-only disk is now a no-op.
- **M4, the desktop app: in progress (2026-10-15).**
  - **Easy Install:** `crates/media` writes Windows' `autounattend.xml` and Ubuntu's autoinstall (SHA-512-crypt password hashes, written from the spec and checked against its test vector) onto an answer disc in the VM's folder; the config's `[install]` section lists what to type on the serial console at the first boot (Windows' "Press any key to boot from CD or DVD"; Ubuntu's GRUB command line, adding `autoinstall`), which the worker does by watching the guest's serial output. Done steps leave the config, so later boots don't reinstall; at the end (Windows' first restart, Ubuntu's power-off) the install discs come out, the answer disc (which holds the password) is deleted, and the guest starts from its disk.
  - **Test:** `cargo xtask easy-install-test` creates both VMs with `create_vm`, as the app does, in sandboxed workers: Windows 10 installed and reached its desktop (a first-logon command reports on COM1) in 288 s; Ubuntu 26.04 installed, ejected its discs and booted from its disk in 541 s.
  - **The app** (`bins/vmmc`, Slint): the library with each VM's state, open VMs as tabs, a VM's page (hardware, times, notes, origin; start, shut down, power off, reset, Ctrl+Alt+Del), the console (the worker's shared screen and hardware cursor, the tablet, the keyboard through `crates/keyhook` while the console has it, Ctrl+Alt to release, full screen, fitting the guest to the window), the New VM wizard with Easy Install, VMware import, and settings. Input goes down the pipe from a thread of its own. A test renders every state offscreen with Slint's software renderer.
  - Since: folders in the library; settings for the disks (add, remove, expand) and the network adapter (2026-10-24). Still to do: going through [MANUAL-CHECKS.md](MANUAL-CHECKS.md) by hand.
- **M5, snapshots and suspend/resume: done (2026-10-23).**
  - **Saved state:** with the machine running, every vCPU parks and saves its state, the guest's time and the devices' clock stop, and every device saves its state (the legacy chipset, every PCI device, flash, fw_cfg, the TPM). A state file holds a header (postcard, LZ4) and the RAM's non-zero pages, compressed on several threads. A machine built from the same configuration starts from it, the guest's time going on from the saved moment.
  - **Snapshots:** the tree is in `snapshots/snapshots.toml`. A snapshot of a running VM pauses it, saves its state, moves its disks to new deltas (`disk::Switchable`, keeping NVMe and AHCI identities from the bottom of the chain), and commits. A revert puts new deltas on the snapshot's images, restores its config, UEFI variables and TPM, and starts from its state. Suspend writes `suspended.state`, and the next start resumes from it. Deleting a snapshot merges what nothing needs any more, as VMware does: an image neither the VM nor a snapshot uses, with one delta of vmmc's on top, takes that delta's data and its place (at once with the VM off, or when its worker stops); a VMware VM's own disks are never written. Through the worker's pipe, `vmmc-cli snapshot create | list | revert | delete` and `suspend`, and the app: a VM's page lists its snapshots as a tree, with Take snapshot, Revert to it and Delete, and the toolbar has Suspend.
  - **Tests:** `cargo xtask snapshot-test` saves the test Linux while it runs (paused ~300 ms, 69 MiB) and restores it in a new process with its RAM, timers, disk and network working. `win-snapshot-test` takes a snapshot of Windows 10 (Secure Boot, TPM, Hyper-V enlightenments, 4 vCPUs, 8 GiB) through the worker's pipe as the app does (paused 5.9 s), reverts it to the snapshot (a new machine from its state), suspends it (4.6 GB) and resumes it in a new process; the power button then shuts it down cleanly. This retires risk C2. `worker-test` snapshots, reverts, suspends and resumes in the sandboxed worker. `vmdk-compat-test` has VMware's vdiskmanager check a chain vmmc merged.
- **M6, clones, disk maintenance, OVF/OVA: done (2026-10-24).**
  - **Clones** (`vmconfig::clone`): of a VM as it is (off) or of a snapshot. A full clone copies each chain of disks into one new monolithic sparse disk; a linked clone of a snapshot gets a delta on each of its images, and the snapshot lists the clone and can't be deleted while it's there. A clone gets a new UUID and MAC, its own UEFI variables and TPM, and no memory. In the app (Clone…) and `vmmc-cli clone`.
  - **Disk maintenance:** `disk::expand` and `disk::compact` write a monolithic sparse disk anew (its tables ahead of its grains, as VMware's checker insists), at a bigger size or without its grains of zeroes, keeping its disk database. Not for deltas or VMs with snapshots. In the app's settings (expand) and `vmmc-cli disk expand | compact`.
  - **OVF/OVA** (`vmconfig::ovf`, `vmconfig::tar`): export writes the VM's disks as stream-optimized VMDKs (`disk::write_stream_optimized`), the `.ovf` as VMware writes it, and a SHA-256 manifest, tarred into an `.ova`. Import reads OVF 1.x and 2.x, checks every file against the manifest (SHA-256, SHA-1, SHA-512), takes the first virtual system's hardware, and converts the disks into a new VM's folder; SCSI and IDE disks go on SATA, other NICs become e1000e, BIOS VMs are refused. In the app (Import VM, Export…) and `vmmc-cli import | export`.
  - **VMware imports** record VMware's disks as the VM's first snapshot, "Imported from VMware", as the plan had it: reverting to it goes back to them (the new deltas in vmmc's folder, as for any image outside the VM's folder), and its deltas are named as snapshots' are, so reverts and merges tidy them.
  - **Tests:** `cargo xtask clone-test` boots a linked and a full clone of the test Linux in sandboxed workers. `ova-test` exports the test Linux, imports and boots it; then through VMware's ovftool, which makes a VMware VM of vmmc's OVA and an OVA of that VM in VMware's own words, which vmmc imports and boots. `vmdk-compat-test` has vdiskmanager check disks vmmc expanded and compacted, and copy a stream-optimized disk vmmc wrote.
  - Found on the way: an expanded disk whose grain directory moved to the end of the file is "corrupted and cannot be repaired" to VMware, so resizing writes the disk anew; grain tables vmmc allocates on demand at the end of a file (only for disks whose directory has empty entries, which neither VMware nor vmmc makes) are "corrupted" to it too, and are left as they are for now.
- **M7, the command line, docs and the exit: done (2026-10-24).**
  - `vmmc-cli` does all of it: `create`, `import` (`.vmx`, `.ova`, `.ovf`), `export`, `list`, `start`, `stop`, `reset`, `status`, `suspend`, `tools`, `snapshot create | list | revert | delete`, `clone`, `disk create | info | convert | expand | compact`, `add`, `remove`.
  - The README has the app, a guide to moving over from VMware, and the command line; [MANUAL-CHECKS.md](MANUAL-CHECKS.md) has what to check in the app by hand; [RELEASE-BLOCKERS.md](RELEASE-BLOCKERS.md) has what Phase 3 left for a release.
  - **The exit criteria, on WHP:** `easy-install-test` (Windows 10 running from its disk after 284 s, Ubuntu 26.04 after 787 s, in sandboxed workers; it had stopped after the install since M5, fixed), `worker-test`, `snapshot-test`, `win-snapshot-test` (snapshot, revert, suspend, resume of Windows), `clone-test`, `ova-test` (through VMware's ovftool too), `vmdk-compat-test`, and the app's UI snapshots all pass. The VMware VM `vmware-import-test` uses opens with its files unchanged and boots Windows to its logon, but VMware ran it twice on 2026-10-17 and powered it off seconds into its boot (its NVMe disk removed, too), so its Windows now starts in Automatic Repair first; the test passes again once it has booted cleanly in VMware.
- **Pulled ahead from Phase 4, vmmc's agent (2026-10-22).** The agent works without a driver, so it ships before Phase 4's paravirtual devices. It talks to the worker over a second serial port (COM2), and the worker sends nothing there until the agent has said hello (firmware and GRUB read COM2 as a keyboard). A drag's files go over TCP to a NAT address the worker answers itself (10.0.2.2:5640), so drag and drop needs the VM's network adapter.
  - **Windows** (`vmmcagent.exe`, on the Tools disc) and **Linux** (`vmmc-agent`, static musl, through X11; under Wayland, XWayland bridges it to Wayland programs): the clipboard's text both ways, and files and folders dragged from Explorer into the console and out of the guest to the host.
  - **Tests:** `cargo xtask win-dnd-test` (Windows 10) and `ubuntu-dnd-test` (Ubuntu Desktop under GNOME's Wayland, with a GTK 4 peer) check the clipboard both ways, a drag in that's called off, drops in before and after it, and a drag out, every file by SHA-256. The app's side of drags (OLE in `bins/vmmc`) has unit tests only, and still needs a manual run-through.
  - Found on the way: Windows' layered windows take clicks only once they've painted; GNOME's mutter shows its drop window for Wayland programs only when a drag goes onto another window than the last one did, so the Linux agent sends to its hidden window when it sees nothing under the pointer.

- **Slint app:**
  - library with folders and tabs, VM notes, timestamps;
  - New VM wizard with OS presets and Easy Install (generated `autounattend.xml` / Ubuntu autoinstall);
  - settings editor and power operations;
  - console with scaling and fullscreen, input grab and Ctrl+Alt release, a low-level keyboard hook for Win/Alt-Tab, and Send Ctrl+Alt+Del.
- **Worker isolation:** separate `vmmc-vmx` process, restricted token, shared-memory framebuffer.
- **Snapshots:**
  - snapshot tree + manager (VMDK delta chains + device state + RAM), revert, suspend/resume;
  - linked and full clones; expand and compact disks.
- **VMware import:**
  - `.vmx` → native config, VMDK opened in place, controller and NIC mapping (nvme→NVMe, sata→AHCI, vmxnet3/e1000→e1000e);
  - fresh OVMF variables;
  - warnings for BitLocker recovery (the TPM changes), Windows re-activation, and encrypted VMware VMs (decrypt in VMware first).
- **OVF/OVA:** import and export.
- **`vmmc-cli`:** list, start, stop, suspend, snapshot, revert, clone, import.

### Phase 4: Guest integration ("Tools", about 3 months)

- **Paravirtual devices:** virtio-net, virtio-gpu 2D (dynamic resize), virtio-vsock, virtio-balloon, and a virtio-fs server written for Win32 host file APIs.
- **Guest agent (Windows + Linux):** clipboard, display autofit, time sync, graceful shutdown, guest IP reporting, drag & drop file copy, shared folders. (Clipboard text and drag & drop came early, in Phase 3.)
- **Windows guest drivers:** rebuild virtio-win (BSD-3) and attestation-sign it with our EV cert via Microsoft Partner Center. Check WinFsp licensing for virtio-fs on Windows guests. Ship an MSI, plus .deb/.rpm for Linux.

Progress:

- **virtio-gpu 2D: done (2026-10-28).** `crates/devices/src/virtio/gpu.rs`, written from the virtio 1.2 spec: the control and cursor queues, 2D resources backed by guest memory (the host's copies capped at 512 MiB in all, every rectangle, size and offset checked before a copy), one scanout, the EDID, and the hardware cursor, which the console draws as it does the Windows driver's. The size the host asks for becomes the scanout's preferred mode, in the display info and the EDID, with a display event; Linux's driver marks its connector for desktops to follow such changes, and GNOME does. A snapshot saves the resources and reads their pixels back from guest memory (state format 4). `devices::display::Display` covers both adapters, so the console, screenshots and the pipe work with either.
  - **Where:** the Ubuntu preset uses it, and the app's settings choose it for any VM (`[display] adapter = "virtio-gpu"`, `vmmc-vmx --adapter virtio-gpu`). vmmc's own adapter stays the default for the rest: with virtio-gpu the firmware's screen is drawn by OVMF's VirtioGpuDxe through blits, with no linear framebuffer to hand on, so Windows (until a driver) and a Linux without `virtio_gpu` would have no screen.
  - **Test:** `cargo xtask ubuntu-gpu-test` boots ubuntu-dnd-test's Ubuntu 26.04 on virtio-gpu, brings its vmmc Tools up to the disc's, checks their agent started with the session, and asks for three sizes: GNOME follows each in 100-400 ms. Then it suspends the VM (907 MiB, 3 s) and resumes it in a new process: the picture is back at its size, and the desktop follows a size asked for after.
  - **Found on the way:** GNOME 50's session skips autostart entries that set `X-GNOME-Autostart-Phase`, so vmmc Tools' agent never started at logon on Ubuntu 26.04 (ubuntu-dnd-test runs its own agent, so it didn't notice); the entry no longer sets it. OVMF enables only the control queue, so the device serves only queues the driver enabled.

### Phase 5: Networking + device parity on Windows hosts (about 3–4 months; signed kernel drivers)

- **Bridged and host-only networking:**
  - an NDIS 6 LWF driver for bridged mode and an NDIS miniport virtual adapter for host-only;
  - a Virtual Network Editor;
  - LAN segments (an in-process switch between workers) and network simulation (bandwidth, latency, loss).
- **USB passthrough:** a filter driver + arbitration service, exposed through xHCI (USB 3.x).
- **Other devices:** host CD/DVD, serial/parallel port mapping, and LSI SAS emulation for older imported VMs.

### Phase 6: Linux host + v1.0 commercial release

- **Linux host:** `hv-kvm` backend, TAP/bridge networking, usbfs USB, PipeWire audio; Flatpak/deb/rpm packages.
- **Release engineering:** signed MSI (WiX), signed auto-update, opt-in crash reporting, and an installer that enables WHP.
- **Features:** VM encryption (XTS-AES disks + encrypted vTPM state, keys via DPAPI/Credential Manager).
- **Pro tier:** headless `vmmcd` + REST API, license enforcement for Pro, docs site, and public download with no account.

### Post-1.0 R&D

- **3D graphics:**
  - Linux guests: virtio-gpu with Venus/gfxstream.
  - Windows guests: needs a WDDM 3D driver, a multi-year effort and VMware's biggest moat. Be upfront about it in marketing.
- **Other:** Unity/seamless mode, macOS Apple Silicon host (Hypervisor.framework), ARM64 Windows host, nested virtualization, ESXi/Proxmox remote management.

---

## Top risks and mitigations

1. **WHP exit cost hurts I/O performance.** Prioritize paravirtual devices with doorbells and batch MMIO. Benchmark against VMware and VirtualBox on the same host every phase.
2. **Driver signing (guest Tools drivers and the Phase 5 host drivers).** Buy an EV code-signing cert and set up Microsoft Partner Center attestation signing before Phase 4.
3. **Migrated Windows VMs:** expect BitLocker recovery prompts, re-activation and boot-controller mismatches. Mitigate with a pre-flight import check, clear warnings and LSI SAS emulation.
4. **Market (VMware is free):** lead with trust, openness, security and frictionless migration. Monetize Pro and team features, not the basics.
5. **Legal:** trademarks, clean-room implementation and GPL contamination, all guarded by `cargo-deny` and contributor rules.

## Host prerequisites

- **Enable WHP:** run `Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform` as admin, then reboot.
- **Side effects:** this turns on the Hyper-V hypervisor, which then owns VT-x. Other software that drives VT-x directly stops working while it is on (toggle with `bcdedit /set hypervisorlaunchtype off|auto` + reboot). VMware/VirtualBox switch to their WHP mode.
- **Test hardware:** a self-hosted CI runner (this machine or a dedicated box). GitHub-hosted Windows runners can't be relied on for WHP/nested virtualization, so VM boot tests run there only.

## Verification

- **Unit tests:** register-level tests per device. Property and fuzz tests (`cargo-fuzz`) for every guest-facing parser (virtio descriptors, NVMe/AHCI commands, e1000e descriptors, VMDK headers, .vmx/OVF). This is the core security claim, so fuzzing is mandatory.
- **Boot tests (`cargo xtask boot-test`):**
  - headless runs that capture the serial console and assert markers: the Phase 0 "Hello from vmmc", the Phase 1 Linux shell prompt;
  - Phase 2: a Windows unattended install whose `FirstLogonCommands` writes `INSTALL_OK` plus Secure Boot/TPM status to COM1.
- **Import tests:** a corpus of real VMware-created VMs (Win10/11, Ubuntu; NVMe/SATA/LSI; sparse/split/flat VMDK) must import and boot.
- **Performance:** boot time, `fio`/CrystalDiskMark and `iperf3` against VMware Workstation and VirtualBox on the same host; results tracked per release.
- **GUI:** a manual checklist per phase (wizard, console input grab, snapshots, fullscreen, multi-monitor).

## Sources

- **Release notes:**
  - [Workstation 26H1u1 release notes](https://techdocs.broadcom.com/us/en/vmware-cis/desktop-hypervisors/workstation-pro/26H1/release-notes/vmware-workstation-pro-26h1u1-release-notes.html)
  - [Workstation 26H1 release notes](https://techdocs.broadcom.com/us/en/vmware-cis/desktop-hypervisors/workstation-pro/26H1/release-notes/vmware-workstation-pro-26h1-release-notes.html)
  - [VMware blog: Workstation and Fusion 26H1](https://blogs.vmware.com/cloud-foundation/2026/05/14/announcing-vmware-workstation-and-fusion-26h1/)
- **25H2 coverage:**
  - [Neowin: 25H2 is out](https://www.neowin.net/news/vmware-workstation-pro-25h2-is-out-with-new-command-line-tool-usb-32-support-and-more/)
  - [OMG! Ubuntu: 25H2 released](https://www.omgubuntu.co.uk/2025/10/vmware-workstation-pro-25h2-released)
  - [Heise: 25H2 with USB 3.2 and new virtual hardware](https://heise.de/-10778091)
- **Security:** [CVE-2026-59346 VMXNET3 escape](https://thecybersecguru.com/exploits/cve-2026-59346-vmware-vmxnet3-poc)
- **Engine references:**
  - [OpenVMM guide](https://openvmm.dev/guide/)
  - [crosvm hypervisor support](https://crosvm.dev/book/hypervisors.html)
  - [vm-memory](https://docs.rs/crate/vm-memory/latest)
- **GUI licensing:** [Slint royalty-free license](https://slint.dev/terms-and-conditions)
