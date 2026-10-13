//! Links the TPM 2.0 reference implementation (ms-tpm-20-ref) and OpenSSL, as static
//! MSVC libraries, when `cargo xtask fetch-native` has put them in `target/native`
//! (or `VMMC_NATIVE_DIR`). Then it sets `cfg(tpm_ref)`. Without them the crate still
//! builds, and `Tpm::open` explains how to get them.

use std::env;
use std::fs;
use std::path::PathBuf;

/// Every library the native-deps workflow publishes for the TPM starts with this.
const PREFIX: &str = "tpm-";
/// What OpenSSL's static libcrypto needs from Windows.
const SYSTEM_LIBS: [&str; 5] = ["ws2_32", "crypt32", "advapi32", "user32", "bcrypt"];

fn main() {
    println!("cargo::rustc-check-cfg=cfg(tpm_ref)");
    println!("cargo::rerun-if-env-changed=VMMC_NATIVE_DIR");
    let dir = env::var_os("VMMC_NATIVE_DIR").map_or_else(
        || {
            PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("set by cargo"))
                .join("../../target/native")
        },
        PathBuf::from,
    );
    println!("cargo::rerun-if-changed={}", dir.display());
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let libraries: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|name| {
            name.strip_prefix(PREFIX)?;
            name.strip_suffix(".lib").map(str::to_owned)
        })
        .collect();
    // The TPM core, its OpenSSL glue, the reference platform, and OpenSSL itself.
    let complete = ["Tpm_CoreLib", "Tpm_PlatformLib", "libcrypto"]
        .iter()
        .all(|wanted| {
            libraries
                .iter()
                .any(|lib| lib == &format!("{PREFIX}{wanted}"))
        });
    if !complete {
        return;
    }
    println!("cargo::rustc-link-search=native={}", dir.display());
    for library in &libraries {
        println!(
            "cargo::rerun-if-changed={}",
            dir.join(format!("{library}.lib")).display()
        );
        // The MSVC linker resolves symbols across all of them whatever the order.
        println!("cargo::rustc-link-lib=static={library}");
    }
    for library in SYSTEM_LIBS {
        println!("cargo::rustc-link-lib=dylib={library}");
    }
    println!("cargo::rustc-cfg=tpm_ref");
}
