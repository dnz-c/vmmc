//! A real-mode guest that exercises the exits the WHP instruction emulator handles:
//! `rep outsb`, an MMIO write and an MMIO read. It also does a plain `in`.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use hv::Hypervisor;
use vm_core::{BusDevice, VmExit};

use crate::real_mode;

/// Where the guest image is loaded and starts executing.
pub const LOAD_ADDR: u64 = 0x1000;
/// RAM ends at 512 KiB so that `MMIO_BASE` has no RAM behind it.
pub const RAM: (u64, usize) = (0, 0x8_0000);
/// The test MMIO device, reached in real mode through DS = 0xd000.
pub const MMIO_BASE: u64 = 0xd_0000;
/// A port whose reads return `b'!'`.
pub const BANG_PORT: u16 = 0xf0;
/// What the guest prints: the `rep outsb` prefix, the two bytes read from MMIO offset 2,
/// then the byte read from `BANG_PORT`.
pub const EXPECTED_OUTPUT: &str = "rep: OK!";
/// The guest's one MMIO write: 0x1234 to offset 0, little-endian.
pub const EXPECTED_MMIO_WRITE: (u64, [u8; 2]) = (0, [0x34, 0x12]);

const PREFIX: &[u8] = b"rep: ";

/// Hand-assembled 16-bit code, `org 0x1000`. `PREFIX` follows it at 0x102f.
pub const CODE: [u8; 0x2f] = [
    0xfa, //                         1000  cli
    0x31, 0xc0, //                   1001  xor  ax, ax
    0x8e, 0xd8, //                   1003  mov  ds, ax
    0xbe, 0x2f, 0x10, //             1005  mov  si, 0x102f   ; PREFIX
    0xb9, 0x05, 0x00, //             1008  mov  cx, 5
    0xba, 0xe9, 0x00, //             100b  mov  dx, 0xe9     ; debug console port
    0xf3, 0x6e, //                   100e  rep  outsb
    0xb8, 0x00, 0xd0, //             1010  mov  ax, 0xd000
    0x8e, 0xd8, //                   1013  mov  ds, ax       ; DS -> MMIO_BASE
    0xc7, 0x06, 0x00, 0x00, 0x34, 0x12, // 1015  mov  word [0], 0x1234
    0xa1, 0x02, 0x00, //             101b  mov  ax, [2]      ; "OK"
    0xe6, 0xe9, //                   101e  out  0xe9, al
    0x88, 0xe0, //                   1020  mov  al, ah
    0xe6, 0xe9, //                   1022  out  0xe9, al
    0xe4, 0xf0, //                   1024  in   al, 0xf0     ; '!'
    0xe6, 0xe9, //                   1026  out  0xe9, al
    0x30, 0xc0, //                   1028  xor  al, al       ; exit code 0
    0xe6, 0xf4, //                   102a  out  0xf4, al     ; exit port
    0xf4, //                         102c  hlt
    0xeb, 0xfd, //                   102d  jmp  0x102c
];

/// The code followed by `PREFIX`.
pub fn image() -> Vec<u8> {
    let mut image = CODE.to_vec();
    image.extend_from_slice(PREFIX);
    image
}

/// Records writes; reads at offset 2 return "OK".
#[derive(Default)]
struct OkDevice {
    writes: Vec<(u64, Vec<u8>)>,
}

impl BusDevice for OkDevice {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = match offset + i as u64 {
                2 => b'O',
                3 => b'K',
                _ => 0,
            };
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        self.writes.push((offset, data.to_vec()));
    }
}

struct Bang;

impl BusDevice for Bang {
    fn read(&mut self, _: u64, data: &mut [u8]) {
        data.fill(b'!');
    }

    fn write(&mut self, _: u64, _: &[u8]) {}
}

/// What a run of the guest produced.
pub struct Outcome {
    pub exit: VmExit,
    /// What the guest printed.
    pub output: String,
    /// The MMIO writes the test device saw, as (offset, bytes).
    pub mmio_writes: Vec<(u64, Vec<u8>)>,
}

/// Boots the guest in a VM with RAM at `RAM` and the test devices attached.
pub fn run<H: Hypervisor>(hypervisor: &H) -> vm_core::Result<Outcome> {
    let device = Arc::new(Mutex::new(OkDevice::default()));
    let mmio = device.clone();
    let (exit, output) = real_mode::run(hypervisor, &[RAM], LOAD_ADDR, &image(), |vm| {
        vm.mmio().insert(MMIO_BASE, 0x1000, mmio)?;
        vm.pio()
            .insert(BANG_PORT.into(), 1, Arc::new(Mutex::new(Bang)))
    })?;
    let mmio_writes = std::mem::take(&mut device.lock().unwrap().writes);
    Ok(Outcome {
        exit,
        output,
        mmio_writes,
    })
}
