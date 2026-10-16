use std::fmt;
use std::sync::{Arc, Weak};

use hv::{CpuIo, SpecialRegisters, StandardRegisters, VcpuExit};
use windows::Win32::System::Hypervisor::{
    WHV_REGISTER_NAME, WHV_REGISTER_VALUE, WHV_RUN_VP_EXIT_CONTEXT, WHV_VP_EXIT_CONTEXT,
    WHvRegisterInternalActivityState, WHvRunVpExitReasonCanceled,
    WHvRunVpExitReasonInvalidVpRegisterValue, WHvRunVpExitReasonMemoryAccess,
    WHvRunVpExitReasonUnrecoverableException, WHvRunVpExitReasonX64ApicEoi,
    WHvRunVpExitReasonX64Cpuid, WHvRunVpExitReasonX64Halt, WHvRunVpExitReasonX64IoPortAccess,
    WHvX64RegisterRax, WHvX64RegisterRbx, WHvX64RegisterRcx, WHvX64RegisterRdx, WHvX64RegisterRip,
};

use crate::api::check;
use crate::cpuid;
use crate::emulator::{Context, Emulator};
use crate::partition::PartitionInner;
use crate::regs::{self, Aligned16};

/// `WHV_INTERNAL_ACTIVITY_REGISTER.StartupSuspend`: the processor waits for a SIPI.
const STARTUP_SUSPEND: u64 = 1;

/// A WHP virtual processor.
pub struct WhpVcpu {
    pub(crate) partition: Arc<PartitionInner>,
    pub(crate) index: u32,
    exit: Aligned16<WHV_RUN_VP_EXIT_CONTEXT>,
    emulator: Emulator,
}

impl WhpVcpu {
    pub(crate) fn create(partition: Arc<PartitionInner>, index: u32) -> hv::Result<Self> {
        let emulator = Emulator::new(partition.api.clone())?;
        // SAFETY: plain call on a valid partition handle.
        check("WHvCreateVirtualProcessor", unsafe {
            (partition.api.WHvCreateVirtualProcessor)(partition.handle, index, 0)
        })?;
        Ok(Self {
            partition,
            index,
            exit: Aligned16::default(),
            emulator,
        })
    }

    fn get_registers<const N: usize>(
        &self,
        names: &[WHV_REGISTER_NAME; N],
    ) -> hv::Result<[WHV_REGISTER_VALUE; N]> {
        let mut values = Aligned16([WHV_REGISTER_VALUE::default(); N]);
        // SAFETY: `names` and `values` both hold N elements, and `values` is 16-byte
        // aligned as WHP requires.
        check("WHvGetVirtualProcessorRegisters", unsafe {
            (self.partition.api.WHvGetVirtualProcessorRegisters)(
                self.partition.handle,
                self.index,
                names.as_ptr(),
                N as u32,
                values.as_mut_ptr(),
            )
        })?;
        Ok(values.0)
    }

    fn set_registers<const N: usize>(
        &self,
        names: &[WHV_REGISTER_NAME; N],
        values: &[WHV_REGISTER_VALUE; N],
    ) -> hv::Result<()> {
        let values = Aligned16(*values);
        // SAFETY: `names` and `values` both hold N elements, and `values` is 16-byte
        // aligned as WHP requires.
        check("WHvSetVirtualProcessorRegisters", unsafe {
            (self.partition.api.WHvSetVirtualProcessorRegisters)(
                self.partition.handle,
                self.index,
                names.as_ptr(),
                N as u32,
                values.as_ptr(),
            )
        })
    }

    fn io_port_exit(&mut self, io: &dyn CpuIo) -> hv::Result<VcpuExit> {
        // SAFETY: WHP fills the IoPortAccess member for this exit reason.
        let access = unsafe { self.exit.Anonymous.IoPortAccess };
        // SAFETY: AsUINT32 spans the whole bitfield union.
        let info = IoAccess::decode(unsafe { access.AccessInfo.AsUINT32 });
        let port = access.PortNumber;

        if info.string {
            // INS/OUTS move data between ports and guest memory, so let the emulator
            // decode them.
            let context = Context::new(&self.partition, self.index, io);
            // Hand the emulator WHP's own (aligned) exit context, not a stack copy.
            // SAFETY: WHP fills the IoPortAccess member for this exit reason.
            let access = unsafe { &self.exit.Anonymous.IoPortAccess };
            return Ok(
                match self.emulator.io(&context, &self.exit.VpContext, access)? {
                    Ok(()) => VcpuExit::Handled,
                    Err(reason) => VcpuExit::Unhandled(format!("port {port:#x}: {reason}")),
                },
            );
        }

        let size = info.size;
        if !matches!(size, 1 | 2 | 4) {
            return Ok(VcpuExit::Unhandled(format!(
                "{size}-byte I/O on port {port:#x}"
            )));
        }
        let next_rip = self.exit.VpContext.Rip + instruction_length(&self.exit.VpContext);
        if info.is_write {
            io.io_write(port, &access.Rax.to_le_bytes()[..size]);
            self.set_registers(&[WHvX64RegisterRip], &[regs::reg64(next_rip)])?;
        } else {
            let mut data = [0u8; 4];
            io.io_read(port, &mut data[..size]);
            let rax = merge_in_result(access.Rax, &data[..size]);
            self.set_registers(
                &[WHvX64RegisterRax, WHvX64RegisterRip],
                &[regs::reg64(rax), regs::reg64(next_rip)],
            )?;
        }
        Ok(VcpuExit::Handled)
    }

    /// Answers CPUID for the leaves in `cpuid::LEAVES`, from WHP's own results.
    fn cpuid_exit(&mut self) -> hv::Result<VcpuExit> {
        // SAFETY: WHP fills the CpuidAccess member for this exit reason.
        let access = unsafe { self.exit.Anonymous.CpuidAccess };
        let default = [
            access.DefaultResultRax as u32,
            access.DefaultResultRbx as u32,
            access.DefaultResultRcx as u32,
            access.DefaultResultRdx as u32,
        ];
        // WHP's initial APIC IDs are the vCPU indexes.
        let [eax, ebx, ecx, edx] = cpuid::adjust(
            access.Rax as u32,
            access.Rcx as u32,
            default,
            self.index,
            self.partition.vcpus,
        );
        let next_rip = self.exit.VpContext.Rip + instruction_length(&self.exit.VpContext);
        self.set_registers(
            &[
                WHvX64RegisterRax,
                WHvX64RegisterRbx,
                WHvX64RegisterRcx,
                WHvX64RegisterRdx,
                WHvX64RegisterRip,
            ],
            &[
                regs::reg64(eax.into()),
                regs::reg64(ebx.into()),
                regs::reg64(ecx.into()),
                regs::reg64(edx.into()),
                regs::reg64(next_rip),
            ],
        )?;
        Ok(VcpuExit::Handled)
    }

    fn memory_exit(&mut self, io: &dyn CpuIo) -> hv::Result<VcpuExit> {
        // SAFETY: WHP fills the MemoryAccess member for this exit reason. The emulator
        // gets WHP's own (aligned) exit context, not a stack copy.
        let access = unsafe { &self.exit.Anonymous.MemoryAccess };
        let context = Context::new(&self.partition, self.index, io);
        Ok(
            match self.emulator.mmio(&context, &self.exit.VpContext, access)? {
                Ok(()) => VcpuExit::Handled,
                Err(reason) => VcpuExit::Unhandled(format!(
                    "GPA {:#x} at RIP {:#x}: {reason}",
                    access.Gpa, self.exit.VpContext.Rip
                )),
            },
        )
    }
}

impl hv::Vcpu for WhpVcpu {
    type Kicker = WhpKicker;

    fn index(&self) -> u32 {
        self.index
    }

    fn regs(&self) -> hv::Result<StandardRegisters> {
        Ok(regs::standard_from_values(
            &self.get_registers(&regs::STANDARD_NAMES)?,
        ))
    }

    fn set_regs(&mut self, r: &StandardRegisters) -> hv::Result<()> {
        self.set_registers(&regs::STANDARD_NAMES, &regs::standard_to_values(r))
    }

    fn sregs(&self) -> hv::Result<SpecialRegisters> {
        Ok(regs::special_from_values(
            &self.get_registers(&regs::SPECIAL_NAMES)?,
        ))
    }

    fn set_sregs(&mut self, s: &SpecialRegisters) -> hv::Result<()> {
        self.set_registers(&regs::SPECIAL_NAMES, &regs::special_to_values(s))
    }

    fn save_state(&self) -> hv::Result<hv::VcpuState> {
        crate::state::save_vcpu(self)
    }

    fn restore_state(&mut self, state: &hv::VcpuState) -> hv::Result<()> {
        crate::state::restore_vcpu(self, state)
    }

    fn local_apic(&self) -> hv::Result<Option<Vec<u8>>> {
        let mut page = vec![0u8; 4096];
        let mut written = 0u32;
        // SAFETY: `page` is a writable buffer of the size passed, and `written` a valid
        // out-pointer.
        check("WHvGetVirtualProcessorInterruptControllerState2", unsafe {
            (self
                .partition
                .api
                .WHvGetVirtualProcessorInterruptControllerState2)(
                self.partition.handle,
                self.index,
                page.as_mut_ptr().cast(),
                page.len() as u32,
                &mut written,
            )
        })?;
        page.truncate(written as usize);
        Ok(Some(page))
    }

    fn set_waiting_for_sipi(&mut self) -> hv::Result<()> {
        let mut value = WHV_REGISTER_VALUE::default();
        value.InternalActivity.AsUINT64 = STARTUP_SUSPEND;
        self.set_registers(&[WHvRegisterInternalActivityState], &[value])
    }

    // The WHP constants matched below keep their Windows SDK names.
    #[allow(non_upper_case_globals)]
    fn run(&mut self, io: &dyn CpuIo) -> hv::Result<VcpuExit> {
        // SAFETY: `self.exit` is a writable WHV_RUN_VP_EXIT_CONTEXT of the size passed.
        check("WHvRunVirtualProcessor", unsafe {
            (self.partition.api.WHvRunVirtualProcessor)(
                self.partition.handle,
                self.index,
                (&raw mut self.exit).cast(),
                size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
            )
        })?;

        match self.exit.ExitReason {
            WHvRunVpExitReasonX64IoPortAccess => self.io_port_exit(io),
            WHvRunVpExitReasonMemoryAccess => self.memory_exit(io),
            WHvRunVpExitReasonX64ApicEoi => {
                // SAFETY: WHP fills the ApicEoi member for this exit reason.
                let vector = unsafe { self.exit.Anonymous.ApicEoi }.InterruptVector;
                io.eoi(vector as u8);
                Ok(VcpuExit::Handled)
            }
            WHvRunVpExitReasonX64Cpuid => self.cpuid_exit(),
            WHvRunVpExitReasonX64Halt => Ok(VcpuExit::Halt),
            WHvRunVpExitReasonCanceled => Ok(VcpuExit::Canceled),
            WHvRunVpExitReasonUnrecoverableException => Ok(VcpuExit::Shutdown),
            WHvRunVpExitReasonInvalidVpRegisterValue => {
                Ok(VcpuExit::Unhandled("invalid vCPU register state".into()))
            }
            other => Ok(VcpuExit::Unhandled(format!(
                "WHP exit reason {:#x}",
                other.0
            ))),
        }
    }

    fn kicker(&self) -> WhpKicker {
        WhpKicker {
            partition: Arc::downgrade(&self.partition),
            index: self.index,
        }
    }
}

impl Drop for WhpVcpu {
    fn drop(&mut self) {
        // SAFETY: plain call on a valid partition handle; this vCPU isn't running because
        // `run` takes `&mut self`.
        let _ = unsafe {
            (self.partition.api.WHvDeleteVirtualProcessor)(self.partition.handle, self.index)
        };
    }
}

impl fmt::Debug for WhpVcpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WhpVcpu")
            .field("partition", &self.partition)
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

/// Interrupts a [`WhpVcpu`] from another thread.
#[derive(Debug, Clone)]
pub struct WhpKicker {
    partition: Weak<PartitionInner>,
    index: u32,
}

impl hv::VcpuKicker for WhpKicker {
    fn kick(&self) {
        let Some(partition) = self.partition.upgrade() else {
            return;
        };
        // SAFETY: plain call on a valid partition handle. It fails only if the vCPU is
        // gone, in which case there is nothing to interrupt.
        let _ = unsafe {
            (partition.api.WHvCancelRunVirtualProcessor)(partition.handle, self.index, 0)
        };
    }
}

/// The fields of `WHV_X64_IO_PORT_ACCESS_INFO` this backend uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IoAccess {
    is_write: bool,
    /// Access size in bytes.
    size: usize,
    /// INS/OUTS rather than IN/OUT.
    string: bool,
}

impl IoAccess {
    /// Bit 0 IsWrite, bits 1..4 AccessSize, bit 4 StringOp, bit 5 RepPrefix.
    fn decode(bits: u32) -> Self {
        Self {
            is_write: bits & 1 != 0,
            size: ((bits >> 1) & 0b111) as usize,
            string: bits & (1 << 4) != 0,
        }
    }
}

/// Length of the exiting instruction: the low 4 bits of the bitfield after
/// `ExecutionState` (the high 4 bits are CR8).
fn instruction_length(vp: &WHV_VP_EXIT_CONTEXT) -> u64 {
    u64::from(vp._bitfield & 0x0f)
}

/// Puts the bytes an IN instruction read into RAX the way the CPU does: 8- and 16-bit
/// reads keep the rest of RAX, a 32-bit read zero-extends into all 64 bits.
fn merge_in_result(rax: u64, data: &[u8]) -> u64 {
    match *data {
        [b] => (rax & !0xff) | u64::from(b),
        [b0, b1] => (rax & !0xffff) | u64::from(u16::from_le_bytes([b0, b1])),
        [b0, b1, b2, b3] => u64::from(u32::from_le_bytes([b0, b1, b2, b3])),
        _ => unreachable!("I/O size is checked to be 1, 2 or 4"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_io_access_info() {
        // OUT DX, AL: write, 1 byte.
        assert_eq!(
            IoAccess::decode(0b0_0011),
            IoAccess {
                is_write: true,
                size: 1,
                string: false
            }
        );
        // IN EAX, DX: read, 4 bytes.
        assert_eq!(
            IoAccess::decode(0b0_1000),
            IoAccess {
                is_write: false,
                size: 4,
                string: false
            }
        );
        // REP OUTSW: write, 2 bytes, string op, rep prefix.
        assert_eq!(
            IoAccess::decode(0b11_0101),
            IoAccess {
                is_write: true,
                size: 2,
                string: true
            }
        );
    }

    #[test]
    fn instruction_length_ignores_cr8_bits() {
        let vp = WHV_VP_EXIT_CONTEXT {
            _bitfield: 0xf2,
            ..Default::default()
        };
        assert_eq!(instruction_length(&vp), 2);
    }

    #[test]
    fn in_results_merge_into_rax_like_the_cpu() {
        let rax = 0x1122_3344_5566_7788;
        assert_eq!(merge_in_result(rax, &[0xaa]), 0x1122_3344_5566_77aa);
        assert_eq!(merge_in_result(rax, &[0xaa, 0xbb]), 0x1122_3344_5566_bbaa);
        assert_eq!(merge_in_result(rax, &[0xaa, 0xbb, 0xcc, 0xdd]), 0xddcc_bbaa);
    }
}
