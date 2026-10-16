//! The PCI root complex: bus 0, reached through port I/O (CF8/CFC) and ECAM.
//!
//! When a guest moves a BAR or toggles memory decode, the root moves the device's
//! mapping on the MMIO bus to match.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

use vm_core::{Bus, BusDevice};

use super::{BarMapping, SharedPciDevice};
use crate::snapshot::{self, Snapshot, StateError};
use crate::{PowerEvent, PowerHandler};

/// Port I/O configuration mechanism #1: address at 0xcf8, data at 0xcfc.
pub const CONFIG_IO_BASE: u16 = 0xcf8;
pub const CONFIG_IO_PORTS: u16 = 8;
/// ECAM window: 1 MiB per bus, 256 buses.
pub const ECAM_SIZE: u64 = 256 << 20;
pub const NUM_SLOTS: u8 = 32;

pub const NUM_FUNCTIONS: u8 = 8;
/// Header type register; bit 7 marks a multi-function device.
const HEADER_TYPE: u16 = 0x0e;
const MULTI_FUNCTION: u8 = 0x80;

struct Function {
    device: SharedPciDevice,
    /// The BAR mappings currently on the MMIO bus.
    mapped: Vec<BarMapping>,
}

pub struct PciRoot {
    /// Keyed by (slot, function).
    functions: BTreeMap<(u8, u8), Function>,
    mmio: Arc<Bus>,
}

impl PciRoot {
    pub fn new(mmio: Arc<Bus>) -> Self {
        Self {
            functions: BTreeMap::new(),
            mmio,
        }
    }

    /// Plugs `device` in as function 0 of `slot` on bus 0.
    pub fn add_device(&mut self, slot: u8, device: SharedPciDevice) {
        self.add_function(slot, 0, device);
    }

    /// Plugs `device` in as `function` of `slot` on bus 0, and maps any BARs it already
    /// has decoding.
    pub fn add_function(&mut self, slot: u8, function: u8, device: SharedPciDevice) {
        assert!(
            slot < NUM_SLOTS && function < NUM_FUNCTIONS,
            "PCI {slot}.{function}"
        );
        let key = (slot, function);
        assert!(
            !self.functions.contains_key(&key),
            "PCI {slot}.{function} taken"
        );
        self.functions.insert(
            key,
            Function {
                device,
                mapped: Vec::new(),
            },
        );
        self.sync_bars(key);
    }

    /// Every memory range a device's BARs currently claim, decoding or not.
    pub fn assigned_bar_ranges(&self) -> Vec<(u64, u64)> {
        self.functions
            .values()
            .flat_map(|function| {
                function
                    .device
                    .lock()
                    .expect("a PCI device panicked")
                    .config()
                    .assigned_bars()
            })
            .map(|bar| (bar.address, bar.size))
            .collect()
    }

    pub fn config_read(&self, bus: u8, slot: u8, function: u8, offset: u16, data: &mut [u8]) {
        let Some(entry) = self.function(bus, slot, function) else {
            // Nothing there: reads float high, which is how guests detect empty slots.
            data.fill(0xff);
            return;
        };
        entry
            .device
            .lock()
            .expect("a PCI device panicked")
            .config_read(offset, data);
        // Function 0 of a slot with more functions advertises them in its header type.
        let header = usize::from(HEADER_TYPE.wrapping_sub(offset));
        if function == 0
            && (offset..offset + data.len() as u16).contains(&HEADER_TYPE)
            && self
                .functions
                .range((slot, 1)..=(slot, NUM_FUNCTIONS - 1))
                .next()
                .is_some()
        {
            data[header] |= MULTI_FUNCTION;
        }
    }

    pub fn config_write(&mut self, bus: u8, slot: u8, function: u8, offset: u16, data: &[u8]) {
        let Some(entry) = self.function(bus, slot, function) else {
            return;
        };
        entry
            .device
            .lock()
            .expect("a PCI device panicked")
            .config_write(offset, data);
        self.sync_bars((slot, function));
    }

    fn function(&self, bus: u8, slot: u8, function: u8) -> Option<&Function> {
        if bus != 0 {
            return None;
        }
        self.functions.get(&(slot, function))
    }

    /// Makes the MMIO bus match the BARs the function at `key` is decoding.
    fn sync_bars(&mut self, key: (u8, u8)) {
        let entry = self.functions.get_mut(&key).expect("function exists");
        let (slot, _) = key;
        let wanted = entry
            .device
            .lock()
            .expect("a PCI device panicked")
            .config()
            .bar_mappings();
        if wanted == entry.mapped {
            return;
        }
        for old in &entry.mapped {
            if !wanted.contains(old) {
                self.mmio.remove(old.address);
            }
        }
        let mut mapped: Vec<BarMapping> = entry
            .mapped
            .iter()
            .filter(|old| wanted.contains(old))
            .copied()
            .collect();
        for new in wanted {
            if mapped.contains(&new) {
                continue;
            }
            let region = Arc::new(Mutex::new(BarRegion {
                device: entry.device.clone(),
                bar: new.bar,
            }));
            match self.mmio.insert(new.address, new.size, region) {
                Ok(()) => mapped.push(new),
                Err(e) => tracing::warn!(slot, bar = new.bar, %e, "can't map BAR"),
            }
        }
        entry.mapped = mapped;
    }
}

impl Snapshot for PciRoot {
    /// Every function's state, by slot and function.
    fn save(&self) -> Vec<u8> {
        let functions: Vec<((u8, u8), Vec<u8>)> = self
            .functions
            .iter()
            .map(|(&key, function)| {
                let state = function
                    .device
                    .lock()
                    .expect("a PCI device panicked")
                    .save();
                (key, state)
            })
            .collect();
        snapshot::encode(&functions)
    }

    /// Restores each function, then maps its BARs where the restored state has them.
    fn restore(&mut self, state: &[u8]) -> Result<(), StateError> {
        let functions: Vec<((u8, u8), Vec<u8>)> = snapshot::decode(state)?;
        let saved: Vec<(u8, u8)> = functions.iter().map(|(key, _)| *key).collect();
        let here: Vec<(u8, u8)> = self.functions.keys().copied().collect();
        if saved != here {
            return Err(StateError(format!(
                "the PCI bus had other devices: {saved:?}, not {here:?}"
            )));
        }
        for ((slot, function), state) in functions {
            self.functions[&(slot, function)]
                .device
                .lock()
                .expect("a PCI device panicked")
                .restore(&state)
                .map_err(|e| StateError(format!("PCI {slot:02x}.{function}: {e}")))?;
            self.sync_bars((slot, function));
        }
        Ok(())
    }

    fn quiesce(&mut self) {
        for function in self.functions.values() {
            function
                .device
                .lock()
                .expect("a PCI device panicked")
                .quiesce();
        }
    }

    fn resume(&mut self) {
        for function in self.functions.values() {
            function
                .device
                .lock()
                .expect("a PCI device panicked")
                .resume();
        }
    }
}

/// One BAR of a device, as it sits on the MMIO bus.
struct BarRegion {
    device: SharedPciDevice,
    bar: u8,
}

impl BusDevice for BarRegion {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        self.device
            .lock()
            .expect("a PCI device panicked")
            .bar_read(self.bar, offset, data);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        self.device
            .lock()
            .expect("a PCI device panicked")
            .bar_write(self.bar, offset, data);
    }
}

/// Ports 0xcf8–0xcff: the config address and data registers, plus Q35's reset control
/// register, which is the byte at 0xcf9.
pub struct PciConfigIo {
    root: Arc<Mutex<PciRoot>>,
    address: u32,
    reset_control: u8,
    power: PowerHandler,
}

/// Reset control register: bit 2 starts a reset.
const RESET_CPU: u8 = 1 << 2;

impl PciConfigIo {
    pub fn new(root: Arc<Mutex<PciRoot>>, power: PowerHandler) -> Self {
        Self {
            root,
            address: 0,
            reset_control: 0,
            power,
        }
    }

    /// The (bus, slot, function, register) the address register selects, if enabled.
    fn target(&self, data_offset: u64) -> Option<(u8, u8, u8, u16)> {
        if self.address & (1 << 31) == 0 {
            return None;
        }
        let bus = (self.address >> 16) as u8;
        let slot = ((self.address >> 11) & 0x1f) as u8;
        let function = ((self.address >> 8) & 0x7) as u8;
        let register = (self.address & 0xfc) as u16 + data_offset as u16;
        Some((bus, slot, function, register))
    }
}

impl Snapshot for PciConfigIo {
    fn save(&self) -> Vec<u8> {
        snapshot::encode(&(self.address, self.reset_control))
    }

    fn restore(&mut self, state: &[u8]) -> Result<(), StateError> {
        (self.address, self.reset_control) = snapshot::decode(state)?;
        Ok(())
    }
}

impl BusDevice for PciConfigIo {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        match offset {
            1 if data.len() == 1 => data[0] = self.reset_control,
            0..=3 => {
                let bytes = self.address.to_le_bytes();
                for (i, byte) in data.iter_mut().enumerate() {
                    *byte = bytes.get(offset as usize + i).copied().unwrap_or(0xff);
                }
            }
            _ => match self.target(offset - 4) {
                Some((bus, slot, function, register)) => {
                    self.root
                        .lock()
                        .unwrap()
                        .config_read(bus, slot, function, register, data);
                    tracing::trace!(
                        bus,
                        slot,
                        function,
                        register,
                        ?data,
                        "PCI config read (cf8)"
                    );
                }
                None => data.fill(0xff),
            },
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        match offset {
            1 if data.len() == 1 => {
                if data[0] & RESET_CPU != 0 {
                    (self.power)(PowerEvent::Reset);
                }
                self.reset_control = data[0] & !RESET_CPU;
            }
            0..=3 => {
                let mut bytes = self.address.to_le_bytes();
                for (i, &byte) in data.iter().enumerate() {
                    if let Some(slot) = bytes.get_mut(offset as usize + i) {
                        *slot = byte;
                    }
                }
                self.address = u32::from_le_bytes(bytes) & 0x80ff_fffc;
            }
            _ => {
                if let Some((bus, slot, function, register)) = self.target(offset - 4) {
                    tracing::trace!(
                        bus,
                        slot,
                        function,
                        register,
                        ?data,
                        "PCI config write (cf8)"
                    );
                    self.root
                        .lock()
                        .unwrap()
                        .config_write(bus, slot, function, register, data);
                }
            }
        }
    }
}

/// The PCIe enhanced configuration access mechanism (ECAM) window.
///
/// It holds the root weakly: the root owns the MMIO bus this window sits on, so a
/// strong reference would keep the whole machine alive forever.
pub struct PciEcam(pub Weak<Mutex<PciRoot>>);

/// (bus, slot, function, register) for an offset into the ECAM window.
fn ecam_target(offset: u64) -> (u8, u8, u8, u16) {
    (
        (offset >> 20) as u8,
        ((offset >> 15) & 0x1f) as u8,
        ((offset >> 12) & 0x7) as u8,
        (offset & 0xfff) as u16,
    )
}

impl BusDevice for PciEcam {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let (bus, slot, function, register) = ecam_target(offset);
        match self.0.upgrade() {
            Some(root) => root
                .lock()
                .unwrap()
                .config_read(bus, slot, function, register, data),
            None => data.fill(0xff),
        }
        tracing::trace!(
            bus,
            slot,
            function,
            register,
            ?data,
            "PCI config read (ecam)"
        );
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let (bus, slot, function, register) = ecam_target(offset);
        tracing::trace!(
            bus,
            slot,
            function,
            register,
            ?data,
            "PCI config write (ecam)"
        );
        if let Some(root) = self.0.upgrade() {
            root.lock()
                .unwrap()
                .config_write(bus, slot, function, register, data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{BarKind, ConfigSpace, Identity, PciDevice, config};
    use super::*;

    /// A device with one 64-bit BAR whose reads return the BAR offset.
    struct Probe {
        config: ConfigSpace,
        writes: Vec<(u8, u64, Vec<u8>)>,
    }

    impl Probe {
        fn new() -> Self {
            let mut config = ConfigSpace::new(Identity {
                vendor_id: 0x1234,
                device_id: 0x5678,
                revision: 0,
                class: 0xff_00_00,
                subsystem_vendor_id: 0,
                subsystem_id: 0,
            });
            config.add_bar(0, BarKind::Memory64, 0x1000, false);
            Self {
                config,
                writes: Vec::new(),
            }
        }
    }

    impl Snapshot for Probe {
        fn save(&self) -> Vec<u8> {
            self.config.save()
        }
        fn restore(&mut self, state: &[u8]) -> Result<(), StateError> {
            self.config.restore(state)
        }
    }

    impl PciDevice for Probe {
        fn config(&self) -> &ConfigSpace {
            &self.config
        }
        fn config_mut(&mut self) -> &mut ConfigSpace {
            &mut self.config
        }
        fn bar_read(&mut self, _: u8, offset: u64, data: &mut [u8]) {
            data.fill(offset as u8);
        }
        fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
            self.writes.push((bar, offset, data.to_vec()));
        }
    }

    struct Fixture {
        io: PciConfigIo,
        root: Arc<Mutex<PciRoot>>,
        ecam: PciEcam,
        mmio: Arc<Bus>,
        probe: Arc<Mutex<Probe>>,
        resets: Arc<Mutex<u32>>,
    }

    fn fixture() -> Fixture {
        let mmio = Arc::new(Bus::default());
        let root = Arc::new(Mutex::new(PciRoot::new(mmio.clone())));
        let probe = Arc::new(Mutex::new(Probe::new()));
        root.lock().unwrap().add_device(3, probe.clone());
        let resets = Arc::new(Mutex::new(0));
        let power: PowerHandler = Arc::new({
            let resets = resets.clone();
            move |event| {
                assert_eq!(event, PowerEvent::Reset);
                *resets.lock().unwrap() += 1;
            }
        });
        Fixture {
            io: PciConfigIo::new(root.clone(), power),
            root: root.clone(),
            ecam: PciEcam(Arc::downgrade(&root)),
            mmio,
            probe,
            resets,
        }
    }

    fn io_read32(io: &mut PciConfigIo, slot: u8, register: u8) -> u32 {
        let address = 0x8000_0000 | (u32::from(slot) << 11) | u32::from(register);
        io.write(0, &address.to_le_bytes());
        let mut data = [0u8; 4];
        io.read(4, &mut data);
        u32::from_le_bytes(data)
    }

    fn io_write32(io: &mut PciConfigIo, slot: u8, register: u8, value: u32) {
        let address = 0x8000_0000 | (u32::from(slot) << 11) | u32::from(register);
        io.write(0, &address.to_le_bytes());
        io.write(4, &value.to_le_bytes());
    }

    fn ecam_offset(slot: u8, register: u16) -> u64 {
        (u64::from(slot) << 15) | u64::from(register)
    }

    #[test]
    fn enumerates_through_both_mechanisms() {
        let mut f = fixture();
        assert_eq!(io_read32(&mut f.io, 3, 0), 0x5678_1234);
        assert_eq!(io_read32(&mut f.io, 4, 0), 0xffff_ffff, "empty slot");

        let mut data = [0u8; 4];
        f.ecam.read(ecam_offset(3, 0), &mut data);
        assert_eq!(u32::from_le_bytes(data), 0x5678_1234);
        f.ecam.read((1 << 20) | ecam_offset(3, 0), &mut data);
        assert_eq!(data, [0xff; 4], "nothing behind bus 1");
    }

    #[test]
    fn function_zero_advertises_further_functions() {
        let mut f = fixture();
        let header = |ecam: &mut PciEcam, function: u64| {
            let mut data = [0u8; 4];
            ecam.read(ecam_offset(3, 0x0c) | (function << 12), &mut data);
            data[2]
        };
        assert_eq!(header(&mut f.ecam, 0), 0, "single-function so far");

        f.root
            .lock()
            .unwrap()
            .add_function(3, 2, Arc::new(Mutex::new(Probe::new())));
        assert_eq!(header(&mut f.ecam, 0), MULTI_FUNCTION);
        assert_eq!(header(&mut f.ecam, 2), 0, "only function 0 carries the bit");
        let mut id = [0u8; 2];
        f.ecam.read(ecam_offset(3, 0) | (2 << 12), &mut id);
        assert_eq!(u16::from_le_bytes(id), 0x1234);
        f.ecam.read(ecam_offset(3, 0) | (1 << 12), &mut id);
        assert_eq!(id, [0xff; 2], "function 1 is empty");
    }

    #[test]
    fn byte_and_word_config_reads_through_cfc() {
        let mut f = fixture();
        f.io.write(0, &(0x8000_0000u32 | (3 << 11)).to_le_bytes());
        let mut word = [0u8; 2];
        f.io.read(6, &mut word); // 0xcfe: device ID
        assert_eq!(u16::from_le_bytes(word), 0x5678);
    }

    #[test]
    fn bars_appear_on_the_mmio_bus_and_follow_moves() {
        let mut f = fixture();
        io_write32(&mut f.io, 3, config::BAR0 as u8, 0xd000_0000);
        io_write32(&mut f.io, 3, config::BAR0 as u8 + 4, 0);
        let mut data = [0u8; 1];
        assert!(!f.mmio.read(0xd000_0010, &mut data), "decode still off");

        io_write32(
            &mut f.io,
            3,
            config::COMMAND as u8,
            u32::from(config::COMMAND_MEMORY),
        );
        assert!(f.mmio.read(0xd000_0010, &mut data));
        assert_eq!(data, [0x10]);
        assert!(f.mmio.write(0xd000_0020, &[7]));
        assert_eq!(f.probe.lock().unwrap().writes, [(0, 0x20, vec![7])]);

        // Move it through ECAM, the way an OS reassigning resources would.
        f.ecam
            .write(ecam_offset(3, config::BAR0), &0xe000_0000u32.to_le_bytes());
        assert!(
            !f.mmio.read(0xd000_0010, &mut data),
            "gone from the old address"
        );
        assert!(f.mmio.read(0xe000_0010, &mut data));

        // Turning decode off unmaps it.
        f.ecam
            .write(ecam_offset(3, config::COMMAND), &0u16.to_le_bytes());
        assert!(!f.mmio.read(0xe000_0010, &mut data));
    }

    #[test]
    fn byte_write_to_cf9_resets() {
        let mut f = fixture();
        f.io.write(1, &[0x02]);
        assert_eq!(*f.resets.lock().unwrap(), 0, "SYS_RST alone just arms");
        f.io.write(1, &[0x06]);
        assert_eq!(*f.resets.lock().unwrap(), 1);
        // A dword write at 0xcf8 is the address register, never a reset.
        f.io.write(0, &0x8000_0604u32.to_le_bytes());
        assert_eq!(*f.resets.lock().unwrap(), 1);
    }
}
