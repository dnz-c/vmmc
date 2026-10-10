//! A fake hypervisor whose boot CPU runs a Rust closure instead of guest code. The
//! closure makes the same port and memory accesses a guest would, through `CpuIo`, so
//! the whole platform behind it is exercised for real.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use hv::{
    CpuIo, Hypervisor, Interrupt, InterruptController, MemoryAccess, Partition, PartitionConfig,
    SpecialRegisters, StandardRegisters, Vcpu, VcpuExit, VcpuKicker,
};

/// What the boot CPU does, once, on its first run.
pub type Program = Box<dyn FnOnce(&dyn CpuIo) + Send>;

/// What the test can inspect afterwards.
#[derive(Default)]
pub struct Recorded {
    pub interrupts: Vec<Interrupt>,
    pub boot_regs: Option<StandardRegisters>,
    pub boot_sregs: Option<SpecialRegisters>,
    /// vCPUs put into wait-for-SIPI.
    pub waiting_for_sipi: Vec<u32>,
}

pub type Shared = Arc<Mutex<Recorded>>;

pub struct FakeHv {
    program: Mutex<Option<Program>>,
    pub recorded: Shared,
}

impl FakeHv {
    pub fn new(program: impl FnOnce(&dyn CpuIo) + Send + 'static) -> Self {
        Self {
            program: Mutex::new(Some(Box::new(program))),
            recorded: Shared::default(),
        }
    }
}

impl Hypervisor for FakeHv {
    type Partition = FakePartition;

    fn create_partition(&self, _: &PartitionConfig) -> hv::Result<FakePartition> {
        Ok(FakePartition {
            program: Mutex::new(self.program.lock().unwrap().take()),
            recorded: self.recorded.clone(),
        })
    }
}

pub struct FakePartition {
    program: Mutex<Option<Program>>,
    recorded: Shared,
}

impl Partition for FakePartition {
    type Vcpu = FakeVcpu;
    type Interrupts = FakeInterrupts;

    unsafe fn map_memory(&self, _: u64, _: *mut u8, _: u64, _: MemoryAccess) -> hv::Result<()> {
        Ok(())
    }

    fn unmap_memory(&self, _: u64, _: u64) -> hv::Result<()> {
        Ok(())
    }

    fn create_vcpu(&self, index: u32) -> hv::Result<FakeVcpu> {
        let program = if index == 0 {
            self.program.lock().unwrap().take()
        } else {
            None
        };
        Ok(FakeVcpu {
            index,
            program,
            recorded: self.recorded.clone(),
        })
    }

    fn interrupts(&self) -> FakeInterrupts {
        FakeInterrupts(self.recorded.clone())
    }
}

pub struct FakeVcpu {
    index: u32,
    program: Option<Program>,
    recorded: Shared,
}

impl Vcpu for FakeVcpu {
    type Kicker = FakeKicker;

    fn index(&self) -> u32 {
        self.index
    }

    fn regs(&self) -> hv::Result<StandardRegisters> {
        Ok(StandardRegisters::default())
    }

    fn set_regs(&mut self, regs: &StandardRegisters) -> hv::Result<()> {
        self.recorded.lock().unwrap().boot_regs = Some(*regs);
        Ok(())
    }

    fn sregs(&self) -> hv::Result<SpecialRegisters> {
        Ok(SpecialRegisters::default())
    }

    fn set_sregs(&mut self, sregs: &SpecialRegisters) -> hv::Result<()> {
        self.recorded.lock().unwrap().boot_sregs = Some(*sregs);
        Ok(())
    }

    fn set_waiting_for_sipi(&mut self) -> hv::Result<()> {
        self.recorded
            .lock()
            .unwrap()
            .waiting_for_sipi
            .push(self.index);
        Ok(())
    }

    fn run(&mut self, io: &dyn CpuIo) -> hv::Result<VcpuExit> {
        match self.program.take() {
            Some(program) => {
                program(io);
                Ok(VcpuExit::Handled)
            }
            None => {
                // Idle until kicked: the VMM loop checks for a stop each time.
                thread::sleep(Duration::from_millis(1));
                Ok(VcpuExit::Canceled)
            }
        }
    }

    fn kicker(&self) -> FakeKicker {
        FakeKicker
    }
}

#[derive(Clone)]
pub struct FakeKicker;

impl VcpuKicker for FakeKicker {
    fn kick(&self) {}
}

#[derive(Clone)]
pub struct FakeInterrupts(Shared);

impl InterruptController for FakeInterrupts {
    fn request(&self, interrupt: &Interrupt) -> hv::Result<()> {
        self.0.lock().unwrap().interrupts.push(*interrupt);
        Ok(())
    }
}
