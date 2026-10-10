# VMMC patch: virtio-queue 0.18.0

This is the crates.io release of `virtio-queue` 0.18.0, unchanged except for its
`Cargo.toml`.

## Changes

1. `vm-memory` is depended on with `default-features = false`.
2. The `vmm-sys-util` dependency is removed.

## Why

1. The upstream manifest pulls in `vm-memory` with default features. That enables
   `vm-memory`'s `rawfd` feature, which is Unix-only and has a `compile_error!` on
   Windows. Cargo unifies features across the build graph, so this one dependency breaks
   every Windows build of vmmc, even though `virtio-queue` doesn't use `rawfd`.
2. Nothing under `src/` references `vmm-sys-util`, and it is a mostly Linux-specific
   crate. Dropping it removes a needless Windows build risk.

## Removing this patch

Drop the `[patch.crates-io]` entry in the workspace `Cargo.toml` and delete this
directory once an upstream release has both changes.
