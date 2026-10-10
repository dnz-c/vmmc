//! A real-mode guest that prints a greeting one `out` at a time, then stops the VM.
#![allow(dead_code)]

use hv::Hypervisor;
use vm_core::VmExit;

use crate::real_mode;

pub const GREETING: &str = "Hello from vmmc\n";
/// Where the guest image is loaded and starts executing.
pub const LOAD_ADDR: u64 = 0x1000;

/// Hand-assembled 16-bit code, `org 0x1000`. The greeting follows it at 0x101a.
pub const CODE: [u8; 0x1a] = [
    0xfa, //             1000  cli
    0x31, 0xc0, //       1001  xor  ax, ax
    0x8e, 0xd8, //       1003  mov  ds, ax
    0xbe, 0x1a, 0x10, // 1005  mov  si, 0x101a      ; greeting
    0xba, 0xe9, 0x00, // 1008  mov  dx, 0xe9        ; debug console port
    0xac, //             100b  lodsb                ; next:
    0x84, 0xc0, //       100c  test al, al
    0x74, 0x03, //       100e  je   0x1013          ; -> done
    0xee, //             1010  out  dx, al
    0xeb, 0xf8, //       1011  jmp  0x100b          ; -> next
    0xb0, 0x00, //       1013  mov  al, 0           ; done: exit code 0
    0xe6, 0xf4, //       1015  out  0xf4, al        ; exit port
    0xf4, //             1017  hlt                  ; only reached if nothing claims 0xf4
    0xeb, 0xfd, //       1018  jmp  0x1017
];

/// The code followed by the NUL-terminated greeting.
pub fn image() -> Vec<u8> {
    let mut image = CODE.to_vec();
    image.extend_from_slice(GREETING.as_bytes());
    image.push(0);
    image
}

/// Boots the guest in a 2 MiB VM and returns how it exited and what it printed.
pub fn run<H: Hypervisor>(hypervisor: &H) -> vm_core::Result<(VmExit, String)> {
    real_mode::run(hypervisor, &[(0, 0x20_0000)], LOAD_ADDR, &image(), |_| {
        Ok(())
    })
}
