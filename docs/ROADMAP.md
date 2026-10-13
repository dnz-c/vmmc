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

**Where Phase 2 stands (2026-10-13):** Ubuntu Desktop installs and runs (M10). Windows 10 22H2 installs with Secure Boot on, a TPM 2.0, its USB tablet and network all working; the Windows test's one failing check is `Get-Tpm`, which needs a decision on the TPM's manufacturer ID (see M7). Still to do: a Windows 11 ISO run, and a Windows driver for the display adapter.

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

    10 tests run with a second smoltcp stack as the guest: DHCP, a 300 KB TCP echo, connection refused, DNS, and UDP.
  - **virtio-net**, with no offloads.
  - **e1000e (82574L)**, written from the datasheet:
    - EEPROM with a valid checksum, PHY over MDIC;
    - legacy and extended receive descriptors;
    - transmit with context descriptors, checksum insertion and TSO in software;
    - MSI or INTA, with ICR auto-mask.

    Linux's own `e1000e` driver identifies it ("MAC: 3, PHY: 8"), brings the link up at 1 Gb/s, and passes its MSI self-test.
  - **vmmc-vmx:** `--nic virtio|e1000e[,mac=...]`. The MAC is kept in the VM directory.
  - **Tests:** the direct Linux test runs DHCP, HTTP and DNS over virtio-net; the UEFI Linux test runs the same over e1000e.
  - **Bugs found:**
    - The NAT treated SYN-RECEIVED as "guest closed" and shut the host side.
    - The e1000e raised a link change when the driver unmasked it, rather than latching it at link-up. That broke Linux's MSI self-test half the time.
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
  - **Open: the manufacturer ID.** The reference implementation reports manufacturer "XYZ ", a placeholder. Windows' TPM WMI provider can't format a firmware version for a manufacturer it doesn't know, so `Get-Tpm` fails (`Win32_Tpm.get_ManufacturerVersion`, NTE_NOT_FOUND) while BitLocker-style use works. Tried in the Windows test: "MSFT" passes everything, an ID of vmmc's own ("VMMC") fails like "XYZ ". Which registered ID to report is a product decision; the CI build sets it in `VendorInfo.c`.
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
  - **Known:** the USB IDs (QEMU's xHCI ID, and pid.codes test IDs 1209:0001 and 1209:0002 for the HID devices) need replacing with vmmc's own before release, as NVMe's do.
- **M9, the console window:** `vmmc-vmx run --display window`.
  - **Screen:** Slint with its software renderer, on winit. It shows the framebuffer scaled to fit, up to 60 times a second, and skips frames that haven't changed.
  - **Pointer:** the USB tablet. Clicks and the wheel go through; the position maps through the scaled picture, bands included. Nothing is captured.
  - **Keyboard:** a low-level keyboard hook on its own thread sends physical scan codes to the PS/2 keyboard while a window of the process is in front. Windows, Alt+Tab and the like reach the guest. The hook only queues keys and another thread delivers them, because Windows silently removes a hook that's slow to answer. When another window takes the keyboard, the guest gets key-ups for every key it still holds. Ctrl+Alt+Del never reaches a hook, so the toolbar sends it.
  - **Toolbar:** Ctrl+Alt+Del, Power button (ACPI), Reset, Screenshot (with `--screenshot`), and About with the `AboutSlint` widget that Slint's royalty-free licence asks for.
  - **Lifetime:** closing the window stops the VM, and a VM that stops closes the window. The terminal remains the serial console.
  - **Verified:** OVMF's screen in the window, and an Enter typed into the window opens OVMF's boot manager. The hook's routing has unit tests.
  - **Licences:** `deny.toml` allows `LicenseRef-Slint-Royalty-free-2.0` (Slint is GPL-3.0 or royalty-free; vmmc takes the royalty-free licence) and BSL-1.0 (the clipboard crates under winit). It ignores RUSTSEC-2026-0192 (ttf-parser unmaintained), which only reaches winit's Wayland title bars.
- **M10, Ubuntu Desktop install (passes with Ubuntu 26.04.1):** `cargo xtask ubuntu-desktop-test`.
  - **Inputs:** the 26.04.1 desktop ISO (6.5 GB), downloaded once into the test image directory and checked against Ubuntu's published SHA-256 (or `VMMC_UBUNTU_ISO`).
  - **Autoinstall:** a second disc labelled CIDATA holds the configuration, which cloud-init reads as a NoCloud source. xtask opens GRUB's command line over the serial console and boots the ISO's kernel with `autoinstall` (no confirmation prompt) and a serial console added.
  - **Install:** offline, onto a 64 GiB NVMe disk, with Secure Boot on. The late commands install a first-boot service, then the installer powers off. It takes about 4 minutes.
  - **First boot:** from the disk alone, with an e1000e adapter added. The service reports kernel 7.0, 4 CPUs, Secure Boot on, root on NVMe, the `bochs-drm` display driver, GNOME's display manager running, a page fetched from the host through the NAT, and no failed units. Then it powers off.
  - **Bug found: ARP answers for the guest's own address.** smoltcp's AnyIP mode, which the NAT uses to terminate guest TCP, answers ARP for any address. NetworkManager checks a new DHCP address with ARP (RFC 5227), took the answer for a conflict, and left the interface without routes. The NAT now passes ARP to smoltcp only for its own addresses. The earlier tests missed it: busybox's `udhcpc` doesn't check its address, and Windows' check wasn't tripped by it.
- **M10, Hyper-V enlightenments (an experiment; opt-in):** `vmmc-vmx run --enlightenments` has WHP expose Hyper-V's paravirtual interfaces, which the Windows hypervisor implements itself, so they cost no exits: the hypervisor CPUID leaves, hypercalls, VP index and runtime, the reference counter and TSC page, frequency MSRs, the guest idle MSR, the synthetic interrupt controller with direct-mode synthetic timers, the APIC-access MSRs, and the TLB-flush and IPI hypercalls.
  - **Found:** Windows needs the guest idle MSR whenever it sees the hypervisor. Without it, Windows 10 stops with KMODE_EXCEPTION_NOT_HANDLED during boot; bisecting the feature set over Windows Setup boots found it.
  - **Verified:** a WHP test checks that the guest sees "Microsoft Hv" only with the option. The Windows install test passes with it on (apart from the `Get-Tpm` check), and Windows reports a hypervisor.
  - **Not settled: whether it's faster.** One install with enlightenments reached first logon in 178 s; without, runs took 187 to 503 s on the same host, which is too noisy to call. A back-to-back comparison was cut short, so the option stays off by default until a steadier measurement (boot time of an installed VM, several runs each) shows a gain.

### Phase 3: Desktop app alpha + VMware migration (about 3–4 months)

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
- **Guest agent (Rust, Windows + Linux):** clipboard, display autofit, time sync, graceful shutdown, guest IP reporting, drag & drop file copy, shared folders.
- **Windows guest drivers:** rebuild virtio-win (BSD-3) and attestation-sign it with our EV cert via Microsoft Partner Center. Check WinFsp licensing for virtio-fs on Windows guests. Ship an MSI, plus .deb/.rpm for Linux.

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
