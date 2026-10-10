# Contributing

## Contributor licence agreement

vmmc is open-core: this repository is Apache-2.0, and a separate proprietary Pro tier
builds on it. To keep that possible, every contribution needs a signed contributor
licence agreement (CLA). The CLA isn't published yet, so outside contributions can't be
merged until it is.

## Clean-room rules

These protect the project's right to ship. A change that breaks them is rejected,
however good the code is.

- **No copyleft code.** Don't copy code from QEMU, VirtualBox or any other GPL, LGPL or
  AGPL project. Reading their documentation is fine. Dependencies must use permissive
  licences; `cargo deny check` enforces the list in `deny.toml`.
- **No reverse engineering of VMware software.** Work only from public specifications
  such as VMware's published VMDK specification, DMTF OVF (DSP0243), and documented
  `.vmx` keys, and from observing files VMware creates.
- **Permissive code we may borrow from**, keeping the original copyright notice: the
  rust-vmm crates, crosvm (BSD-3-Clause), cloud-hypervisor (Apache-2.0/BSD-3-Clause),
  OpenVMM (MIT) and Firecracker (Apache-2.0).
- **Patched upstream crates** live in `third_party/` and are wired in with
  `[patch.crates-io]`. Keep the crates.io release unchanged except for the patch, and
  explain it in a `VMMC-PATCH.md` that also says when the patch can go. Send the fix
  upstream.
- **Trademarks.** Don't put "VMware" or "Workstation" in product, crate or binary names.
  Saying a feature opens VMware VMs is fine.

## Code

- Run `cargo xtask ci` before sending a change. It runs the format check, clippy with
  warnings as errors, the tests and cargo-deny.
- Every `unsafe` block needs a `// SAFETY:` comment that explains why it is sound.
- Device models parse guest-controlled data and are the main attack surface. Code that
  parses guest input needs tests for malformed input, and fuzz targets once the fuzzing
  harness lands.
