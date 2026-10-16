//! A TPM 2.0 behind the Command Response Buffer (CRB) interface, from TCG's PC Client
//! Platform TPM Profile (PTP) specification.
//!
//! - **Registers:** locality 0's 4 KiB page at [`TPM_BASE`]: locality state and
//!   control, the interface ID, the CRB control area at +0x40 and the data buffer at
//!   +0x80. Drivers find the control area through the ACPI TPM2 table.
//! - **Commands:** the driver writes a command into the buffer and sets START. The
//!   command runs at once, its response replaces it in the buffer, and START reads 0,
//!   which is what drivers poll for. There are no interrupts.
//! - **The TPM itself** is a [`TpmBackend`]; it outlives the machine, so a guest reset
//!   reaches it as a platform reset.

use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use vm_core::BusDevice;

use crate::snapshot::{self, Snapshot, StateError};

/// Where the TPM's registers live (the PTP's fixed address), and how much of it.
pub const TPM_BASE: u64 = 0xfed4_0000;
pub const TPM_SIZE: u64 = 0x1000;
/// The CRB control area, which the ACPI TPM2 table points at.
pub const CONTROL_AREA: u64 = TPM_BASE + CTRL_REQ;

/// Runs TPM 2.0 commands.
pub trait TpmBackend: Send {
    /// Runs one command sent from `locality` and returns the response.
    fn execute(&mut self, locality: u8, command: &[u8]) -> Vec<u8>;
    /// The platform reset (`_TPM_Init`): PCRs clear, and TPM2_Startup comes next.
    fn reset(&mut self);
    /// All of the TPM's NV memory.
    fn nv(&self) -> Vec<u8>;
    /// Replaces the TPM's NV memory, powering it off and on: a platform reset follows.
    fn set_nv(&mut self, nv: &[u8]) -> Result<(), String>;
}

/// TPM2_Shutdown(TPM_SU_STATE) and TPM2_Startup(TPM_SU_STATE): what a suspend and the
/// resume after it send, saving the TPM's volatile state (PCRs, sessions) in NV memory
/// and taking it back from there.
const SHUTDOWN_STATE: [u8; 12] = [0x80, 0x01, 0, 0, 0, 12, 0, 0, 0x01, 0x45, 0, 1];
const STARTUP_STATE: [u8; 12] = [0x80, 0x01, 0, 0, 0, 12, 0, 0, 0x01, 0x44, 0, 1];

fn response_code(response: &[u8]) -> u32 {
    response.get(6..10).map_or(u32::MAX, |code| {
        u32::from_be_bytes(code.try_into().unwrap())
    })
}

#[derive(Serialize, Deserialize)]
struct CrbState {
    assigned: bool,
    idle: bool,
    cancel: u32,
    interrupt_enable: u32,
    interrupt_status: u32,
    buffer: Vec<u8>,
    /// The TPM's NV memory, with its volatile state in it if `started`.
    nv: Vec<u8>,
    /// The guest had started the TPM (TPM2_Startup), so it resumes.
    started: bool,
}

/// A TPM the machine shares with whoever keeps it across machine rebuilds.
pub type SharedTpm = Arc<Mutex<dyn TpmBackend>>;

const LOC_STATE: u64 = 0x00;
const LOC_CTRL: u64 = 0x08;
const LOC_STS: u64 = 0x0c;
const INTF_ID: u64 = 0x30;
const CTRL_REQ: u64 = 0x40;
const CTRL_STS: u64 = 0x44;
const CTRL_CANCEL: u64 = 0x48;
const CTRL_START: u64 = 0x4c;
const INT_ENABLE: u64 = 0x50;
const INT_STS: u64 = 0x54;
const CMD_SIZE: u64 = 0x58;
const CMD_LADDR: u64 = 0x5c;
const CMD_HADDR: u64 = 0x60;
const RSP_SIZE: u64 = 0x64;
const RSP_ADDR: u64 = 0x68;
const DATA_BUFFER: u64 = 0x80;
const BUFFER_SIZE: usize = (TPM_SIZE - DATA_BUFFER) as usize;

const LOC_STATE_ESTABLISHED: u32 = 1 << 0;
const LOC_STATE_ASSIGNED: u32 = 1 << 1;
const LOC_STATE_REG_VALID: u32 = 1 << 7;
const LOC_CTRL_REQUEST: u32 = 1 << 0;
const LOC_CTRL_RELINQUISH: u32 = 1 << 1;
const LOC_CTRL_SEIZE: u32 = 1 << 2;
const LOC_STS_GRANTED: u32 = 1 << 0;
const CTRL_REQ_CMD_READY: u32 = 1 << 0;
const CTRL_REQ_GO_IDLE: u32 = 1 << 1;
const CTRL_STS_IDLE: u32 = 1 << 1;
const START: u32 = 1 << 0;

/// The interface ID: CRB (type and version 1), 64-byte transfers, CRB capable and
/// selected (and locked there), vendor 0x1414 (Microsoft, whose reference TPM it is).
const INTERFACE_ID: u64 = 0x1 // InterfaceType: CRB
    | (0x1 << 4) // InterfaceVersion: CRB
    | (0x3 << 11) // CapDataXferSizeSupport: 64 bytes
    | (1 << 14) // CapCRB
    | (0x1 << 17) // InterfaceSelector: CRB
    | (1 << 19) // IntfSelLock
    | (0x1414 << 32) // VID
    | (0x0001 << 48); // DID

/// A TPM_RC_FAILURE response, for a response that doesn't fit the buffer.
const FAILURE_RESPONSE: [u8; 10] = [0x80, 0x01, 0, 0, 0, 10, 0, 0, 0x01, 0x01];

pub struct Crb {
    tpm: SharedTpm,
    /// Whether locality 0 holds the TPM.
    assigned: bool,
    idle: bool,
    cancel: u32,
    interrupt_enable: u32,
    interrupt_status: u32,
    buffer: Box<[u8; BUFFER_SIZE]>,
}

impl Crb {
    pub fn new(tpm: SharedTpm) -> Self {
        Self {
            tpm,
            assigned: false,
            idle: true,
            cancel: 0,
            interrupt_enable: 0,
            interrupt_status: 0,
            buffer: Box::new([0; BUFFER_SIZE]),
        }
    }

    /// The control area's registers as a driver reads them, `[CTRL_REQ, DATA_BUFFER)`
    /// and the locality registers before it.
    fn registers(&self) -> [u8; DATA_BUFFER as usize] {
        let mut image = [0u8; DATA_BUFFER as usize];
        let mut put32 = |offset: u64, value: u32| {
            image[offset as usize..offset as usize + 4].copy_from_slice(&value.to_le_bytes());
        };
        let assigned = if self.assigned { LOC_STATE_ASSIGNED } else { 0 };
        // Active locality (bits 4:2) is always 0.
        put32(
            LOC_STATE,
            LOC_STATE_REG_VALID | LOC_STATE_ESTABLISHED | assigned,
        );
        put32(LOC_STS, if self.assigned { LOC_STS_GRANTED } else { 0 });
        put32(CTRL_STS, if self.idle { CTRL_STS_IDLE } else { 0 });
        put32(CTRL_CANCEL, self.cancel);
        put32(INT_ENABLE, self.interrupt_enable);
        put32(INT_STS, self.interrupt_status);
        put32(CMD_SIZE, BUFFER_SIZE as u32);
        put32(CMD_LADDR, (TPM_BASE + DATA_BUFFER) as u32);
        put32(CMD_HADDR, ((TPM_BASE + DATA_BUFFER) >> 32) as u32);
        put32(RSP_SIZE, BUFFER_SIZE as u32);
        image[INTF_ID as usize..INTF_ID as usize + 8].copy_from_slice(&INTERFACE_ID.to_le_bytes());
        image[RSP_ADDR as usize..RSP_ADDR as usize + 8]
            .copy_from_slice(&(TPM_BASE + DATA_BUFFER).to_le_bytes());
        image
    }

    fn write_register(&mut self, register: u64, value: u32) {
        match register {
            LOC_CTRL => {
                if value & (LOC_CTRL_REQUEST | LOC_CTRL_SEIZE) != 0 {
                    self.assigned = true;
                }
                if value & LOC_CTRL_RELINQUISH != 0 {
                    self.assigned = false;
                }
            }
            CTRL_REQ => {
                if value & CTRL_REQ_CMD_READY != 0 {
                    self.idle = false;
                }
                if value & CTRL_REQ_GO_IDLE != 0 {
                    self.idle = true;
                }
            }
            CTRL_CANCEL => self.cancel = value & 1,
            CTRL_START if value & START != 0 => self.run_command(),
            INT_ENABLE => self.interrupt_enable = value,
            INT_STS => self.interrupt_status &= !value,
            // The buffer's address and size are fixed; drivers write them anyway.
            _ => {}
        }
    }

    fn run_command(&mut self) {
        let declared = u32::from_be_bytes(self.buffer[2..6].try_into().unwrap()) as usize;
        let command = &self.buffer[..declared.clamp(10, BUFFER_SIZE)];
        let response = self
            .tpm
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .execute(0, command);
        let response: &[u8] = if response.len() <= BUFFER_SIZE {
            &response
        } else {
            tracing::warn!(len = response.len(), "TPM response too big for the CRB");
            &FAILURE_RESPONSE
        };
        tracing::trace!(
            command = format_args!(
                "{:#x}",
                u32::from_be_bytes(command[6..10].try_into().unwrap())
            ),
            response = format_args!(
                "{:#x}",
                response
                    .get(6..10)
                    .map_or(0, |code| u32::from_be_bytes(code.try_into().unwrap()))
            ),
            "TPM command"
        );
        self.buffer[..response.len()].copy_from_slice(response);
        self.cancel = 0;
    }
}

impl Snapshot for Crb {
    /// Saves the TPM's volatile state into its NV memory, as a suspend does, takes a
    /// copy, and resumes it: the guest carries on with its PCRs and sessions as they
    /// were.
    fn save(&self) -> Vec<u8> {
        let mut tpm = self.tpm.lock().unwrap_or_else(PoisonError::into_inner);
        // A TPM the guest hasn't started yet refuses the shutdown; its NV is all it has.
        let started = response_code(&tpm.execute(0, &SHUTDOWN_STATE)) == 0;
        let nv = tpm.nv();
        if started {
            tpm.reset();
            let code = response_code(&tpm.execute(0, &STARTUP_STATE));
            if code != 0 {
                tracing::warn!(code, "the TPM didn't resume after a snapshot");
            }
        }
        snapshot::encode(&CrbState {
            assigned: self.assigned,
            idle: self.idle,
            cancel: self.cancel,
            interrupt_enable: self.interrupt_enable,
            interrupt_status: self.interrupt_status,
            buffer: self.buffer.to_vec(),
            nv,
            started,
        })
    }

    fn restore(&mut self, state: &[u8]) -> Result<(), StateError> {
        let state: CrbState = snapshot::decode(state)?;
        let mut tpm = self.tpm.lock().unwrap_or_else(PoisonError::into_inner);
        tpm.set_nv(&state.nv)
            .map_err(|e| StateError(format!("TPM: {e}")))?;
        if state.started {
            let code = response_code(&tpm.execute(0, &STARTUP_STATE));
            if code != 0 {
                return Err(StateError(format!(
                    "the TPM didn't resume from the saved state (response code {code:#x})"
                )));
            }
        }
        drop(tpm);
        self.assigned = state.assigned;
        self.idle = state.idle;
        self.cancel = state.cancel;
        self.interrupt_enable = state.interrupt_enable;
        self.interrupt_status = state.interrupt_status;
        self.buffer = state
            .buffer
            .try_into()
            .map_err(|_| StateError("the TPM buffer was a different size".into()))?;
        Ok(())
    }
}

impl BusDevice for Crb {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        if offset >= DATA_BUFFER {
            let start = (offset - DATA_BUFFER) as usize;
            for (i, byte) in data.iter_mut().enumerate() {
                *byte = self.buffer.get(start + i).copied().unwrap_or(0xff);
            }
            return;
        }
        let image = self.registers();
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = image.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if offset >= DATA_BUFFER {
            let start = (offset - DATA_BUFFER) as usize;
            for (i, &byte) in data.iter().enumerate() {
                if let Some(slot) = self.buffer.get_mut(start + i) {
                    *slot = byte;
                }
            }
            return;
        }
        // Registers are 32 bits; a narrower write lands in its register's value.
        let register = offset & !3;
        let mut value = [0u8; 4];
        let shift = (offset - register) as usize;
        for (i, &byte) in data.iter().take(4 - shift).enumerate() {
            value[shift + i] = byte;
        }
        self.write_register(register, u32::from_le_bytes(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Echoes commands back with a success code, and counts resets.
    #[derive(Default)]
    struct Echo {
        commands: Vec<Vec<u8>>,
    }

    impl TpmBackend for Echo {
        fn nv(&self) -> Vec<u8> {
            Vec::new()
        }

        fn set_nv(&mut self, _: &[u8]) -> Result<(), String> {
            Ok(())
        }

        fn execute(&mut self, _: u8, command: &[u8]) -> Vec<u8> {
            self.commands.push(command.to_vec());
            let mut response = vec![0x80, 0x01, 0, 0, 0, 12, 0, 0, 0, 0];
            response.extend_from_slice(&command[command.len() - 2..]);
            response
        }

        fn reset(&mut self) {}
    }

    fn crb() -> (Crb, Arc<Mutex<Echo>>) {
        let echo = Arc::new(Mutex::new(Echo::default()));
        (Crb::new(echo.clone()), echo)
    }

    fn read32(crb: &mut Crb, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        crb.read(offset, &mut data);
        u32::from_le_bytes(data)
    }

    #[test]
    fn identifies_as_a_crb_with_its_buffer_inside_the_page() {
        let (mut crb, _) = crb();
        let mut id = [0u8; 8];
        crb.read(INTF_ID, &mut id);
        let id = u64::from_le_bytes(id);
        assert_eq!(id & 0xf, 1, "CRB interface type");
        assert_ne!(id & (1 << 14), 0, "CRB capable");
        assert_eq!(read32(&mut crb, CMD_LADDR) as u64, TPM_BASE + DATA_BUFFER);
        assert_eq!(read32(&mut crb, CMD_SIZE) as usize, BUFFER_SIZE);
        let mut rsp = [0u8; 8];
        crb.read(RSP_ADDR, &mut rsp);
        assert_eq!(u64::from_le_bytes(rsp), TPM_BASE + DATA_BUFFER);
        assert_eq!(BUFFER_SIZE, 0xf80);
    }

    #[test]
    fn locality_and_idle_handshakes() {
        let (mut crb, _) = crb();
        assert_eq!(read32(&mut crb, LOC_STATE) & LOC_STATE_ASSIGNED, 0);
        crb.write(LOC_CTRL, &LOC_CTRL_REQUEST.to_le_bytes());
        assert_eq!(
            read32(&mut crb, LOC_STATE) & (LOC_STATE_ASSIGNED | LOC_STATE_REG_VALID),
            LOC_STATE_ASSIGNED | LOC_STATE_REG_VALID
        );
        assert_eq!(read32(&mut crb, LOC_STS), LOC_STS_GRANTED);
        assert_eq!(read32(&mut crb, CTRL_STS), CTRL_STS_IDLE);
        crb.write(CTRL_REQ, &CTRL_REQ_CMD_READY.to_le_bytes());
        assert_eq!(
            read32(&mut crb, CTRL_REQ),
            0,
            "request bits clear themselves"
        );
        assert_eq!(read32(&mut crb, CTRL_STS), 0, "ready");
        crb.write(CTRL_REQ, &CTRL_REQ_GO_IDLE.to_le_bytes());
        assert_eq!(read32(&mut crb, CTRL_STS), CTRL_STS_IDLE);
        crb.write(LOC_CTRL, &LOC_CTRL_RELINQUISH.to_le_bytes());
        assert_eq!(read32(&mut crb, LOC_STS), 0);
    }

    #[test]
    fn start_runs_the_command_in_the_buffer() {
        let (mut crb, echo) = crb();
        // TPM2_GetRandom(0x0008): 12 bytes, with 4 stale bytes after it.
        let command = [
            0x80, 0x01, 0, 0, 0, 12, 0, 0, 0x01, 0x7b, 0, 8, 0xee, 0xee, 0xee, 0xee,
        ];
        crb.write(DATA_BUFFER, &command);
        crb.write(CTRL_START, &START.to_le_bytes());
        assert_eq!(read32(&mut crb, CTRL_START), 0, "done");
        assert_eq!(echo.lock().unwrap().commands, [command[..12].to_vec()]);
        let mut response = [0u8; 12];
        crb.read(DATA_BUFFER, &mut response);
        assert_eq!(response, [0x80, 0x01, 0, 0, 0, 12, 0, 0, 0, 0, 0, 8]);
    }
}
