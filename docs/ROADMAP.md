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

**Known guest-log noise to address with firmware boot in Phase 2:** no DMI/SMBIOS, MTRRs not set up, no `_OSC` on the PCI root, and no PS/2 mouse.

### Phase 2: Installs Windows 11 from ISO (MVP engine, about 3–5 months)

- **Firmware:** `fw_cfg` + table-loader, ACPI (DSDT/MADT/FADT/MCFG/HPET), and the pinned OVMF build with Secure Boot and MS keys enrolled. UEFI variables persist per VM.
- **Devices:** AHCI (HDD + ATAPI CD-ROM for ISOs), NVMe, bochs-display (GOP), PS/2, xHCI + USB tablet, e1000e, TPM 2.0 CRB and HDA audio.
- **Disk and network:** `disk` crate with VMDK + raw and IOCP async I/O; `net-nat` with DHCP and DNS.
- **Console:** a minimal Slint console window (framebuffer + input) as the dev UI.
- **Exit criteria:**
  - Windows 11 (current ISO) installs unattended onto NVMe and reaches the desktop with NAT internet. `msinfo32` shows Secure Boot On and TPM 2.0.
  - Ubuntu 26.04 Desktop installs from ISO.

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
