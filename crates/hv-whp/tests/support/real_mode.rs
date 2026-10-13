//! Runs tiny hand-assembled real-mode guests. They print through the debug console port
//! and stop the VM through the exit port.
#![allow(dead_code)]

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use hv::{Hypervisor, PartitionConfig, Segment, StandardRegisters};
use vm_core::{BusDevice, Bytes, GuestAddress, StopHandle, StopReason, Vm, VmExit};

/// Bochs/QEMU debug console convention.
pub const DEBUG_PORT: u16 = 0xe9;
/// Writing a byte here stops the VM with that byte as the code.
pub const EXIT_PORT: u16 = 0xf4;
/// How long a guest may run before the watchdog stops it.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// The stop code the watchdog uses.
pub const TIMED_OUT: u32 = 0xdead;

/// Collects what the guest writes to the debug console. Reads return 0xe9, which is how
/// guests detect the port.
#[derive(Default)]
struct DebugConsole {
    output: Vec<u8>,
}

impl BusDevice for DebugConsole {
    fn read(&mut self, _: u64, data: &mut [u8]) {
        data.fill(0xe9);
    }

    fn write(&mut self, _: u64, data: &[u8]) {
        self.output.extend_from_slice(data);
    }
}

struct ExitPort(StopHandle);

impl BusDevice for ExitPort {
    fn read(&mut self, _: u64, data: &mut [u8]) {
        data.fill(0);
    }

    fn write(&mut self, _: u64, data: &[u8]) {
        self.0.request_stop(StopReason::Requested(data[0].into()));
    }
}

/// Boots `image` at `load_addr` (real mode, all segments 0) in a single-vCPU VM with RAM
/// at `ram`. `setup` can add devices first. Returns how the guest exited and what it
/// printed. A watchdog stops the guest if it doesn't finish within [`TIMEOUT`].
pub fn run<H: Hypervisor>(
    hypervisor: &H,
    ram: &[(u64, usize)],
    load_addr: u64,
    image: &[u8],
    setup: impl FnOnce(&Vm<H::Partition>) -> vm_core::Result<()>,
) -> vm_core::Result<(VmExit, String)> {
    run_with(
        hypervisor,
        &PartitionConfig::default(),
        ram,
        load_addr,
        image,
        setup,
    )
}

/// [`run`], in a partition made with `config` (its vCPU count aside: one vCPU runs).
pub fn run_with<H: Hypervisor>(
    hypervisor: &H,
    config: &PartitionConfig,
    ram: &[(u64, usize)],
    load_addr: u64,
    image: &[u8],
    setup: impl FnOnce(&Vm<H::Partition>) -> vm_core::Result<()>,
) -> vm_core::Result<(VmExit, String)> {
    // Windows allows one partition with guest memory per process, and the test harness
    // runs tests on parallel threads: take turns.
    static ONE_VM_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _serial = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());

    let vm = Vm::new(hypervisor, config, ram)?;
    vm.memory().write_slice(image, GuestAddress(load_addr))?;

    let console = Arc::new(Mutex::new(DebugConsole::default()));
    vm.pio().insert(DEBUG_PORT.into(), 1, console.clone())?;
    vm.pio().insert(
        EXIT_PORT.into(),
        1,
        Arc::new(Mutex::new(ExitPort(vm.stop_handle()))),
    )?;
    setup(&vm)?;

    let mut vcpu = vm.create_vcpu(0)?;
    let mut sregs = vcpu.sregs()?;
    sregs.cs = Segment::real_mode_code(0);
    sregs.ds = Segment::real_mode_data(0);
    sregs.es = Segment::real_mode_data(0);
    sregs.ss = Segment::real_mode_data(0);
    vcpu.set_sregs(&sregs)?;
    vcpu.set_regs(&StandardRegisters {
        rip: load_addr,
        rsp: 0x8000,
        rflags: 0x2,
        ..Default::default()
    })?;

    let (done_tx, done_rx) = mpsc::channel::<()>();
    let stop = vm.stop_handle();
    let watchdog = thread::spawn(move || {
        if done_rx.recv_timeout(TIMEOUT) == Err(RecvTimeoutError::Timeout) {
            stop.request_stop(StopReason::Requested(TIMED_OUT));
            // A kick can land just before the vCPU enters the guest; repeat until the
            // run returns.
            while done_rx.recv_timeout(Duration::from_millis(50)) == Err(RecvTimeoutError::Timeout)
            {
                stop.kick_all();
            }
        }
    });
    let exit = vcpu.run();
    drop(done_tx);
    watchdog.join().expect("watchdog thread panicked");

    let output = String::from_utf8_lossy(&console.lock().unwrap().output).into_owned();
    Ok((exit?, output))
}
