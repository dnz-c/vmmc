//! Saved vCPU and partition state, for snapshots and suspend.

use serde::{Deserialize, Serialize};

/// A vCPU's whole state as its backend saved it: registers by the backend's own names,
/// and what the backend saves whole (the FPU and vector registers, the local APIC, the
/// synthetic interrupt controller's pages and timers). Only the backend that saved it
/// can restore it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcpuState {
    /// The backend that saved it, such as `"whp"`.
    pub backend: String,
    /// (the backend's register name, value).
    pub registers: Vec<(u32, u128)>,
    /// (the backend's state type, contents).
    pub blobs: Vec<(u32, Vec<u8>)>,
}

impl VcpuState {
    /// The value of the register the backend calls `name`, if it was saved.
    pub fn register(&self, name: u32) -> Option<u128> {
        self.registers
            .iter()
            .find(|&&(saved, _)| saved == name)
            .map(|&(_, value)| value)
    }

    /// The contents of the state the backend calls `kind`, if it was saved.
    pub fn blob(&self, kind: u32) -> Option<&[u8]> {
        self.blobs
            .iter()
            .find(|(saved, _)| *saved == kind)
            .map(|(_, data)| data.as_slice())
    }
}

/// A partition's own state: its clock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionState {
    /// The partition's reference time, in 100 ns units: what the guest's Hyper-V
    /// reference counter reads.
    pub reference_time: u64,
}
