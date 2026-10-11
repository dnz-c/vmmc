//! The Q35 chipset functions firmware programs: the MCH host bridge and the ICH9 LPC
//! bridge. OVMF identifies the platform by them and sets them up before anything else.

pub mod lpc;
pub mod mch;

/// Whether a config write at `offset..offset + len` touches `register..register + size`.
fn touches(offset: u16, len: usize, register: u16, size: u16) -> bool {
    let end = usize::from(offset) + len;
    usize::from(register) < end && usize::from(offset) < usize::from(register + size)
}
