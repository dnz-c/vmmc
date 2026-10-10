//! Backend-neutral hypervisor interface for the vmmc engine.
//!
//! `vm-core` and the device models are written only against these traits. Each host
//! hypervisor API (WHP on Windows today, KVM on Linux later) implements them in its own
//! crate, so backend-specific types never leak into the rest of the engine.

mod regs;

use std::fmt;
use std::ops::BitOr;

pub use regs::{DescriptorTable, Segment, SpecialRegisters, StandardRegisters};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The host cannot run VMs with this backend. The message says how to fix that.
    #[error("{0}")]
    Unavailable(String),
    #[error("{op} failed: {source}")]
    Backend {
        op: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

/// Where the guest's local APIC lives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LocalApicMode {
    /// No local APIC. Only useful for bare real-mode or legacy-PIC guests.
    #[default]
    Disabled,
    /// The hypervisor emulates an xAPIC.
    XApic,
    /// The hypervisor emulates an x2APIC (xAPIC mode stays available to the guest).
    X2Apic,
}

#[derive(Clone, Debug)]
pub struct PartitionConfig {
    pub vcpu_count: u32,
    pub local_apic: LocalApicMode,
}

impl Default for PartitionConfig {
    fn default() -> Self {
        Self {
            vcpu_count: 1,
            local_apic: LocalApicMode::default(),
        }
    }
}

/// Guest access rights for a mapped range of guest-physical memory.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MemoryAccess(u8);

impl MemoryAccess {
    pub const READ: Self = Self(1);
    pub const WRITE: Self = Self(2);
    pub const EXECUTE: Self = Self(4);
    pub const ALL: Self = Self(7);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for MemoryAccess {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl fmt::Debug for MemoryAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let flag = |access, c| if self.contains(access) { c } else { '-' };
        write!(
            f,
            "{}{}{}",
            flag(Self::READ, 'r'),
            flag(Self::WRITE, 'w'),
            flag(Self::EXECUTE, 'x')
        )
    }
}

/// Why [`Vcpu::run`] returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VcpuExit {
    /// The exit was handled through [`CpuIo`] (a port or memory access, or an EOI). Call
    /// [`Vcpu::run`] again to continue.
    Handled,
    /// The guest halted and nothing inside the hypervisor will wake it.
    Halt,
    /// The guest cannot continue, for example after a triple fault.
    Shutdown,
    /// [`VcpuKicker::kick`] interrupted the run.
    Canceled,
    /// An exit this backend does not handle yet.
    Unhandled(String),
}

/// The VMM side of a vCPU: [`Vcpu::run`] calls these while it handles an exit.
///
/// Calls happen on the vCPU's thread, possibly several per exit (string I/O, or an
/// emulated instruction that touches both MMIO and RAM).
pub trait CpuIo {
    /// The guest read from an I/O port. Fill `data` (1, 2 or 4 bytes).
    fn io_read(&self, port: u16, data: &mut [u8]);
    /// The guest wrote `data` (1, 2 or 4 bytes) to an I/O port.
    fn io_write(&self, port: u16, data: &[u8]);
    /// The guest read guest-physical memory the hypervisor couldn't serve itself: device
    /// MMIO, or RAM touched by an emulated instruction. Fill `data` (1 to 8 bytes).
    fn mem_read(&self, gpa: u64, data: &mut [u8]);
    /// The guest wrote `data` (1 to 8 bytes) to guest-physical memory the hypervisor
    /// couldn't serve itself.
    fn mem_write(&self, gpa: u64, data: &[u8]);
    /// The guest's local APIC finished a level-triggered interrupt with this vector.
    fn eoi(&self, vector: u8);
}

/// How the local APIC delivers an [`Interrupt`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Fixed,
    LowestPriority,
    Nmi,
    Init,
    Sipi,
}

/// An interrupt message to one or more local APICs, as an MSI or the IOAPIC sends it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interrupt {
    pub vector: u8,
    pub delivery: Delivery,
    /// An APIC ID, or a logical destination when `logical` is set.
    pub destination: u32,
    pub logical: bool,
    /// Level-triggered interrupts get an EOI notification through [`CpuIo::eoi`].
    pub level_triggered: bool,
}

/// Sends interrupts into a partition from any thread.
pub trait InterruptController: Clone + Send + Sync + 'static {
    /// Delivers `interrupt`. Does nothing once the partition is gone.
    fn request(&self, interrupt: &Interrupt) -> Result<()>;
}

/// A host hypervisor API.
pub trait Hypervisor {
    type Partition: Partition;

    fn create_partition(&self, config: &PartitionConfig) -> Result<Self::Partition>;
}

/// One VM's guest-physical address space and its virtual processors.
pub trait Partition: Send + Sync {
    type Vcpu: Vcpu;
    type Interrupts: InterruptController;

    /// Maps `size` bytes of host memory at `host` into the guest at `gpa`.
    ///
    /// # Safety
    ///
    /// `host` must point to `size` bytes of page-aligned memory that stays allocated
    /// until the range is unmapped, or until this partition and every vCPU created from
    /// it have been dropped. The guest can read and write that memory at any time while
    /// one of its vCPUs runs. Kickers and interrupt handles don't keep the partition
    /// alive.
    unsafe fn map_memory(
        &self,
        gpa: u64,
        host: *mut u8,
        size: u64,
        access: MemoryAccess,
    ) -> Result<()>;

    fn unmap_memory(&self, gpa: u64, size: u64) -> Result<()>;

    /// Creates the vCPU with the given index (`0..vcpu_count`). Each index can be
    /// created once.
    fn create_vcpu(&self, index: u32) -> Result<Self::Vcpu>;

    /// Returns a handle that injects interrupts from any thread.
    fn interrupts(&self) -> Self::Interrupts;
}

/// A virtual processor. Runs on whichever thread calls [`Vcpu::run`].
pub trait Vcpu: Send {
    type Kicker: VcpuKicker;

    fn index(&self) -> u32;
    fn regs(&self) -> Result<StandardRegisters>;
    fn set_regs(&mut self, regs: &StandardRegisters) -> Result<()>;
    fn sregs(&self) -> Result<SpecialRegisters>;
    fn set_sregs(&mut self, sregs: &SpecialRegisters) -> Result<()>;

    /// Puts an application processor into wait-for-SIPI, where it stays until another
    /// vCPU starts it with INIT-SIPI. Needs [`LocalApicMode::XApic`] or `X2Apic`.
    fn set_waiting_for_sipi(&mut self) -> Result<()>;

    /// Runs the guest until it exits once. Port and memory accesses and EOIs go to `io`
    /// during the call, and then the exit is [`VcpuExit::Handled`].
    fn run(&mut self, io: &dyn CpuIo) -> Result<VcpuExit>;

    /// Returns a handle that can interrupt [`Vcpu::run`] from another thread.
    fn kicker(&self) -> Self::Kicker;
}

/// Interrupts a running vCPU from another thread.
pub trait VcpuKicker: Clone + Send + Sync + 'static {
    /// If the vCPU is inside [`Vcpu::run`], makes it return [`VcpuExit::Canceled`].
    /// Does nothing once the partition is gone.
    fn kick(&self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_access_flags_combine() {
        let rw = MemoryAccess::READ | MemoryAccess::WRITE;
        assert!(rw.contains(MemoryAccess::READ));
        assert!(rw.contains(MemoryAccess::WRITE));
        assert!(!rw.contains(MemoryAccess::EXECUTE));
        assert!(MemoryAccess::ALL.contains(rw | MemoryAccess::EXECUTE));
        assert_eq!(format!("{rw:?}"), "rw-");
    }
}
