//! MSI-X: the capability, the vector table and the pending bit array.

use std::sync::Arc;

use super::ConfigSpace;
use crate::irq::{MsiMessage, MsiSink};

pub const CAPABILITY_ID: u8 = 0x11;
/// Bytes per table entry: address low, address high, data, vector control.
pub const ENTRY_SIZE: u64 = 16;

const CONTROL_FUNCTION_MASK: u16 = 1 << 14;
const CONTROL_ENABLE: u16 = 1 << 15;
const VECTOR_MASKED: u32 = 1;

/// Adds the MSI-X capability. The table and PBA live in the given BARs at the given
/// offsets. Returns the capability's offset.
pub fn add_capability(
    config: &mut ConfigSpace,
    vectors: u16,
    table: (u8, u32),
    pba: (u8, u32),
) -> u16 {
    assert!((1..=2048).contains(&vectors));
    let mut body = Vec::with_capacity(10);
    body.extend_from_slice(&(vectors - 1).to_le_bytes());
    body.extend_from_slice(&(table.1 | u32::from(table.0)).to_le_bytes());
    body.extend_from_slice(&(pba.1 | u32::from(pba.0)).to_le_bytes());
    let offset = config.add_capability(CAPABILITY_ID, &body);
    config.set_wmask_u16(offset + 2, CONTROL_FUNCTION_MASK | CONTROL_ENABLE);
    offset
}

/// (enabled, function masked) from the capability at `offset`.
pub fn control(config: &ConfigSpace, offset: u16) -> (bool, bool) {
    let control = config.u16_at(offset + 2);
    (
        control & CONTROL_ENABLE != 0,
        control & CONTROL_FUNCTION_MASK != 0,
    )
}

/// Size in bytes of the PBA for `vectors` vectors.
pub fn pba_size(vectors: u16) -> u64 {
    u64::from(vectors).div_ceil(64) * 8
}

pub struct MsixTable {
    /// Address low, address high, data, vector control.
    entries: Vec<[u32; 4]>,
    pending: Vec<bool>,
    enabled: bool,
    function_masked: bool,
    sink: Arc<dyn MsiSink>,
}

impl MsixTable {
    /// A table with every vector masked, as after reset.
    pub fn new(vectors: u16, sink: Arc<dyn MsiSink>) -> Self {
        Self {
            entries: vec![[0, 0, 0, VECTOR_MASKED]; usize::from(vectors)],
            pending: vec![false; usize::from(vectors)],
            enabled: false,
            function_masked: false,
            sink,
        }
    }

    pub fn len(&self) -> u16 {
        self.entries.len() as u16
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether MSI-X is on, so the device signals through it rather than INTx.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Applies the capability's enable and function-mask bits.
    pub fn set_control(&mut self, enabled: bool, function_masked: bool) {
        self.enabled = enabled;
        self.function_masked = function_masked;
        self.deliver_pending();
    }

    /// Back to reset state: all vectors masked, nothing pending, disabled.
    pub fn reset(&mut self) {
        self.entries.fill([0, 0, 0, VECTOR_MASKED]);
        self.pending.fill(false);
        self.enabled = false;
        self.function_masked = false;
    }

    /// Signals `vector`. Delivered now if MSI-X is on and the vector is unmasked,
    /// otherwise left pending until it is.
    pub fn notify(&mut self, vector: u16) {
        let Some(entry) = self.entries.get(usize::from(vector)) else {
            tracing::debug!(vector, "MSI-X vector out of range");
            return;
        };
        let deliverable = self.deliverable(entry);
        tracing::trace!(
            vector,
            deliverable,
            enabled = self.enabled,
            function_masked = self.function_masked,
            entry_masked = entry[3] & VECTOR_MASKED != 0,
            "MSI-X notify"
        );
        if deliverable {
            self.sink.send(message(entry));
        } else {
            self.pending[usize::from(vector)] = true;
        }
    }

    fn deliverable(&self, entry: &[u32; 4]) -> bool {
        self.enabled && !self.function_masked && entry[3] & VECTOR_MASKED == 0
    }

    fn deliver_pending(&mut self) {
        for vector in 0..self.entries.len() {
            if self.pending[vector] && self.deliverable(&self.entries[vector]) {
                self.pending[vector] = false;
                self.sink.send(message(&self.entries[vector]));
            }
        }
    }

    pub fn read_table(&self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            let at = offset + i as u64;
            let entry = (at / ENTRY_SIZE) as usize;
            let word = ((at % ENTRY_SIZE) / 4) as usize;
            *byte = self
                .entries
                .get(entry)
                .map_or(0, |entry| entry[word].to_le_bytes()[(at % 4) as usize]);
        }
    }

    pub fn write_table(&mut self, offset: u64, data: &[u8]) {
        for (i, &byte) in data.iter().enumerate() {
            let at = offset + i as u64;
            let entry = (at / ENTRY_SIZE) as usize;
            let word = ((at % ENTRY_SIZE) / 4) as usize;
            let Some(entry) = self.entries.get_mut(entry) else {
                continue;
            };
            let mut bytes = entry[word].to_le_bytes();
            bytes[(at % 4) as usize] = byte;
            entry[word] = u32::from_le_bytes(bytes);
        }
        // Unmasking a vector delivers anything pending on it.
        self.deliver_pending();
    }

    pub fn read_pba(&self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            let first = ((offset + i as u64) * 8) as usize;
            *byte = (0..8)
                .filter(|bit| self.pending.get(first + bit).copied().unwrap_or(false))
                .fold(0, |acc, bit| acc | (1 << bit));
        }
    }
}

fn message(entry: &[u32; 4]) -> MsiMessage {
    MsiMessage {
        address: u64::from(entry[0]) | (u64::from(entry[1]) << 32),
        data: entry[2],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irq::testing::RecordingSink;
    use crate::pci::Identity;

    fn table(vectors: u16) -> (MsixTable, Arc<RecordingSink>) {
        let sink = Arc::new(RecordingSink::default());
        (MsixTable::new(vectors, sink.clone()), sink)
    }

    fn program(table: &mut MsixTable, vector: u16, address: u32, data: u32, masked: bool) {
        let base = u64::from(vector) * ENTRY_SIZE;
        table.write_table(base, &address.to_le_bytes());
        table.write_table(base + 4, &0u32.to_le_bytes());
        table.write_table(base + 8, &data.to_le_bytes());
        table.write_table(base + 12, &u32::from(masked).to_le_bytes());
    }

    #[test]
    fn capability_layout_and_control_bits() {
        let mut config = ConfigSpace::new(Identity {
            vendor_id: 1,
            device_id: 2,
            revision: 0,
            class: 0,
            subsystem_vendor_id: 0,
            subsystem_id: 0,
        });
        let cap = add_capability(&mut config, 3, (2, 0), (2, 0x1000));
        assert_eq!(config.u8_at(cap), CAPABILITY_ID);
        assert_eq!(config.u16_at(cap + 2), 2, "table size is N-1");
        assert_eq!(config.u32_at(cap + 4), 2, "table at BAR 2 offset 0");
        assert_eq!(config.u32_at(cap + 8), 0x1002);

        config.write(cap + 2, &0xffffu16.to_le_bytes());
        assert_eq!(control(&config, cap), (true, true));
        assert_eq!(config.u16_at(cap + 2) & 0x7ff, 2, "table size is read-only");
    }

    #[test]
    fn delivers_only_when_enabled_and_unmasked_then_flushes_pending() {
        let (mut table, sink) = table(2);
        program(&mut table, 1, 0xfee0_1000, 0x45, false);

        table.notify(1);
        assert!(sink.take().is_empty(), "MSI-X disabled");
        let mut pba = [0u8; 1];
        table.read_pba(0, &mut pba);
        assert_eq!(pba[0], 0b10);

        table.set_control(true, false);
        assert_eq!(
            sink.take(),
            [MsiMessage {
                address: 0xfee0_1000,
                data: 0x45
            }]
        );
        table.read_pba(0, &mut pba);
        assert_eq!(pba[0], 0);

        table.notify(1);
        assert_eq!(sink.take().len(), 1, "direct delivery");
    }

    #[test]
    fn masked_vector_pends_until_unmasked() {
        let (mut table, sink) = table(1);
        table.set_control(true, false);
        program(&mut table, 0, 0xfee0_0000, 0x30, true);
        table.notify(0);
        assert!(sink.take().is_empty());
        table.write_table(12, &0u32.to_le_bytes());
        assert_eq!(sink.take().len(), 1);
    }

    #[test]
    fn table_reads_back_what_was_written() {
        let (mut table, _) = table(1);
        program(&mut table, 0, 0xfee0_2000, 0x1234, false);
        let mut entry = [0u8; 16];
        table.read_table(0, &mut entry);
        assert_eq!(&entry[0..4], &0xfee0_2000u32.to_le_bytes());
        assert_eq!(&entry[8..12], &0x1234u32.to_le_bytes());
        assert_eq!(&entry[12..16], &0u32.to_le_bytes());
    }
}
