//! ACPI tables: RSDP, XSDT, FADT, FACS, MADT, MCFG and the DSDT.
//!
//! The static tables are written field by field; the DSDT's AML comes from the
//! `acpi_tables` builder.

use acpi_tables::Aml;
use acpi_tables::aml::{
    AddressSpace, AddressSpaceCacheable, Device, EISAName, IO, Interrupt, Memory32Fixed, Name,
    Package, Path, ResourceTemplate,
};
use devices::legacy::acpi_pm::{
    PM_TMR, PM_TMR_LEN, PM1A_CNT, PM1A_CNT_LEN, PM1A_EVT, PM1A_EVT_LEN, S5_SLEEP_TYPE,
};
use devices::legacy::i8042::{I8042_COMMAND, I8042_DATA};
use devices::legacy::ioapic::IOAPIC_BASE;
use devices::legacy::rtc::RTC_BASE;
use devices::legacy::serial::{COM1_BASE, COM1_IRQ};
use devices::pci::root::CONFIG_IO_BASE;

use crate::layout::{ECAM_BASE, ECAM_SIZE, PCI_MMIO32_END, PCI_MMIO32_START};

const OEM_ID: [u8; 6] = *b"VMMC  ";
const OEM_TABLE_ID: [u8; 8] = *b"VMMCPC  ";
const CREATOR_ID: [u8; 4] = *b"VMMC";
const HEADER_LEN: usize = 36;

/// The local APIC base every x86 CPU uses.
pub const LAPIC_BASE: u32 = 0xfee0_0000;
/// The IOAPIC's APIC ID.
pub const IOAPIC_ID: u8 = 0;
/// SCI interrupt (ISA IRQ 9, level-triggered, active high).
pub const SCI_IRQ: u8 = 9;
/// Reset register: writing 6 to port 0xcf9 resets the system.
const RESET_PORT: u16 = 0xcf9;
const RESET_VALUE: u8 = 0x06;
/// CMOS index of the century byte.
const RTC_CENTURY: u8 = 0x32;

/// What the tables describe.
#[derive(Clone, Debug)]
pub struct AcpiConfig {
    pub cpus: u8,
    /// The 64-bit PCI MMIO window, `[start, end)`.
    pub mmio64: (u64, u64),
}

/// Tables laid out for a guest-physical base address.
#[derive(Clone, Debug)]
pub struct AcpiTables {
    pub bytes: Vec<u8>,
    /// Address of the RSDP (which is at the base).
    pub rsdp: u64,
}

/// Builds every table, laid out to be copied to guest-physical `base`. `base` must be
/// 16-byte aligned so the RSDP can be found by scanning.
pub fn build(config: &AcpiConfig, base: u64) -> AcpiTables {
    assert!(base.is_multiple_of(16));
    let mut out = Layout {
        bytes: vec![0; RSDP_LEN],
        base,
    };
    let facs = out.add(&facs(), 64);
    let dsdt = out.add(&dsdt(config), 16);
    let fadt = out.add(&fadt(facs, dsdt), 16);
    let madt = out.add(&madt(config), 16);
    let mcfg = out.add(&mcfg(), 16);
    let xsdt = out.add(&xsdt(&[fadt, madt, mcfg]), 16);
    let rsdp = rsdp(xsdt);
    out.bytes[..RSDP_LEN].copy_from_slice(&rsdp);
    AcpiTables {
        bytes: out.bytes,
        rsdp: base,
    }
}

struct Layout {
    bytes: Vec<u8>,
    base: u64,
}

impl Layout {
    /// Appends `table` at the next `align`-aligned offset and returns its address.
    fn add(&mut self, table: &[u8], align: usize) -> u64 {
        let offset = self.bytes.len().next_multiple_of(align);
        self.bytes.resize(offset, 0);
        self.bytes.extend_from_slice(table);
        self.base + offset as u64
    }
}

/// Sum-to-zero checksum byte for `bytes`.
fn checksum(bytes: &[u8]) -> u8 {
    0u8.wrapping_sub(bytes.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)))
}

/// A system description table: header, then fields written by offset.
struct Table(Vec<u8>);

impl Table {
    fn new(signature: &[u8; 4], revision: u8, length: usize) -> Self {
        let mut bytes = vec![0u8; length];
        bytes[0..4].copy_from_slice(signature);
        bytes[8] = revision;
        bytes[10..16].copy_from_slice(&OEM_ID);
        bytes[16..24].copy_from_slice(&OEM_TABLE_ID);
        bytes[24..28].copy_from_slice(&1u32.to_le_bytes());
        bytes[28..32].copy_from_slice(&CREATOR_ID);
        bytes[32..36].copy_from_slice(&1u32.to_le_bytes());
        Self(bytes)
    }

    fn u8(&mut self, offset: usize, value: u8) {
        self.0[offset] = value;
    }

    fn u16(&mut self, offset: usize, value: u16) {
        self.0[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, offset: usize, value: u32) {
        self.0[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, offset: usize, value: u64) {
        self.0[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    /// A Generic Address Structure for an I/O port register.
    fn io_gas(&mut self, offset: usize, port: u16, bytes: u8) {
        let access_size = match bytes {
            1 => 1,
            2 => 2,
            _ => 3,
        };
        self.u8(offset, 1); // System I/O space
        self.u8(offset + 1, bytes * 8);
        self.u8(offset + 2, 0);
        self.u8(offset + 3, access_size);
        self.u64(offset + 4, port.into());
    }

    fn push(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    /// Sets the length and checksum.
    fn finish(mut self) -> Vec<u8> {
        let len = self.0.len() as u32;
        self.u32(4, len);
        self.0[9] = 0;
        self.0[9] = checksum(&self.0);
        self.0
    }
}

const RSDP_LEN: usize = 36;

fn rsdp(xsdt: u64) -> [u8; RSDP_LEN] {
    let mut rsdp = [0u8; RSDP_LEN];
    rsdp[0..8].copy_from_slice(b"RSD PTR ");
    rsdp[9..15].copy_from_slice(&OEM_ID);
    rsdp[15] = 2; // ACPI 2.0+: use the XSDT
    rsdp[20..24].copy_from_slice(&(RSDP_LEN as u32).to_le_bytes());
    rsdp[24..32].copy_from_slice(&xsdt.to_le_bytes());
    rsdp[8] = checksum(&rsdp[..20]);
    rsdp[32] = checksum(&rsdp);
    rsdp
}

fn xsdt(entries: &[u64]) -> Vec<u8> {
    let mut table = Table::new(b"XSDT", 1, HEADER_LEN);
    for entry in entries {
        table.push(&entry.to_le_bytes());
    }
    table.finish()
}

fn facs() -> Vec<u8> {
    let mut facs = vec![0u8; 64];
    facs[0..4].copy_from_slice(b"FACS");
    facs[4..8].copy_from_slice(&64u32.to_le_bytes());
    facs[32] = 2; // version
    facs
}

fn fadt(facs: u64, dsdt: u64) -> Vec<u8> {
    // FADT flags.
    const WBINVD: u32 = 1 << 0;
    const PROC_C1: u32 = 1 << 2;
    const SLP_BUTTON: u32 = 1 << 5;
    const RESET_REG_SUP: u32 = 1 << 10;
    // IA-PC boot architecture flags.
    const LEGACY_DEVICES: u16 = 1 << 0;
    const HAS_8042: u16 = 1 << 1;
    const VGA_NOT_PRESENT: u16 = 1 << 2;

    let mut t = Table::new(b"FACP", 6, 276);
    t.u32(36, facs as u32);
    t.u32(40, dsdt as u32);
    t.u8(45, 1); // preferred PM profile: desktop
    t.u16(46, SCI_IRQ.into());
    // SMI_CMD stays 0: the platform is always in ACPI mode.
    t.u32(56, PM1A_EVT.into());
    t.u32(64, PM1A_CNT.into());
    t.u32(76, PM_TMR.into());
    t.u8(88, PM1A_EVT_LEN);
    t.u8(89, PM1A_CNT_LEN);
    t.u8(91, PM_TMR_LEN);
    // C2 and C3 not supported.
    t.u16(96, 101);
    t.u16(98, 1001);
    t.u8(108, RTC_CENTURY);
    t.u16(109, LEGACY_DEVICES | HAS_8042 | VGA_NOT_PRESENT);
    t.u32(112, WBINVD | PROC_C1 | SLP_BUTTON | RESET_REG_SUP);
    t.io_gas(116, RESET_PORT, 1);
    t.u8(128, RESET_VALUE);
    t.u8(131, 1); // FADT minor version (ACPI 6.1)
    // X_FIRMWARE_CTRL (132) stays 0: the spec requires it when FIRMWARE_CTRL is set.
    t.u64(140, dsdt);
    t.io_gas(148, PM1A_EVT, PM1A_EVT_LEN);
    t.io_gas(172, PM1A_CNT, PM1A_CNT_LEN);
    t.io_gas(208, PM_TMR, PM_TMR_LEN);
    t.finish()
}

fn madt(config: &AcpiConfig) -> Vec<u8> {
    const PCAT_COMPAT: u32 = 1;
    const ENABLED: u32 = 1;
    // Interrupt source override flags: polarity [1:0], trigger mode [3:2].
    const ACTIVE_HIGH_LEVEL: u16 = 0b01 | (0b11 << 2);

    let mut t = Table::new(b"APIC", 5, HEADER_LEN + 8);
    t.u32(36, LAPIC_BASE);
    t.u32(40, PCAT_COMPAT);
    for cpu in 0..config.cpus {
        // Processor local APIC: type 0, length 8, UID, APIC ID, flags.
        t.push(&[0, 8, cpu, cpu]);
        t.push(&ENABLED.to_le_bytes());
    }
    // I/O APIC: type 1, length 12, ID, reserved, address, GSI base.
    t.push(&[1, 12, IOAPIC_ID, 0]);
    t.push(&(IOAPIC_BASE as u32).to_le_bytes());
    t.push(&0u32.to_le_bytes());
    // Interrupt source overrides: type 2, length 10, bus (ISA), IRQ, GSI, flags.
    // The PIT's IRQ 0 is wired to IOAPIC pin 2.
    t.push(&[2, 10, 0, 0]);
    t.push(&2u32.to_le_bytes());
    t.push(&0u16.to_le_bytes());
    t.push(&[2, 10, 0, SCI_IRQ]);
    t.push(&u32::from(SCI_IRQ).to_le_bytes());
    t.push(&ACTIVE_HIGH_LEVEL.to_le_bytes());
    // Local APIC NMI on LINT1 for all processors: type 4, length 6, UID 0xff, flags, LINT.
    t.push(&[4, 6, 0xff, 0, 0, 1]);
    t.finish()
}

fn mcfg() -> Vec<u8> {
    let mut t = Table::new(b"MCFG", 1, HEADER_LEN + 8);
    // One allocation: base, segment 0, buses 0-255.
    t.push(&ECAM_BASE.to_le_bytes());
    t.push(&[0, 0, 0, (ECAM_SIZE >> 20).saturating_sub(1) as u8]);
    t.push(&[0; 4]);
    t.finish()
}

fn dsdt(config: &AcpiConfig) -> Vec<u8> {
    let mut t = Table::new(b"DSDT", 2, HEADER_LEN);
    let mut aml = Vec::new();
    dsdt_aml(config, &mut aml);
    t.push(&aml);
    t.finish()
}

fn dsdt_aml(config: &AcpiConfig, sink: &mut Vec<u8>) {
    // \_SB.PCI0: the PCIe root complex and the windows it decodes.
    let buses = AddressSpace::<u16>::new_bus_number(0, 0xff);
    let config_ports = IO::new(CONFIG_IO_BASE, CONFIG_IO_BASE, 1, 8);
    let io_low = AddressSpace::<u16>::new_io(0, CONFIG_IO_BASE - 1, None);
    let io_high = AddressSpace::<u16>::new_io(0xd00, 0xffff, None);
    let mmio32 = AddressSpace::<u32>::new_memory(
        AddressSpaceCacheable::NotCacheable,
        true,
        PCI_MMIO32_START as u32,
        (PCI_MMIO32_END - 1) as u32,
        None,
    );
    let mmio64 = AddressSpace::<u64>::new_memory(
        AddressSpaceCacheable::NotCacheable,
        true,
        config.mmio64.0,
        config.mmio64.1 - 1,
        None,
    );
    let pci_crs = ResourceTemplate::new(vec![
        &buses,
        &config_ports,
        &io_low,
        &io_high,
        &mmio32,
        &mmio64,
    ]);
    let zero = 0u8;
    let one = 1u8;
    device(
        "\\_SB_.PCI0",
        &[
            ("_HID", &EISAName::new("PNP0A08")),
            ("_CID", &EISAName::new("PNP0A03")),
            ("_ADR", &zero),
            ("_SEG", &zero),
            ("_UID", &zero),
            ("_BBN", &zero),
            ("_CRS", &pci_crs),
        ],
        sink,
    );

    // Motherboard resources: reserves the ECAM window, which Linux requires before it
    // will use ECAM.
    let ecam = Memory32Fixed::new(true, ECAM_BASE as u32, ECAM_SIZE as u32);
    device(
        "\\_SB_.MBRS",
        &[
            ("_HID", &EISAName::new("PNP0C02")),
            ("_UID", &one),
            ("_CRS", &ResourceTemplate::new(vec![&ecam])),
        ],
        sink,
    );

    let com1_io = IO::new(COM1_BASE, COM1_BASE, 1, 8);
    let com1_irq = Interrupt::new(true, true, false, false, COM1_IRQ.into());
    device(
        "\\_SB_.COM1",
        &[
            ("_HID", &EISAName::new("PNP0501")),
            ("_UID", &one),
            ("_CRS", &ResourceTemplate::new(vec![&com1_io, &com1_irq])),
        ],
        sink,
    );

    // The 8042 keyboard controller.
    let kbd_data = IO::new(I8042_DATA, I8042_DATA, 1, 1);
    let kbd_command = IO::new(I8042_COMMAND, I8042_COMMAND, 1, 1);
    let kbd_irq = Interrupt::new(true, true, false, false, 1);
    device(
        "\\_SB_.KBD_",
        &[
            ("_HID", &EISAName::new("PNP0303")),
            (
                "_CRS",
                &ResourceTemplate::new(vec![&kbd_data, &kbd_command, &kbd_irq]),
            ),
        ],
        sink,
    );

    // The CMOS RTC (no interrupt wired yet).
    let rtc_io = IO::new(RTC_BASE, RTC_BASE, 1, 2);
    device(
        "\\_SB_.RTC_",
        &[
            ("_HID", &EISAName::new("PNP0B00")),
            ("_CRS", &ResourceTemplate::new(vec![&rtc_io])),
        ],
        sink,
    );

    // \_S5: soft-off uses SLP_TYP 5 (what the ACPI PM device powers off on).
    let s5_type = S5_SLEEP_TYPE as u8;
    let s5 = Package::new(vec![&s5_type, &s5_type, &zero, &zero]);
    Name::new(Path::new("\\_S5_"), &s5).to_aml_bytes(sink);
}

/// Emits `Device(path) { Name(name, value) ... }`.
fn device(path: &str, objects: &[(&str, &dyn Aml)], sink: &mut Vec<u8>) {
    let names: Vec<Name> = objects
        .iter()
        .map(|&(name, value)| Name::new(Path::new(name), value))
        .collect();
    let children: Vec<&dyn Aml> = names.iter().map(|name| name as &dyn Aml).collect();
    Device::new(Path::new(path), children).to_aml_bytes(sink);
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0xe_0000;

    fn tables(cpus: u8) -> AcpiTables {
        build(
            &AcpiConfig {
                cpus,
                mmio64: (0x1_0000_0000, 0x9_0000_0000),
            },
            BASE,
        )
    }

    fn at(tables: &AcpiTables, address: u64) -> &[u8] {
        &tables.bytes[(address - BASE) as usize..]
    }

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn u64_at(bytes: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }

    /// A table at `address`, checked for a valid length and checksum.
    fn table<'a>(tables: &'a AcpiTables, address: u64, signature: &[u8; 4]) -> &'a [u8] {
        let bytes = at(tables, address);
        assert_eq!(&bytes[0..4], signature);
        let len = u32_at(bytes, 4) as usize;
        let table = &bytes[..len];
        assert_eq!(
            table.iter().fold(0u8, |s, &b| s.wrapping_add(b)),
            0,
            "{} checksum",
            String::from_utf8_lossy(signature)
        );
        table
    }

    /// The XSDT entries, by signature.
    fn xsdt_entries(tables: &AcpiTables) -> Vec<(String, u64)> {
        let rsdp = at(tables, tables.rsdp);
        let xsdt = table(tables, u64_at(rsdp, 24), b"XSDT");
        (HEADER_LEN..xsdt.len())
            .step_by(8)
            .map(|offset| {
                let address = u64_at(xsdt, offset);
                let signature = String::from_utf8_lossy(&at(tables, address)[0..4]).into_owned();
                (signature, address)
            })
            .collect()
    }

    fn find(tables: &AcpiTables, signature: &str) -> u64 {
        xsdt_entries(tables)
            .into_iter()
            .find(|(s, _)| s == signature)
            .unwrap_or_else(|| panic!("{signature} missing"))
            .1
    }

    #[test]
    fn rsdp_is_valid_and_points_at_the_xsdt() {
        let tables = tables(4);
        let rsdp = at(&tables, tables.rsdp);
        assert_eq!(&rsdp[0..8], b"RSD PTR ");
        assert_eq!(rsdp[15], 2);
        assert_eq!(rsdp[..20].iter().fold(0u8, |s, &b| s.wrapping_add(b)), 0);
        assert_eq!(rsdp[..36].iter().fold(0u8, |s, &b| s.wrapping_add(b)), 0);
        let signatures: Vec<String> = xsdt_entries(&tables).into_iter().map(|(s, _)| s).collect();
        assert_eq!(signatures, ["FACP", "APIC", "MCFG"]);
    }

    #[test]
    fn madt_lists_every_cpu_the_ioapic_and_overrides() {
        let tables = tables(4);
        let madt = table(&tables, find(&tables, "APIC"), b"APIC");
        assert_eq!(u32_at(madt, 36), LAPIC_BASE);
        assert_eq!(u32_at(madt, 40), 1, "PCAT_COMPAT");

        let mut lapics = Vec::new();
        let mut ioapics = Vec::new();
        let mut overrides = Vec::new();
        let mut offset = 44;
        while offset < madt.len() {
            let (kind, len) = (madt[offset], madt[offset + 1] as usize);
            let entry = &madt[offset..offset + len];
            match kind {
                0 => lapics.push((entry[3], u32_at(entry, 4))),
                1 => ioapics.push((entry[2], u32_at(entry, 4))),
                2 => overrides.push((entry[3], u32_at(entry, 4))),
                _ => {}
            }
            offset += len;
        }
        assert_eq!(lapics, [(0, 1), (1, 1), (2, 1), (3, 1)]);
        assert_eq!(ioapics, [(IOAPIC_ID, IOAPIC_BASE as u32)]);
        assert_eq!(overrides, [(0, 2), (SCI_IRQ, u32::from(SCI_IRQ))]);
    }

    #[test]
    fn fadt_points_at_dsdt_facs_and_the_pm_registers() {
        let tables = tables(1);
        let fadt = table(&tables, find(&tables, "FACP"), b"FACP");
        assert_eq!(fadt.len(), 276);
        assert_eq!(fadt[8], 6, "revision");
        let dsdt = u64_at(fadt, 140);
        table(&tables, dsdt, b"DSDT");
        assert_eq!(u64::from(u32_at(fadt, 40)), dsdt);
        let facs = u64::from(u32_at(fadt, 36));
        assert_eq!(&at(&tables, facs)[0..4], b"FACS");
        assert_eq!(facs % 64, 0, "FACS is 64-byte aligned");
        assert_eq!(
            u64_at(fadt, 132),
            0,
            "X_FIRMWARE_CTRL must be 0 alongside FIRMWARE_CTRL"
        );

        assert_eq!(u32_at(fadt, 48), 0, "no SMI command port");
        assert_eq!(u32_at(fadt, 56), u32::from(PM1A_EVT));
        assert_eq!(u32_at(fadt, 64), u32::from(PM1A_CNT));
        assert_eq!(u32_at(fadt, 76), u32::from(PM_TMR));
        assert_eq!(u64_at(fadt, 148 + 4), u64::from(PM1A_EVT), "X_PM1a_EVT_BLK");
        assert_eq!(u64_at(fadt, 208 + 4), u64::from(PM_TMR), "X_PM_TMR_BLK");
        assert_eq!(u64_at(fadt, 116 + 4), 0xcf9, "reset register");
        assert_eq!(fadt[128], 6, "reset value");
    }

    #[test]
    fn mcfg_describes_the_ecam_window() {
        let tables = tables(1);
        let mcfg = table(&tables, find(&tables, "MCFG"), b"MCFG");
        assert_eq!(u64_at(mcfg, 44), ECAM_BASE);
        assert_eq!((mcfg[54], mcfg[55]), (0, 255), "buses 0-255");
    }

    #[test]
    fn dsdt_declares_the_root_complex_and_soft_off() {
        let tables = tables(1);
        let fadt = table(&tables, find(&tables, "FACP"), b"FACP");
        let dsdt = table(&tables, u64_at(fadt, 140), b"DSDT");
        let contains = |needle: &[u8]| dsdt.windows(needle.len()).any(|w| w == needle);
        for name in [
            &b"PCI0"[..],
            b"MBRS",
            b"COM1",
            b"KBD_",
            b"RTC_",
            b"_S5_",
            b"_CRS",
        ] {
            assert!(contains(name), "{}", String::from_utf8_lossy(name));
        }
        // PNP0A08 as an EISA ID: 0x41d0 0a08, compressed to bytes 41 D0 0A 08.
        assert!(contains(&[0x41, 0xd0, 0x0a, 0x08]));
    }
}
