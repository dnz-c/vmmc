//! Boots tiny real-mode guests on WHP.
//!
//! Hosts without a running Windows hypervisor skip the boot tests, unless
//! `VMMC_REQUIRE_WHP` is set (as `cargo xtask boot-test` does), in which case they fail.
//! The listing tests run everywhere: they check the hand-assembled guest code.
#![cfg(windows)]

#[path = "support/hello_guest.rs"]
mod hello_guest;
#[path = "support/io_guest.rs"]
mod io_guest;
#[path = "support/real_mode.rs"]
mod real_mode;

use iced_x86::{Decoder, DecoderOptions, Formatter, IntelFormatter};
use vm_core::{StopReason, VmExit};

/// Disassembles 16-bit `code` loaded at `ip`, one instruction per string.
fn listing(code: &[u8], ip: u64) -> Vec<String> {
    let mut decoder = Decoder::with_ip(16, code, ip, DecoderOptions::NONE);
    let mut formatter = IntelFormatter::new();
    formatter
        .options_mut()
        .set_space_after_operand_separator(false);
    let mut listing = Vec::new();
    for instruction in &mut decoder {
        let mut text = String::new();
        formatter.format(&instruction, &mut text);
        listing.push(text);
    }
    listing
}

/// WHP, or `None` (with a note) when the host can't run it and it isn't required.
fn whp() -> Option<hv_whp::Whp> {
    match hv_whp::Whp::new() {
        Ok(whp) => Some(whp),
        Err(hv::Error::Unavailable(reason)) if std::env::var_os("VMMC_REQUIRE_WHP").is_none() => {
            eprintln!("skipping: {reason}");
            None
        }
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn hello_guest_code_matches_its_listing() {
    assert_eq!(
        listing(&hello_guest::CODE, hello_guest::LOAD_ADDR),
        [
            "cli",
            "xor ax,ax",
            "mov ds,ax",
            "mov si,101Ah",
            "mov dx,0E9h",
            "lodsb [si]",
            "test al,al",
            "je short 1013h",
            "out dx,al",
            "jmp short 100Bh",
            "mov al,0",
            "out 0F4h,al",
            "hlt",
            "jmp short 1017h",
        ]
    );
    assert_eq!(
        hello_guest::LOAD_ADDR + hello_guest::CODE.len() as u64,
        0x101a,
        "the greeting must start where `mov si` points"
    );
}

#[test]
fn io_guest_code_matches_its_listing() {
    assert_eq!(
        listing(&io_guest::CODE, io_guest::LOAD_ADDR),
        [
            "cli",
            "xor ax,ax",
            "mov ds,ax",
            "mov si,102Fh",
            "mov cx,5",
            "mov dx,0E9h",
            "rep outsb dx,[si]",
            "mov ax,0D000h",
            "mov ds,ax",
            "mov word ptr [0],1234h",
            "mov ax,[2]",
            "out 0E9h,al",
            "mov al,ah",
            "out 0E9h,al",
            "in al,0F0h",
            "out 0E9h,al",
            "xor al,al",
            "out 0F4h,al",
            "hlt",
            "jmp short 102Ch",
        ]
    );
    assert_eq!(
        io_guest::LOAD_ADDR + io_guest::CODE.len() as u64,
        0x102f,
        "the prefix must start where `mov si` points"
    );
}

#[test]
fn hello_guest_prints_greeting_and_stops() {
    let Some(whp) = whp() else { return };
    let (exit, output) = hello_guest::run(&whp).expect("running the guest failed");
    assert_eq!(output, hello_guest::GREETING);
    assert_eq!(exit, VmExit::Stopped(StopReason::Requested(0)));
}

#[test]
fn io_guest_exercises_string_io_and_mmio() {
    let Some(whp) = whp() else { return };
    let outcome = io_guest::run(&whp).expect("running the guest failed");
    assert_eq!(outcome.output, io_guest::EXPECTED_OUTPUT);
    let (offset, bytes) = io_guest::EXPECTED_MMIO_WRITE;
    assert_eq!(outcome.mmio_writes, [(offset, bytes.to_vec())]);
    assert_eq!(outcome.exit, VmExit::Stopped(StopReason::Requested(0)));
}
