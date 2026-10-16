//! QEMU's firmware configuration interface (`fw_cfg`), which OVMF reads to learn the
//! platform: memory map, CPU count, ACPI and SMBIOS tables, boot order.
//!
//! Ports: selector at 0x510 (16-bit write), data at 0x511 (8-bit read), and the DMA
//! address register at 0x514 (big-endian; writing its low half at 0x518 starts a
//! transfer). See QEMU's docs/specs/fw_cfg.rst.

use std::collections::BTreeMap;
use std::sync::Arc;

use vm_core::BusDevice;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

pub const FW_CFG_BASE: u16 = 0x510;
pub const FW_CFG_PORTS: u16 = 12;

pub const SIGNATURE: u16 = 0x0000;
pub const ID: u16 = 0x0001;
pub const RAM_SIZE: u16 = 0x0003;
pub const NB_CPUS: u16 = 0x0005;
pub const MAX_CPUS: u16 = 0x000f;
pub const FILE_DIR: u16 = 0x0019;
const FIRST_FILE: u16 = 0x0020;

/// ID features: the traditional port interface and the DMA interface.
const FEATURE_TRADITIONAL: u32 = 1 << 0;
const FEATURE_DMA: u32 = 1 << 1;
/// What a DMA address register read returns ("QEMU CFG" in big-endian).
const DMA_SIGNATURE: u64 = 0x5145_4d55_2043_4647;

// DMA control bits.
const DMA_ERROR: u32 = 1 << 0;
const DMA_READ: u32 = 1 << 1;
const DMA_SKIP: u32 = 1 << 2;
const DMA_SELECT: u32 = 1 << 3;
const DMA_WRITE: u32 = 1 << 4;

const FILE_NAME_LEN: usize = 56;

/// Produces a file's content. Called each time the file is selected, so content can
/// reflect the platform's state at that moment; the length must stay the same.
pub type Generator = Arc<dyn Fn() -> Vec<u8> + Send + Sync>;

enum Content {
    Fixed(Arc<[u8]>),
    Generated { generate: Generator, len: usize },
}

pub struct FwCfg {
    items: BTreeMap<u16, Content>,
    /// File names, sorted, with their selector keys.
    files: BTreeMap<String, u16>,
    next_file_key: u16,
    /// The selected item, its bytes and the read offset into them.
    selected: u16,
    current: Arc<[u8]>,
    offset: usize,
    dma_high: u32,
    memory: GuestMemoryMmap,
}

impl FwCfg {
    pub fn new(memory: GuestMemoryMmap) -> Self {
        let mut fw_cfg = Self {
            items: BTreeMap::new(),
            files: BTreeMap::new(),
            next_file_key: FIRST_FILE,
            selected: 0,
            current: Arc::from([]),
            offset: 0,
            dma_high: 0,
            memory,
        };
        fw_cfg.add_item(SIGNATURE, b"QEMU".to_vec());
        fw_cfg.add_item(
            ID,
            (FEATURE_TRADITIONAL | FEATURE_DMA).to_le_bytes().to_vec(),
        );
        fw_cfg
    }

    /// Adds (or replaces) a numbered item.
    pub fn add_item(&mut self, key: u16, data: Vec<u8>) {
        self.items.insert(key, Content::Fixed(data.into()));
    }

    /// Adds a named file with fixed content.
    pub fn add_file(&mut self, name: &str, data: Vec<u8>) {
        self.insert_file(name, Content::Fixed(data.into()));
    }

    /// Adds a named file whose content is generated whenever it's selected. The
    /// generator runs once now to fix the length the directory advertises.
    pub fn add_generated_file(&mut self, name: &str, generate: Generator) {
        let len = generate().len();
        self.insert_file(name, Content::Generated { generate, len });
    }

    fn insert_file(&mut self, name: &str, content: Content) {
        assert!(
            name.len() < FILE_NAME_LEN,
            "fw_cfg file name too long: {name}"
        );
        let key = *self.files.entry(name.to_owned()).or_insert_with(|| {
            let key = self.next_file_key;
            self.next_file_key += 1;
            key
        });
        self.items.insert(key, content);
    }

    /// The file directory item: a big-endian count, then size/select/name entries.
    fn directory(&self) -> Vec<u8> {
        let mut out = (self.files.len() as u32).to_be_bytes().to_vec();
        for (name, key) in &self.files {
            let size = match &self.items[key] {
                Content::Fixed(data) => data.len(),
                Content::Generated { len, .. } => *len,
            };
            out.extend_from_slice(&(size as u32).to_be_bytes());
            out.extend_from_slice(&key.to_be_bytes());
            out.extend_from_slice(&[0, 0]);
            let mut field = [0u8; FILE_NAME_LEN];
            field[..name.len()].copy_from_slice(name.as_bytes());
            out.extend_from_slice(&field);
        }
        out
    }

    fn select(&mut self, key: u16) {
        self.selected = key;
        self.offset = 0;
        self.current = if key == FILE_DIR {
            self.directory().into()
        } else {
            match self.items.get(&key) {
                Some(Content::Fixed(data)) => data.clone(),
                Some(Content::Generated { generate, len }) => {
                    let mut data = generate();
                    if data.len() != *len {
                        tracing::warn!(key, len, got = data.len(), "fw_cfg file changed size");
                        data.resize(*len, 0);
                    }
                    data.into()
                }
                None => {
                    tracing::debug!(key, "fw_cfg: unknown item selected");
                    Arc::from([])
                }
            }
        };
    }

    fn read_byte(&mut self) -> u8 {
        let byte = self.current.get(self.offset).copied().unwrap_or(0);
        self.offset = self.offset.saturating_add(1);
        byte
    }

    /// Runs the DMA transfer described by the access structure at `address`.
    fn dma(&mut self, address: u64) {
        let at = GuestAddress(address);
        let control = self.memory.read_obj::<u32>(at).map(u32::from_be);
        let length = self
            .memory
            .read_obj::<u32>(GuestAddress(address + 4))
            .map(u32::from_be);
        let target = self
            .memory
            .read_obj::<u64>(GuestAddress(address + 8))
            .map(u64::from_be);
        let (Ok(control), Ok(length), Ok(target)) = (control, length, target) else {
            tracing::warn!(address, "fw_cfg DMA descriptor outside guest RAM");
            return;
        };
        if control & DMA_SELECT != 0 {
            self.select((control >> 16) as u16);
        }
        let length = length as usize;
        let mut result = 0;
        if control & DMA_READ != 0 {
            let available = self.current.len().saturating_sub(self.offset).min(length);
            let mut buffer = vec![0u8; length];
            buffer[..available]
                .copy_from_slice(&self.current[self.offset..self.offset + available]);
            if self
                .memory
                .write_slice(&buffer, GuestAddress(target))
                .is_err()
            {
                result = DMA_ERROR;
            }
            self.offset = self.offset.saturating_add(length);
        } else if control & DMA_SKIP != 0 {
            self.offset = self.offset.saturating_add(length);
        } else if control & DMA_WRITE != 0 {
            // No writable items.
            result = DMA_ERROR;
        }
        if self.memory.write_obj(result.to_be(), at).is_err() {
            tracing::warn!(address, "fw_cfg DMA: can't write the result back");
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FwCfgState {
    selected: u16,
    offset: usize,
    dma_high: u32,
}

impl crate::snapshot::Snapshot for FwCfg {
    fn save(&self) -> Vec<u8> {
        crate::snapshot::encode(&FwCfgState {
            selected: self.selected,
            offset: self.offset,
            dma_high: self.dma_high,
        })
    }

    /// The items are the configuration's; a generated one is generated again.
    fn restore(&mut self, state: &[u8]) -> Result<(), crate::snapshot::StateError> {
        let state: FwCfgState = crate::snapshot::decode(state)?;
        self.select(state.selected);
        self.offset = state.offset;
        self.dma_high = state.dma_high;
        Ok(())
    }
}

impl BusDevice for FwCfg {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        match offset {
            // Data port.
            1 => {
                for byte in data.iter_mut() {
                    *byte = self.read_byte();
                }
            }
            // DMA address register: reads return the signature.
            4..=11 => {
                let bytes = DMA_SIGNATURE.to_be_bytes();
                for (i, byte) in data.iter_mut().enumerate() {
                    *byte = bytes.get(offset as usize - 4 + i).copied().unwrap_or(0);
                }
            }
            _ => data.fill(0),
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        match (offset, data.len()) {
            (0, 2) => self.select(u16::from_le_bytes([data[0], data[1]])),
            (4, 4) => self.dma_high = u32::from_be_bytes(data.try_into().unwrap()),
            (8, 4) => {
                let low = u32::from_be_bytes(data.try_into().unwrap());
                let address = (u64::from(self.dma_high) << 32) | u64::from(low);
                self.dma_high = 0;
                self.dma(address);
            }
            (4, 8) => self.dma(u64::from_be_bytes(data.try_into().unwrap())),
            _ => tracing::debug!(offset, ?data, "fw_cfg: ignored write"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU8, Ordering};

    use super::*;

    fn memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10_0000)]).unwrap()
    }

    fn select(fw: &mut FwCfg, key: u16) {
        fw.write(0, &key.to_le_bytes());
    }

    fn read(fw: &mut FwCfg, len: usize) -> Vec<u8> {
        (0..len)
            .map(|_| {
                let mut byte = [0u8];
                fw.read(1, &mut byte);
                byte[0]
            })
            .collect()
    }

    /// Finds a file's (size, key) in the directory, read through the data port.
    fn lookup(fw: &mut FwCfg, name: &str) -> Option<(u32, u16)> {
        select(fw, FILE_DIR);
        let count = u32::from_be_bytes(read(fw, 4).try_into().unwrap());
        (0..count).find_map(|_| {
            let entry = read(fw, 64);
            let size = u32::from_be_bytes(entry[0..4].try_into().unwrap());
            let key = u16::from_be_bytes(entry[4..6].try_into().unwrap());
            let end = entry[8..].iter().position(|&b| b == 0).unwrap_or(56);
            (&entry[8..8 + end] == name.as_bytes()).then_some((size, key))
        })
    }

    #[test]
    fn signature_and_features() {
        let mut fw = FwCfg::new(memory());
        select(&mut fw, SIGNATURE);
        assert_eq!(read(&mut fw, 4), b"QEMU");
        select(&mut fw, ID);
        assert_eq!(read(&mut fw, 4), [3, 0, 0, 0]);
        let mut sig = [0u8; 4];
        fw.read(4, &mut sig);
        assert_eq!(&sig, b"QEMU", "DMA register reads the signature");
    }

    #[test]
    fn files_are_listed_sorted_and_readable() {
        let mut fw = FwCfg::new(memory());
        fw.add_file("etc/e820", vec![1; 20]);
        fw.add_file("bootorder", b"/pci@i0cf8\n".to_vec());
        let (size, key) = lookup(&mut fw, "bootorder").unwrap();
        assert_eq!(size, 11);
        select(&mut fw, key);
        assert_eq!(read(&mut fw, 11), b"/pci@i0cf8\n");
        assert_eq!(read(&mut fw, 2), [0, 0], "past the end reads zeros");
        assert_eq!(lookup(&mut fw, "etc/e820").unwrap().0, 20);
        assert!(lookup(&mut fw, "missing").is_none());
    }

    #[test]
    fn generated_files_reflect_state_at_selection() {
        let state = Arc::new(AtomicU8::new(1));
        let mut fw = FwCfg::new(memory());
        fw.add_generated_file("etc/acpi/tables", {
            let state = state.clone();
            Arc::new(move || vec![state.load(Ordering::SeqCst); 4])
        });
        let (size, key) = lookup(&mut fw, "etc/acpi/tables").unwrap();
        assert_eq!(size, 4);
        state.store(7, Ordering::SeqCst);
        select(&mut fw, key);
        assert_eq!(read(&mut fw, 4), [7; 4]);
    }

    #[test]
    fn dma_select_read_and_skip() {
        let mem = memory();
        let mut fw = FwCfg::new(mem.clone());
        fw.add_file("etc/boot-fail-wait", vec![1, 2, 3, 4, 5, 6]);
        let (_, key) = lookup(&mut fw, "etc/boot-fail-wait").unwrap();

        // Select + read 4 bytes into 0x2000 (descriptor at 0x1000, big-endian).
        let control = (u32::from(key) << 16) | DMA_SELECT | DMA_READ;
        mem.write_obj(control.to_be(), GuestAddress(0x1000))
            .unwrap();
        mem.write_obj(4u32.to_be(), GuestAddress(0x1004)).unwrap();
        mem.write_obj(0x2000u64.to_be(), GuestAddress(0x1008))
            .unwrap();
        fw.write(4, &0u32.to_be_bytes());
        fw.write(8, &0x1000u32.to_be_bytes());
        assert_eq!(
            mem.read_obj::<u32>(GuestAddress(0x1000)).unwrap(),
            0,
            "done, no error"
        );
        let mut got = [0u8; 4];
        mem.read_slice(&mut got, GuestAddress(0x2000)).unwrap();
        assert_eq!(got, [1, 2, 3, 4]);

        // Skip 1, then read 4: one real byte, then zeros.
        mem.write_obj(DMA_SKIP.to_be(), GuestAddress(0x1000))
            .unwrap();
        mem.write_obj(1u32.to_be(), GuestAddress(0x1004)).unwrap();
        fw.write(8, &0x1000u32.to_be_bytes());
        mem.write_obj(DMA_READ.to_be(), GuestAddress(0x1000))
            .unwrap();
        mem.write_obj(4u32.to_be(), GuestAddress(0x1004)).unwrap();
        fw.write(8, &0x1000u32.to_be_bytes());
        mem.read_slice(&mut got, GuestAddress(0x2000)).unwrap();
        assert_eq!(got, [6, 0, 0, 0]);
    }

    #[test]
    fn dma_write_is_refused() {
        let mem = memory();
        let mut fw = FwCfg::new(mem.clone());
        mem.write_obj(DMA_WRITE.to_be(), GuestAddress(0x1000))
            .unwrap();
        fw.write(4, &[0, 0, 0, 0, 0, 0, 0x10, 0]);
        assert_eq!(
            u32::from_be(mem.read_obj::<u32>(GuestAddress(0x1000)).unwrap()),
            DMA_ERROR
        );
    }
}
