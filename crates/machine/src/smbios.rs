//! SMBIOS 3.0 tables, handed to UEFI firmware through fw_cfg as QEMU does: a `_SM3_`
//! entry point in `etc/smbios/smbios-anchor` and the structures in
//! `etc/smbios/smbios-tables`. OVMF installs them and adds its own BIOS information
//! (type 0).

pub const ANCHOR_FILE: &str = "etc/smbios/smbios-anchor";
pub const TABLES_FILE: &str = "etc/smbios/smbios-tables";

const MANUFACTURER: &str = "vmmc";
const PRODUCT: &str = "vmmc Virtual Machine";
const FAMILY: &str = "Virtual Machine";
/// Speed reported for the virtual processor, in MHz.
const CPU_MHZ: u16 = 2000;

/// What the tables describe.
#[derive(Clone, Debug)]
pub struct SmbiosConfig {
    /// The system UUID in RFC 4122 byte order (as written in its string form).
    pub uuid: [u8; 16],
    pub cpus: u8,
    /// Guest RAM as `(guest-physical address, size)` ranges.
    pub ram: Vec<(u64, u64)>,
}

/// The entry point and the structure table.
pub struct Smbios {
    pub anchor: Vec<u8>,
    pub tables: Vec<u8>,
}

pub fn build(config: &SmbiosConfig) -> Smbios {
    let mut tables = Vec::new();
    let mut handles = 0x100u16..;
    let mut next_handle = || handles.next().expect("handles left");

    // Type 1: system information.
    let mut system = Structure::new(1, next_handle());
    system.string(MANUFACTURER);
    system.string(PRODUCT);
    system.string(env!("CARGO_PKG_VERSION"));
    system.string(&format!("VMMC-{}", hex(&config.uuid)));
    system.bytes(&smbios_uuid(&config.uuid));
    system.u8(0x06); // woken by the power switch
    system.string(MANUFACTURER); // SKU
    system.string(FAMILY);
    system.finish(&mut tables);

    // Type 3: chassis.
    let mut chassis = Structure::new(3, next_handle());
    chassis.string(MANUFACTURER);
    chassis.u8(0x01); // other
    chassis.string(env!("CARGO_PKG_VERSION"));
    chassis.none(); // serial number
    chassis.none(); // asset tag
    chassis.bytes(&[0x03, 0x03, 0x03, 0x02]); // boot-up, power, thermal: safe; security: unknown
    chassis.u32(0); // OEM-defined
    chassis.bytes(&[0, 0, 0, 0]); // height, power cords, no contained elements
    chassis.finish(&mut tables);

    // Type 4: one processor socket with every vCPU as a core.
    let cores = config.cpus;
    let mut cpu = Structure::new(4, next_handle());
    cpu.string("CPU 0");
    cpu.u8(0x03); // central processor
    cpu.u8(0xfe); // family: see family 2
    cpu.string(MANUFACTURER);
    cpu.bytes(&[0; 8]); // processor ID
    cpu.none(); // version
    cpu.u8(0); // voltage
    cpu.u16(0); // external clock: unknown
    cpu.u16(CPU_MHZ); // max speed
    cpu.u16(CPU_MHZ); // current speed
    cpu.u8(0x41); // socket populated, CPU enabled
    cpu.u8(0x01); // upgrade: other
    cpu.u16(0xffff); // no L1, L2 or L3 cache information
    cpu.u16(0xffff);
    cpu.u16(0xffff);
    cpu.none(); // serial number
    cpu.none(); // asset tag
    cpu.none(); // part number
    cpu.bytes(&[cores, cores, cores]); // cores, enabled cores, threads
    cpu.u16(0x0004); // 64-bit capable
    cpu.u16(0x0001); // family 2: other
    cpu.u16(cores.into()); // core count 2
    cpu.u16(cores.into()); // enabled cores 2
    cpu.u16(cores.into()); // thread count 2
    cpu.finish(&mut tables);

    // Type 16: the memory array, and type 17: one device holding all of it.
    // Sizes go in the short fields when they fit; the extended fields are only for
    // what doesn't.
    let total: u64 = config.ram.iter().map(|&(_, size)| size).sum();
    let kib = total >> 10;
    let (capacity, extended_capacity) = match u32::try_from(kib) {
        Ok(kib) if kib < 0x8000_0000 => (kib, 0),
        _ => (0x8000_0000, total),
    };
    let mib = total >> 20;
    let (size, extended_size) = if mib < 0x7fff {
        (mib as u16, 0)
    } else {
        (0x7fff, mib as u32)
    };

    let array_handle = next_handle();
    let mut array = Structure::new(16, array_handle);
    array.u8(0x03); // location: system board
    array.u8(0x03); // use: system memory
    array.u8(0x03); // error correction: none
    array.u32(capacity);
    array.u16(0xfffe); // no error information
    array.u16(1); // devices
    array.u64(extended_capacity);
    array.finish(&mut tables);

    let mut dimm = Structure::new(17, next_handle());
    dimm.u16(array_handle);
    dimm.u16(0xfffe); // no error information
    dimm.u16(0xffff); // total width: unknown
    dimm.u16(0xffff); // data width: unknown
    dimm.u16(size); // MiB
    dimm.u8(0x09); // form factor: DIMM
    dimm.u8(0); // not part of a set
    dimm.string("DIMM 0");
    dimm.none(); // bank locator
    dimm.u8(0x07); // type: RAM
    dimm.u16(0x0002); // type detail: other
    dimm.u16(0); // speed: unknown
    dimm.string(MANUFACTURER);
    dimm.none(); // serial number
    dimm.none(); // asset tag
    dimm.none(); // part number
    dimm.u8(0); // attributes
    dimm.u32(extended_size); // MiB
    dimm.u16(0); // configured speed
    dimm.u16(0); // minimum, maximum and configured voltage: unknown
    dimm.u16(0);
    dimm.u16(0);
    dimm.finish(&mut tables);

    // Type 19: where each RAM range sits.
    for &(address, size) in &config.ram {
        let mut mapped = Structure::new(19, next_handle());
        mapped.u32(0xffff_ffff); // start and end: see the extended fields
        mapped.u32(0xffff_ffff);
        mapped.u16(array_handle);
        mapped.u8(1); // partition width
        mapped.u64(address);
        mapped.u64(address + size - 1);
        mapped.finish(&mut tables);
    }

    // Type 32: system boot information, no errors.
    let mut boot = Structure::new(32, next_handle());
    boot.bytes(&[0; 6]);
    boot.u8(0);
    boot.finish(&mut tables);

    Structure::new(127, next_handle()).finish(&mut tables);

    Smbios {
        anchor: anchor(tables.len()),
        tables,
    }
}

/// The 64-bit (`_SM3_`) entry point. Firmware fills in the table address.
fn anchor(tables_len: usize) -> Vec<u8> {
    let mut anchor = vec![0u8; 0x18];
    anchor[0..5].copy_from_slice(b"_SM3_");
    anchor[6] = 0x18; // length
    anchor[7] = 3; // SMBIOS 3.0.0
    anchor[8] = 0;
    anchor[9] = 0;
    anchor[10] = 1; // entry point revision
    anchor[12..16].copy_from_slice(&(tables_len as u32).to_le_bytes());
    let sum = anchor.iter().fold(0u8, |sum, &b| sum.wrapping_add(b));
    anchor[5] = 0u8.wrapping_sub(sum);
    anchor
}

/// SMBIOS stores the first three UUID fields little-endian.
fn smbios_uuid(uuid: &[u8; 16]) -> [u8; 16] {
    let mut out = *uuid;
    out[0..4].reverse();
    out[4..6].reverse();
    out[6..8].reverse();
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// One structure: the formatted area, then its strings.
struct Structure {
    formatted: Vec<u8>,
    strings: Vec<String>,
}

impl Structure {
    fn new(kind: u8, handle: u16) -> Self {
        let mut formatted = vec![kind, 0];
        formatted.extend_from_slice(&handle.to_le_bytes());
        Self {
            formatted,
            strings: Vec::new(),
        }
    }

    fn u8(&mut self, value: u8) {
        self.formatted.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.formatted.extend_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.formatted.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.formatted.extend_from_slice(&value.to_le_bytes());
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.formatted.extend_from_slice(bytes);
    }

    /// A string field: its 1-based index into the string set.
    fn string(&mut self, value: &str) {
        self.strings.push(value.to_owned());
        self.formatted.push(self.strings.len() as u8);
    }

    /// A string field with no string.
    fn none(&mut self) {
        self.formatted.push(0);
    }

    fn finish(mut self, out: &mut Vec<u8>) {
        self.formatted[1] = self.formatted.len() as u8;
        out.extend_from_slice(&self.formatted);
        for string in &self.strings {
            out.extend_from_slice(string.as_bytes());
            out.push(0);
        }
        if self.strings.is_empty() {
            out.push(0);
        }
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: [u8; 16] = [
        0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
        0xef,
    ];

    fn smbios() -> Smbios {
        build(&SmbiosConfig {
            uuid: UUID,
            cpus: 4,
            ram: vec![(0, 0xb000_0000), (0x1_0000_0000, 0x5000_0000)],
        })
    }

    /// Splits the table into `(type, formatted area, strings)`.
    fn structures(tables: &[u8]) -> Vec<(u8, Vec<u8>, Vec<String>)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < tables.len() {
            let len = usize::from(tables[at + 1]);
            let formatted = tables[at..at + len].to_vec();
            let mut strings = Vec::new();
            let mut cursor = at + len;
            if tables[cursor] == 0 {
                cursor += 2;
            } else {
                while tables[cursor] != 0 {
                    let end = cursor + tables[cursor..].iter().position(|&b| b == 0).unwrap();
                    strings.push(String::from_utf8(tables[cursor..end].to_vec()).unwrap());
                    cursor = end + 1;
                }
                cursor += 1;
            }
            out.push((formatted[0], formatted, strings));
            at = cursor;
        }
        out
    }

    #[test]
    fn anchor_is_a_valid_smbios3_entry_point() {
        let smbios = smbios();
        let anchor = &smbios.anchor;
        assert_eq!(anchor.len(), 24);
        assert_eq!(&anchor[0..5], b"_SM3_");
        assert_eq!(anchor.iter().fold(0u8, |s, &b| s.wrapping_add(b)), 0);
        assert_eq!(anchor[7], 3);
        // OVMF rejects the tables unless this matches the file size exactly.
        assert_eq!(
            u32::from_le_bytes(anchor[12..16].try_into().unwrap()) as usize,
            smbios.tables.len()
        );
    }

    #[test]
    fn structures_parse_and_end_with_type_127() {
        let smbios = smbios();
        let types: Vec<u8> = structures(&smbios.tables).iter().map(|s| s.0).collect();
        assert_eq!(types, [1, 3, 4, 16, 17, 19, 19, 32, 127]);
        let lengths: Vec<usize> = structures(&smbios.tables)
            .iter()
            .map(|s| s.1.len())
            .collect();
        // The spec's lengths for SMBIOS 3.0 structures.
        assert_eq!(lengths, [0x1b, 0x15, 0x30, 0x17, 0x28, 0x1f, 0x1f, 0x0b, 4]);
    }

    #[test]
    fn system_information_carries_the_uuid_in_smbios_order() {
        let all = structures(&smbios().tables);
        let (_, system, strings) = &all[0];
        assert_eq!(strings[usize::from(system[4]) - 1], MANUFACTURER);
        assert_eq!(strings[usize::from(system[5]) - 1], PRODUCT);
        assert_eq!(
            system[8..24],
            [
                0x78, 0x56, 0x34, 0x12, 0xbc, 0x9a, 0xf0, 0xde, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef
            ]
        );
        assert_eq!(
            strings[usize::from(system[7]) - 1],
            "VMMC-123456789ABCDEF00123456789ABCDEF"
        );
    }

    #[test]
    fn memory_structures_add_up() {
        let all = structures(&smbios().tables);
        let array = &all[3].1;
        assert_eq!(
            u32::from_le_bytes(array[0x07..0x0b].try_into().unwrap()),
            4 << 20,
            "4 GiB in KiB"
        );
        let dimm = &all[4].1;
        assert_eq!(u16::from_le_bytes([dimm[0x0c], dimm[0x0d]]), 4096, "MiB");
        let high = &all[6].1;
        assert_eq!(
            u64::from_le_bytes(high[0x0f..0x17].try_into().unwrap()),
            0x1_0000_0000
        );
        assert_eq!(
            u64::from_le_bytes(high[0x17..0x1f].try_into().unwrap()),
            0x1_4fff_ffff
        );

        let huge = build(&SmbiosConfig {
            uuid: UUID,
            cpus: 1,
            ram: vec![(0, 64 << 30)],
        });
        let all = structures(&huge.tables);
        let dimm = &all[4].1;
        assert_eq!(u16::from_le_bytes([dimm[0x0c], dimm[0x0d]]), 0x7fff);
        assert_eq!(
            u32::from_le_bytes(dimm[0x1c..0x20].try_into().unwrap()),
            64 << 10
        );
    }

    #[test]
    fn processor_reports_every_vcpu_as_a_core() {
        let all = structures(&smbios().tables);
        let cpu = &all[2].1;
        assert_eq!(cpu[0x23..0x26], [4, 4, 4]);
        assert_eq!(u16::from_le_bytes([cpu[0x2a], cpu[0x2b]]), 4);
    }
}
