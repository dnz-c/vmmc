//! The vmmc engine core: guest memory, I/O buses and the vCPU run loop.
//!
//! Everything here is written against the backend-neutral `hv` traits, so the same code
//! runs on WHP today and KVM later.

mod bus;
mod vm;

pub use bus::{Bus, BusDevice, SharedDevice};
pub use vm::{StopHandle, StopReason, Vm, VmExit, VmVcpu};
pub use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Hypervisor(#[from] hv::Error),
    #[error("could not allocate guest memory: {0}")]
    MemoryAllocation(#[from] vm_memory::mmap::FromRangesError),
    #[error("guest memory access failed: {0}")]
    GuestMemory(#[from] vm_memory::GuestMemoryError),
    #[error("bus range {base:#x}+{len:#x} is empty, wraps around, or overlaps another device")]
    BusRange { base: u64, len: u64 },
}
