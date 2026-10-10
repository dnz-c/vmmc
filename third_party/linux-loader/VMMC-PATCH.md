# VMMC patch: linux-loader 0.14.0

This is the crates.io release of `linux-loader` 0.14.0, unchanged except for its
`Cargo.toml`.

## Change

`vm-memory` is depended on with `default-features = false`.

## Why

The upstream manifest pulls in `vm-memory` with default features. That enables
`vm-memory`'s `rawfd` feature, which is Unix-only and has a `compile_error!` on Windows.
Cargo unifies features across the build graph, so this one dependency breaks every
Windows build of vmmc, even though `linux-loader` doesn't use `rawfd`.

## Removing this patch

Drop the `[patch.crates-io]` entry in the workspace `Cargo.toml` and delete this
directory once an upstream release depends on `vm-memory` without default features.
