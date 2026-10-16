//! A running vCPU saved, its partition deleted, and restored into a new one: the guest
//! carries on as if nothing happened. Its clocks (the Hyper-V reference counter and the
//! TSC) carry on from where they were, without the time in between, and its SSE
//! registers keep their contents.
//!
//! Skipped without a running Windows hypervisor unless `VMMC_REQUIRE_WHP` is set.
#![cfg(windows)]

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use hv::{LocalApicMode, PartitionConfig, Segment, StandardRegisters};
use vm_core::{BusDevice, Bytes, GuestAddress, StopHandle, StopReason, Vm, VmExit};

/// Where the guest is loaded and starts.
const LOAD_ADDR: u64 = 0x1000;
/// Where it finds the value it loads into XMM0, and where it keeps storing XMM0.
const XMM_SOURCE: u64 = 0x3000;
const XMM_COPY: u64 = 0x3010;
const PATTERN: [u8; 16] = *b"vmmc snapshot ok";
/// The port the guest reports its clocks on.
const CLOCK_PORT: u16 = 0xe0;
/// Clock readings (reference counter, TSC) each run collects.
const READINGS: usize = 2000;
/// 100 ns units of reference time.
const MILLISECOND: u32 = 10_000;

/// Hand-assembled 16-bit code, `org 0x1000`: turns SSE on, loads XMM0, then forever
/// reports the reference counter's low 32 bits and the TSC / 65536 on port 0xe0 and
/// stores XMM0 at 0x3010.
const CODE: [u8; 0x33] = [
    0xfa, //                               1000  cli
    0x31, 0xc0, //                         1001  xor  ax, ax
    0x8e, 0xd8, //                         1003  mov  ds, ax
    0x0f, 0x20, 0xe0, //                   1005  mov  eax, cr4
    0x66, 0x0d, 0x00, 0x02, 0x00, 0x00, // 1008  or   eax, 0x200          ; OSFXSR
    0x0f, 0x22, 0xe0, //                   100e  mov  cr4, eax
    0x0f, 0x10, 0x06, 0x00, 0x30, //       1011  movups xmm0, [0x3000]
    0x66, 0xb9, 0x20, 0x00, 0x00, 0x40, // 1016  mov  ecx, 0x40000020     ; again:
    0x0f, 0x32, //                         101c  rdmsr                    ; reference counter
    0xba, 0xe0, 0x00, //                   101e  mov  dx, 0xe0
    0x66, 0xef, //                         1021  out  dx, eax
    0x0f, 0x31, //                         1023  rdtsc
    0x66, 0x0f, 0xac, 0xd0, 0x10, //       1025  shrd eax, edx, 16
    0x66, 0xef, //                         102a  out  dx, eax
    0x0f, 0x11, 0x06, 0x10, 0x30, //       102c  movups [0x3010], xmm0
    0xeb, 0xe3, //                         1031  jmp  0x1016              ; -> again
];

/// Collects the guest's clock readings, and stops the VM once it has a run's worth.
struct Clocks {
    readings: Vec<u32>,
    stop: StopHandle,
}

impl BusDevice for Clocks {
    fn read(&mut self, _: u64, data: &mut [u8]) {
        data.fill(0);
    }

    fn write(&mut self, _: u64, data: &[u8]) {
        let mut value = [0u8; 4];
        value[..data.len().min(4)].copy_from_slice(&data[..data.len().min(4)]);
        self.readings.push(u32::from_le_bytes(value));
        // Whole pairs only.
        if self.readings.len() >= READINGS * 2 {
            self.stop.request_stop(StopReason::Requested(0));
        }
    }
}

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

const CONFIG: PartitionConfig = PartitionConfig {
    vcpu_count: 1,
    local_apic: LocalApicMode::XApic,
    enlightenments: true,
};
const RAM: usize = 0x20_0000;

/// Windows allows one partition with guest memory per process, and tests run on
/// parallel threads: they take turns.
static ONE_VM_AT_A_TIME: Mutex<()> = Mutex::new(());

/// A VM with the clock port, and its readings.
fn vm(whp: &hv_whp::Whp) -> (Vm<hv_whp::WhpPartition>, Arc<Mutex<Clocks>>) {
    let vm = Vm::new(whp, &CONFIG, &[(0, RAM)]).expect("VM");
    let clocks = Arc::new(Mutex::new(Clocks {
        readings: Vec::new(),
        stop: vm.stop_handle(),
    }));
    vm.pio()
        .insert(CLOCK_PORT.into(), 4, clocks.clone())
        .unwrap();
    (vm, clocks)
}

/// (reference time, TSC / 65536) pairs.
fn pairs(clocks: &Mutex<Clocks>) -> Vec<(u32, u32)> {
    clocks
        .lock()
        .unwrap()
        .readings
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&[reference, tsc]| (reference, tsc))
        .collect()
}

#[test]
fn a_vcpu_restored_into_a_new_partition_carries_on() {
    let Some(whp) = whp() else { return };
    let _turn = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());

    // The first VM runs a while, then is saved and deleted.
    let (state, partition_state, ram, before) = {
        let (vm, clocks) = vm(&whp);
        vm.memory()
            .write_slice(&CODE, GuestAddress(LOAD_ADDR))
            .unwrap();
        vm.memory()
            .write_slice(&PATTERN, GuestAddress(XMM_SOURCE))
            .unwrap();
        let mut vcpu = vm.create_vcpu(0).unwrap();
        let mut sregs = vcpu.sregs().unwrap();
        for segment in [&mut sregs.ds, &mut sregs.es, &mut sregs.ss] {
            *segment = Segment::real_mode_data(0);
        }
        sregs.cs = Segment::real_mode_code(0);
        vcpu.set_sregs(&sregs).unwrap();
        vcpu.set_regs(&StandardRegisters {
            rip: LOAD_ADDR,
            rsp: 0x8000,
            rflags: 0x2,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            vcpu.run().unwrap(),
            VmExit::Stopped(StopReason::Requested(0))
        );
        vm.suspend_time().unwrap();
        let state = vcpu.save_state().unwrap();
        let partition_state = vm.save_state().unwrap();
        let mut ram = vec![0u8; RAM];
        vm.memory().read_slice(&mut ram, GuestAddress(0)).unwrap();
        (state, partition_state, ram, pairs(&clocks))
    };
    eprintln!(
        "saved {} registers and {} areas ({:?}); reference time {}",
        state.registers.len(),
        state.blobs.len(),
        state
            .blobs
            .iter()
            .map(|(kind, data)| (*kind, data.len()))
            .collect::<Vec<_>>(),
        partition_state.reference_time
    );

    // Time passes on the host.
    thread::sleep(Duration::from_secs(1));

    // A new VM, with the old one's RAM, except XMM0's source and copy: the copy only
    // comes back if XMM0 does.
    let (vm, clocks) = vm(&whp);
    let mut ram = ram;
    for address in [XMM_SOURCE, XMM_COPY] {
        ram[address as usize..address as usize + 16].fill(0);
    }
    vm.memory().write_slice(&ram, GuestAddress(0)).unwrap();
    let mut vcpu = vm.create_vcpu(0).unwrap();
    vm.suspend_time().unwrap();
    vm.restore_state(&partition_state).unwrap();
    vcpu.restore_state(&state).unwrap();

    // Everything saved came back as it was.
    let again = vcpu.save_state().unwrap();
    for (&(name, value), &(name_again, value_again)) in state.registers.iter().zip(&again.registers)
    {
        assert_eq!(name, name_again);
        assert_eq!(
            value, value_again,
            "register {name:#x} came back as {value_again:#x}, not {value:#x}"
        );
    }
    for ((kind, data), (_, data_again)) in state.blobs.iter().zip(&again.blobs) {
        assert!(data == data_again, "state {kind:#x} didn't come back");
    }

    vm.resume_time().unwrap();
    assert_eq!(
        vcpu.run().unwrap(),
        VmExit::Stopped(StopReason::Requested(0))
    );
    let after = pairs(&clocks);

    let (first, last) = (before[0], before[before.len() - 1]);
    let resumed = after[0];
    let reference_gap = resumed.0.wrapping_sub(last.0);
    eprintln!(
        "reference time: {} before, {} after the restore ({} ms apart)",
        last.0,
        resumed.0,
        reference_gap / MILLISECOND
    );
    assert!(
        reference_gap < 100 * MILLISECOND,
        "the reference counter jumped by {reference_gap} (100 ns units) over the restore"
    );
    // The TSC's rate, in TSC / 65536 per reference tick, from the first run.
    let rate = f64::from(last.1.wrapping_sub(first.1)) / f64::from(last.0.wrapping_sub(first.0));
    let tsc_gap = f64::from(resumed.1.wrapping_sub(last.1)) / rate;
    eprintln!(
        "the TSC moved {:.1} ms over the restore",
        tsc_gap / f64::from(MILLISECOND)
    );
    assert!(
        tsc_gap < f64::from(100 * MILLISECOND),
        "the TSC jumped by {tsc_gap:.0} reference ticks over the restore"
    );

    let mut copy = [0u8; 16];
    vm.memory()
        .read_slice(&mut copy, GuestAddress(XMM_COPY))
        .unwrap();
    assert_eq!(copy, PATTERN, "XMM0 didn't come back");
}

/// Hand-assembled 16-bit code, `org 0x1000`: sets the guest OS ID and enables and locks
/// the hypercall page at 0x5000, as Windows does, then stops the VM.
const HYPERCALL_CODE: [u8; 0x26] = [
    0xfa, //                               1000  cli
    0x66, 0xb9, 0x00, 0x00, 0x00, 0x40, // 1001  mov  ecx, 0x40000000     ; guest OS ID
    0x66, 0xb8, 0x61, 0x4a, 0x00, 0x00, // 1007  mov  eax, 0x00004a61
    0x66, 0xba, 0x00, 0x0a, 0x01, 0x00, // 100d  mov  edx, 0x00010a00
    0x0f, 0x30, //                         1013  wrmsr
    0x66, 0xb9, 0x01, 0x00, 0x00, 0x40, // 1015  mov  ecx, 0x40000001     ; hypercall
    0x66, 0xb8, 0x03, 0x50, 0x00, 0x00, // 101b  mov  eax, 0x5003         ; enable, lock
    0x66, 0x31, 0xd2, //                   1021  xor  edx, edx
    0x0f, 0x30, //                         1024  wrmsr
];

/// The hypercall page belongs to the partition, and every vCPU's state names it: a
/// second vCPU's restore finds it set and locked already.
#[test]
fn a_locked_hypercall_page_is_restored_on_every_vcpu() {
    let Some(whp) = whp() else { return };
    let _turn = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let config = PartitionConfig {
        vcpu_count: 2,
        ..CONFIG
    };
    let mut code = HYPERCALL_CODE.to_vec();
    code.extend_from_slice(&[0xe6, 0xe0, 0xeb, 0xfc]); // out 0xe0, al; jmp back to it
    let new_vm = || {
        let vm = Vm::new(&whp, &config, &[(0, RAM)]).expect("VM");
        let clocks = Arc::new(Mutex::new(Clocks {
            readings: vec![0; READINGS * 2 - 1],
            stop: vm.stop_handle(),
        }));
        vm.pio()
            .insert(CLOCK_PORT.into(), 4, clocks.clone())
            .unwrap();
        vm
    };
    let (states, partition_state, ram) = {
        let vm = new_vm();
        vm.memory()
            .write_slice(&code, GuestAddress(LOAD_ADDR))
            .unwrap();
        let mut bsp = vm.create_vcpu(0).unwrap();
        let mut ap = vm.create_vcpu(1).unwrap();
        ap.set_waiting_for_sipi().unwrap();
        let mut sregs = bsp.sregs().unwrap();
        for segment in [&mut sregs.ds, &mut sregs.es, &mut sregs.ss] {
            *segment = Segment::real_mode_data(0);
        }
        sregs.cs = Segment::real_mode_code(0);
        bsp.set_sregs(&sregs).unwrap();
        bsp.set_regs(&StandardRegisters {
            rip: LOAD_ADDR,
            rsp: 0x8000,
            rflags: 0x2,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            bsp.run().unwrap(),
            VmExit::Stopped(StopReason::Requested(0))
        );
        vm.suspend_time().unwrap();
        let states = [bsp.save_state().unwrap(), ap.save_state().unwrap()];
        let mut ram = vec![0u8; RAM];
        vm.memory().read_slice(&mut ram, GuestAddress(0)).unwrap();
        (states, vm.save_state().unwrap(), ram)
    };
    assert_eq!(
        states[1].register(0x5001),
        Some(0x5003),
        "the AP sees it too"
    );
    let vm = new_vm();
    vm.memory().write_slice(&ram, GuestAddress(0)).unwrap();
    let mut bsp = vm.create_vcpu(0).unwrap();
    let mut ap = vm.create_vcpu(1).unwrap();
    vm.suspend_time().unwrap();
    vm.restore_state(&partition_state).unwrap();
    bsp.restore_state(&states[0]).unwrap();
    ap.restore_state(&states[1]).unwrap();
    assert_eq!(ap.save_state().unwrap().register(0x5001), Some(0x5003));
}
