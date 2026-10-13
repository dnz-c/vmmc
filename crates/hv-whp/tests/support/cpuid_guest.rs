//! A real-mode guest that prints the hypervisor vendor string (CPUID 0x40000000: EBX,
//! ECX, EDX), then stops the VM.
#![allow(dead_code)]

use hv::{Hypervisor, LocalApicMode, PartitionConfig};
use vm_core::VmExit;

use crate::real_mode;

/// Where the guest image is loaded and starts executing.
pub const LOAD_ADDR: u64 = 0x1000;

/// Hand-assembled 16-bit code, `org 0x1000`. The vendor string goes to 0x2000.
pub const CODE: [u8; 0x30] = [
    0xfa, //                               1000  cli
    0x31, 0xc0, //                         1001  xor  ax, ax
    0x8e, 0xd8, //                         1003  mov  ds, ax
    0x66, 0xb8, 0x00, 0x00, 0x00, 0x40, // 1005  mov  eax, 0x40000000
    0x0f, 0xa2, //                         100b  cpuid
    0x66, 0x89, 0x1e, 0x00, 0x20, //       100d  mov  [0x2000], ebx
    0x66, 0x89, 0x0e, 0x04, 0x20, //       1012  mov  [0x2004], ecx
    0x66, 0x89, 0x16, 0x08, 0x20, //       1017  mov  [0x2008], edx
    0xbe, 0x00, 0x20, //                   101c  mov  si, 0x2000
    0xb9, 0x0c, 0x00, //                   101f  mov  cx, 12
    0xba, 0xe9, 0x00, //                   1022  mov  dx, 0xe9        ; debug console port
    0xac, //                               1025  lodsb                ; next:
    0xee, //                               1026  out  dx, al
    0xe2, 0xfc, //                         1027  loop 0x1025          ; -> next
    0xb0, 0x00, //                         1029  mov  al, 0           ; exit code 0
    0xe6, 0xf4, //                         102b  out  0xf4, al        ; exit port
    0xf4, //                               102d  hlt                  ; only reached if nothing claims 0xf4
    0xeb, 0xfd, //                         102e  jmp  0x102d
];

/// Boots the guest in a 2 MiB VM, with or without Hyper-V enlightenments, and returns
/// how it exited and the vendor string it printed.
pub fn run<H: Hypervisor>(
    hypervisor: &H,
    enlightenments: bool,
) -> vm_core::Result<(VmExit, String)> {
    // As the machine builds it: the synthetic interrupt controller needs a local APIC.
    let config = PartitionConfig {
        local_apic: LocalApicMode::XApic,
        enlightenments,
        ..PartitionConfig::default()
    };
    real_mode::run_with(
        hypervisor,
        &config,
        &[(0, 0x20_0000)],
        LOAD_ADDR,
        &CODE,
        |_| Ok(()),
    )
}
