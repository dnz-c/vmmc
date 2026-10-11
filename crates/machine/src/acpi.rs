//! ACPI tables: RSDP, XSDT, FADT, FACS, MADT, MCFG and the DSDT.
//!
//! [`build`] produces every table in one relocatable blob, plus the RSDP. Direct boot
//! places them at a fixed address ([`AcpiBlobs::place`]). UEFI firmware instead gets
//! them through fw_cfg with a QEMU-style table-loader script ([`AcpiBlobs::loader`])
//! and installs them itself.
//!
//! The static tables are written field by field; the DSDT's AML comes from the
//! `acpi_tables` builder.

use acpi_tables::Aml;
use acpi_tables::aml::{
    AddressSpace, AddressSpaceCacheable, And, Arg, CreateDWordField, Device, EISAName, Else, Equal,
    IO, If, Interrupt, Local, Memory32Fixed, Method, Name, NotEqual, Or, Package, Path,
    ResourceTemplate, Return, Store, Uuid,
};
use devices::legacy::acpi_pm::{
    PM_TMR, PM_TMR_LEN, PM1A_CNT, PM1A_CNT_LEN, PM1A_EVT, PM1A_EVT_LEN, S5_SLEEP_TYPE,
};
use devices::legacy::i8042::{I8042_COMMAND, I8042_DATA};
use devices::legacy::ioapic::IOAPIC_BASE;
use devices::legacy::rtc::RTC_BASE;
use devices::legacy::serial::{COM1_BASE, COM1_IRQ};
use devices::pci::root::{CONFIG_IO_BASE, NUM_SLOTS};

const OEM_ID: [u8; 6] = *b"VMMC  ";
const OEM_TABLE_ID: [u8; 8] = *b"VMMCPC  ";
const CREATOR_ID: [u8; 4] = *b"VMMC";
const HEADER_LEN: usize = 36;
/// Offset of the checksum byte in a table header.
const CHECKSUM: usize = 9;

/// The local APIC base every x86 CPU uses.
pub const LAPIC_BASE: u32 = 0xfee0_0000;
/// The IOAPIC's APIC ID.
pub const IOAPIC_ID: u8 = 0;
/// SCI interrupt (ISA IRQ 9, level-triggered, active high).
pub const SCI_IRQ: u8 = 9;
/// PCI INTx lines land on IOAPIC pins 16 to 23.
pub const PCI_INTX_GSI_BASE: u32 = 16;
/// Reset register: writing 6 to port 0xcf9 resets the system.
const RESET_PORT: u16 = 0xcf9;
const RESET_VALUE: u8 = 0x06;
/// CMOS index of the century byte.
const RTC_CENTURY: u8 = 0x32;
/// PCIe features `_OSC` hands to the OS: native hot-plug, PME, AER and the PCIe
/// capability structure (not SHPC; there are no PCI-to-PCI bridges).
const OSC_GRANTED: u8 = 0x1d;

/// The GSI that INTx `pin` (0 for INTA) of the device in `slot` is wired to.
pub fn intx_gsi(slot: u8, pin: u8) -> u32 {
    PCI_INTX_GSI_BASE + (u32::from(slot) + u32::from(pin)) % 8
}

/// What the tables describe.
#[derive(Clone, Debug)]
pub struct AcpiConfig {
    pub cpus: u8,
    /// The 32-bit PCI MMIO window, `[start, end)`.
    pub mmio32: (u64, u64),
    /// The 64-bit PCI MMIO window, `[start, end)`.
    pub mmio64: (u64, u64),
    /// The ECAM window, `(base, size)`.
    pub ecam: (u64, u64),
    /// I/O base of the ACPI PM block.
    pub pm_base: u16,
}

/// fw_cfg names of the loader's files, as QEMU names them.
pub const RSDP_FILE: &str = "etc/acpi/rsdp";
pub const TABLES_FILE: &str = "etc/acpi/tables";
pub const LOADER_FILE: &str = "etc/table-loader";

const RSDP_LEN: usize = 36;
/// Where the XSDT address sits in the RSDP.
const RSDP_XSDT: usize = 24;
/// Where the tables start, relative to the RSDP, when placed for direct boot. The FACS
/// is first and must be 64-byte aligned.
const PLACED_TABLES_OFFSET: u64 = 64;

/// The tables before they're placed in guest memory. Pointers between tables hold
/// offsets into `tables`, and checksums are zero; placing the tables fixes both.
#[derive(Clone, Debug)]
pub struct AcpiBlobs {
    pub tables: Vec<u8>,
    pub rsdp: [u8; RSDP_LEN],
    /// Pointer fields in `tables`, as `(offset, width)`.
    pointers: Vec<(usize, u8)>,
    /// Tables with a checksum, as `(offset, length)` in `tables`.
    checksummed: Vec<(usize, usize)>,
}

/// Tables laid out for a guest-physical base address.
#[derive(Clone, Debug)]
pub struct AcpiTables {
    pub bytes: Vec<u8>,
    /// Address of the RSDP (which is at the base).
    pub rsdp: u64,
}

/// The fw_cfg files firmware's ACPI loader reads.
#[derive(Clone, Debug)]
pub struct LoaderFiles {
    pub rsdp: Vec<u8>,
    pub tables: Vec<u8>,
    pub script: Vec<u8>,
}

/// Builds every table.
pub fn build(config: &AcpiConfig) -> AcpiBlobs {
    let mut blob = Blob::default();
    let facs = blob.add(Table::raw(facs()), 64);
    let dsdt = blob.add(dsdt(config), 16);
    let fadt = blob.add(fadt(config, facs, dsdt), 16);
    let madt = blob.add(madt(config), 16);
    let mcfg = blob.add(mcfg(config), 16);
    let xsdt = blob.add(xsdt(&[fadt, madt, mcfg]), 16);
    AcpiBlobs {
        tables: blob.bytes,
        rsdp: rsdp(xsdt),
        pointers: blob.pointers,
        checksummed: blob.checksummed,
    }
}

impl AcpiBlobs {
    /// Lays the tables out to be copied to guest-physical `base` (64-byte aligned):
    /// the RSDP first, where a scan of the BIOS area finds it, then the tables.
    pub fn place(&self, base: u64) -> AcpiTables {
        assert!(base.is_multiple_of(64));
        let tables_base = base + PLACED_TABLES_OFFSET;
        let mut tables = self.tables.clone();
        for &(offset, width) in &self.pointers {
            add_to_field(
                &mut tables[offset..offset + usize::from(width)],
                tables_base,
            );
        }
        for &(offset, len) in &self.checksummed {
            tables[offset + CHECKSUM] = checksum(&tables[offset..offset + len]);
        }
        let mut rsdp = self.rsdp;
        add_to_field(&mut rsdp[RSDP_XSDT..RSDP_XSDT + 8], tables_base);
        rsdp[8] = checksum(&rsdp[..20]);
        rsdp[32] = checksum(&rsdp);

        let mut bytes = rsdp.to_vec();
        bytes.resize(PLACED_TABLES_OFFSET as usize, 0);
        bytes.extend_from_slice(&tables);
        AcpiTables { bytes, rsdp: base }
    }

    /// The RSDP, the tables, and the loader script that has firmware allocate them,
    /// patch the pointers and compute the checksums.
    pub fn loader(&self) -> LoaderFiles {
        let mut script = Vec::new();
        loader::allocate(&mut script, RSDP_FILE, 16, loader::ZONE_FSEG);
        loader::allocate(&mut script, TABLES_FILE, 64, loader::ZONE_HIGH);
        for &(offset, width) in &self.pointers {
            loader::add_pointer(&mut script, TABLES_FILE, TABLES_FILE, offset, width);
        }
        loader::add_pointer(&mut script, RSDP_FILE, TABLES_FILE, RSDP_XSDT, 8);
        for &(offset, len) in &self.checksummed {
            loader::add_checksum(&mut script, TABLES_FILE, offset + CHECKSUM, offset, len);
        }
        // The extended checksum covers the first one, so it goes last.
        loader::add_checksum(&mut script, RSDP_FILE, 8, 0, 20);
        loader::add_checksum(&mut script, RSDP_FILE, 32, 0, RSDP_LEN);
        LoaderFiles {
            rsdp: self.rsdp.to_vec(),
            tables: self.tables.clone(),
            script,
        }
    }
}

/// QEMU's BIOS linker/loader commands: 128-byte little-endian entries.
mod loader {
    const ENTRY_LEN: usize = 128;
    const FILE_NAME_LEN: usize = 56;
    const ALLOCATE: u32 = 1;
    const ADD_POINTER: u32 = 2;
    const ADD_CHECKSUM: u32 = 3;
    pub const ZONE_HIGH: u8 = 1;
    /// The 0xf0000 segment, where legacy scans look for the RSDP.
    pub const ZONE_FSEG: u8 = 2;

    fn entry(script: &mut Vec<u8>, command: u32, fields: &[&[u8]]) {
        let start = script.len();
        script.extend_from_slice(&command.to_le_bytes());
        for field in fields {
            script.extend_from_slice(field);
        }
        assert!(script.len() - start <= ENTRY_LEN);
        script.resize(start + ENTRY_LEN, 0);
    }

    fn name(file: &str) -> [u8; FILE_NAME_LEN] {
        assert!(file.len() < FILE_NAME_LEN);
        let mut field = [0u8; FILE_NAME_LEN];
        field[..file.len()].copy_from_slice(file.as_bytes());
        field
    }

    pub fn allocate(script: &mut Vec<u8>, file: &str, align: u32, zone: u8) {
        entry(
            script,
            ALLOCATE,
            &[&name(file), &align.to_le_bytes(), &[zone]],
        );
    }

    /// Adds `source`'s address to the `width`-byte field at `offset` in `dest`.
    pub fn add_pointer(script: &mut Vec<u8>, dest: &str, source: &str, offset: usize, width: u8) {
        entry(
            script,
            ADD_POINTER,
            &[
                &name(dest),
                &name(source),
                &(offset as u32).to_le_bytes(),
                &[width],
            ],
        );
    }

    /// Stores the checksum of `start..start + len` in `file` at `offset`.
    pub fn add_checksum(script: &mut Vec<u8>, file: &str, offset: usize, start: usize, len: usize) {
        entry(
            script,
            ADD_CHECKSUM,
            &[
                &name(file),
                &(offset as u32).to_le_bytes(),
                &(start as u32).to_le_bytes(),
                &(len as u32).to_le_bytes(),
            ],
        );
    }
}

/// Adds `value` to the little-endian integer in `field`.
fn add_to_field(field: &mut [u8], value: u64) {
    let mut bytes = [0u8; 8];
    bytes[..field.len()].copy_from_slice(field);
    let sum = u64::from_le_bytes(bytes).wrapping_add(value);
    field.copy_from_slice(&sum.to_le_bytes()[..field.len()]);
}

/// Sum-to-zero checksum byte for `bytes`.
fn checksum(bytes: &[u8]) -> u8 {
    0u8.wrapping_sub(bytes.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)))
}

#[derive(Default)]
struct Blob {
    bytes: Vec<u8>,
    pointers: Vec<(usize, u8)>,
    checksummed: Vec<(usize, usize)>,
}

impl Blob {
    /// Appends `table` at the next `align`-aligned offset and returns that offset.
    fn add(&mut self, mut table: Table, align: usize) -> usize {
        let len = table.bytes.len();
        if table.header {
            table.u32(4, len as u32);
        }
        let offset = self.bytes.len().next_multiple_of(align);
        self.bytes.resize(offset, 0);
        self.bytes.extend_from_slice(&table.bytes);
        self.pointers.extend(
            table
                .pointers
                .iter()
                .map(|&(at, width)| (offset + at, width)),
        );
        if table.header {
            self.checksummed.push((offset, len));
        }
        offset
    }
}

/// One table: fields written by offset, plus the pointer fields in it.
struct Table {
    bytes: Vec<u8>,
    pointers: Vec<(usize, u8)>,
    /// Whether it starts with the standard header (length and checksum).
    header: bool,
}

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
        Self {
            bytes,
            pointers: Vec::new(),
            header: true,
        }
    }

    /// A structure without the standard header, like the FACS.
    fn raw(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            pointers: Vec::new(),
            header: false,
        }
    }

    fn u8(&mut self, offset: usize, value: u8) {
        self.bytes[offset] = value;
    }

    fn u16(&mut self, offset: usize, value: u16) {
        self.bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, offset: usize, value: u32) {
        self.bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, offset: usize, value: u64) {
        self.bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    /// A `width`-byte pointer at `offset` to the table at blob offset `target`.
    fn pointer(&mut self, offset: usize, width: u8, target: usize) {
        let value = (target as u64).to_le_bytes();
        let width_bytes = usize::from(width);
        self.bytes[offset..offset + width_bytes].copy_from_slice(&value[..width_bytes]);
        self.pointers.push((offset, width));
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
        self.bytes.extend_from_slice(bytes);
    }
}

/// The RSDP, pointing at the XSDT at blob offset `xsdt`. Checksums are left zero.
fn rsdp(xsdt: usize) -> [u8; RSDP_LEN] {
    let mut rsdp = [0u8; RSDP_LEN];
    rsdp[0..8].copy_from_slice(b"RSD PTR ");
    rsdp[9..15].copy_from_slice(&OEM_ID);
    rsdp[15] = 2; // ACPI 2.0+: use the XSDT
    rsdp[20..24].copy_from_slice(&(RSDP_LEN as u32).to_le_bytes());
    rsdp[RSDP_XSDT..RSDP_XSDT + 8].copy_from_slice(&(xsdt as u64).to_le_bytes());
    rsdp
}

fn xsdt(entries: &[usize]) -> Table {
    let mut table = Table::new(b"XSDT", 1, HEADER_LEN + 8 * entries.len());
    for (i, &entry) in entries.iter().enumerate() {
        table.pointer(HEADER_LEN + 8 * i, 8, entry);
    }
    table
}

fn facs() -> Vec<u8> {
    let mut facs = vec![0u8; 64];
    facs[0..4].copy_from_slice(b"FACS");
    facs[4..8].copy_from_slice(&64u32.to_le_bytes());
    facs[32] = 2; // version
    facs
}

fn fadt(config: &AcpiConfig, facs: usize, dsdt: usize) -> Table {
    // FADT flags.
    const WBINVD: u32 = 1 << 0;
    const PROC_C1: u32 = 1 << 2;
    const SLP_BUTTON: u32 = 1 << 5;
    const RESET_REG_SUP: u32 = 1 << 10;
    // IA-PC boot architecture flags.
    const LEGACY_DEVICES: u16 = 1 << 0;
    const HAS_8042: u16 = 1 << 1;
    const VGA_NOT_PRESENT: u16 = 1 << 2;

    let pm1a_evt = config.pm_base + PM1A_EVT;
    let pm1a_cnt = config.pm_base + PM1A_CNT;
    let pm_tmr = config.pm_base + PM_TMR;
    let mut t = Table::new(b"FACP", 6, 276);
    t.pointer(36, 4, facs);
    t.pointer(40, 4, dsdt);
    t.u8(45, 1); // preferred PM profile: desktop
    t.u16(46, SCI_IRQ.into());
    // SMI_CMD stays 0: the platform is always in ACPI mode.
    t.u32(56, pm1a_evt.into());
    t.u32(64, pm1a_cnt.into());
    t.u32(76, pm_tmr.into());
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
    t.pointer(140, 8, dsdt);
    t.io_gas(148, pm1a_evt, PM1A_EVT_LEN);
    t.io_gas(172, pm1a_cnt, PM1A_CNT_LEN);
    t.io_gas(208, pm_tmr, PM_TMR_LEN);
    t
}

fn madt(config: &AcpiConfig) -> Table {
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
    t
}

/// The last bus number the ECAM window covers.
fn last_bus(config: &AcpiConfig) -> u8 {
    ((config.ecam.1 >> 20).clamp(1, 256) - 1) as u8
}

fn mcfg(config: &AcpiConfig) -> Table {
    let mut t = Table::new(b"MCFG", 1, HEADER_LEN + 8);
    // One allocation: base, segment 0, buses 0 to the last.
    t.push(&config.ecam.0.to_le_bytes());
    t.push(&[0, 0, 0, last_bus(config)]);
    t.push(&[0; 4]);
    t
}

fn dsdt(config: &AcpiConfig) -> Table {
    let mut t = Table::new(b"DSDT", 2, HEADER_LEN);
    let mut aml = Vec::new();
    dsdt_aml(config, &mut aml);
    t.push(&aml);
    t
}

fn dsdt_aml(config: &AcpiConfig, sink: &mut Vec<u8>) {
    pci_root(config, sink);

    // Motherboard resources: reserves the ECAM window, which Linux requires before it
    // will use ECAM.
    let ecam = Memory32Fixed::new(true, config.ecam.0 as u32, config.ecam.1 as u32);
    let one = 1u8;
    device(
        "\\_SB_.MBRS",
        &[
            ("_HID", &EISAName::new("PNP0C02")),
            ("_UID", &one),
            ("_CRS", &ResourceTemplate::new(vec![&ecam])),
        ],
        &[],
        sink,
    );

    // Processor devices, matched to the MADT entries by UID. Windows only brings up
    // the processors it finds here.
    for cpu in 0..config.cpus {
        let uid = u32::from(cpu);
        device(
            &format!("\\_SB_.C{cpu:03X}"),
            &[("_HID", &"ACPI0007"), ("_UID", &uid)],
            &[],
            sink,
        );
    }

    // The legacy devices sit behind the ICH9 LPC bridge (00:1f.0), inside PCI0: its
    // I/O window covers their ports, and Windows won't start a root bridge whose
    // windows overlap devices outside it (problem 12, conflicting addresses).
    let lpc_address = 0x001f_0000u32;
    device("\\_SB_.PCI0.ISA_", &[("_ADR", &lpc_address)], &[], sink);

    let com1_io = IO::new(COM1_BASE, COM1_BASE, 1, 8);
    let com1_irq = Interrupt::new(true, true, false, false, COM1_IRQ.into());
    device(
        "\\_SB_.PCI0.ISA_.COM1",
        &[
            ("_HID", &EISAName::new("PNP0501")),
            ("_UID", &one),
            ("_CRS", &ResourceTemplate::new(vec![&com1_io, &com1_irq])),
        ],
        &[],
        sink,
    );

    // The 8042 keyboard controller.
    let kbd_data = IO::new(I8042_DATA, I8042_DATA, 1, 1);
    let kbd_command = IO::new(I8042_COMMAND, I8042_COMMAND, 1, 1);
    let kbd_irq = Interrupt::new(true, true, false, false, 1);
    device(
        "\\_SB_.PCI0.ISA_.KBD_",
        &[
            ("_HID", &EISAName::new("PNP0303")),
            (
                "_CRS",
                &ResourceTemplate::new(vec![&kbd_data, &kbd_command, &kbd_irq]),
            ),
        ],
        &[],
        sink,
    );

    // The CMOS RTC. Its IRQ 8 isn't listed: alarm and update interrupts aren't
    // emulated, and Linux's driver would wait for them.
    let rtc_io = IO::new(RTC_BASE, RTC_BASE, 1, 2);
    device(
        "\\_SB_.PCI0.ISA_.RTC_",
        &[
            ("_HID", &EISAName::new("PNP0B00")),
            ("_CRS", &ResourceTemplate::new(vec![&rtc_io])),
        ],
        &[],
        sink,
    );

    // \_S5: soft-off uses SLP_TYP 0, what the ACPI PM device powers off on.
    let s5_type = S5_SLEEP_TYPE as u8;
    let zero = 0u8;
    let s5 = Package::new(vec![&s5_type, &s5_type, &zero, &zero]);
    Name::new(Path::new("\\_S5_"), &s5).to_aml_bytes(sink);
}

/// \_SB.PCI0: the PCIe root complex, the windows it decodes, its INTx routing and the
/// `_OSC` handshake.
fn pci_root(config: &AcpiConfig, sink: &mut Vec<u8>) {
    let buses = AddressSpace::<u16>::new_bus_number(0, last_bus(config).into());
    let config_ports = IO::new(CONFIG_IO_BASE, CONFIG_IO_BASE, 1, 8);
    let io_low = AddressSpace::<u16>::new_io(0, CONFIG_IO_BASE - 1, None);
    let io_high = AddressSpace::<u16>::new_io(0xd00, 0xffff, None);
    let mmio32 = AddressSpace::<u32>::new_memory(
        AddressSpaceCacheable::NotCacheable,
        true,
        config.mmio32.0 as u32,
        (config.mmio32.1 - 1) as u32,
        None,
    );
    let mmio64 = AddressSpace::<u64>::new_memory(
        AddressSpaceCacheable::NotCacheable,
        true,
        config.mmio64.0,
        config.mmio64.1 - 1,
        None,
    );
    let crs = ResourceTemplate::new(vec![
        &buses,
        &config_ports,
        &io_low,
        &io_high,
        &mmio32,
        &mmio64,
    ]);

    // _PRT: every slot's INTA-INTD, as (address, pin, source, GSI).
    let zero = 0u8;
    let routes: Vec<(u32, u8, u32)> = (0..NUM_SLOTS)
        .flat_map(|slot| {
            (0..4u8).map(move |pin| ((u32::from(slot) << 16) | 0xffff, pin, intx_gsi(slot, pin)))
        })
        .collect();
    let entries: Vec<Package> = routes
        .iter()
        .map(|(address, pin, gsi)| Package::new(vec![address, pin, &zero, gsi]))
        .collect();
    let prt = Package::new(entries.iter().map(|entry| entry as &dyn Aml).collect());

    // _OSC(UUID, revision, count, capabilities): grant the PCIe features we allow and
    // flag anything else the OS asked for.
    let cdw1 = Path::new("CDW1");
    let cdw3 = Path::new("CDW3");
    let capabilities = Arg(3);
    let granted = Local(0);
    let (dword0, dword2) = (0u8, 8u8);
    let create_cdw1 = CreateDWordField::new(&cdw1, &capabilities, &dword0);
    let create_cdw3 = CreateDWordField::new(&cdw3, &capabilities, &dword2);
    let pcie_uuid = Uuid::new("33DB4D5B-1FF7-401C-9657-7441C03DD766");
    let is_pcie = Equal::new(&Arg(0), &pcie_uuid);
    let mask = OSC_GRANTED;
    let grant = And::new(&granted, &cdw3, &mask);
    let revision_one = 1u8;
    let bad_revision = NotEqual::new(&Arg(1), &revision_one);
    let (unknown_revision, masked, unknown_uuid) = (0x08u8, 0x10u8, 0x04u8);
    let flag_revision = Or::new(&cdw1, &cdw1, &unknown_revision);
    let check_revision = If::new(&bad_revision, vec![&flag_revision]);
    let was_masked = NotEqual::new(&cdw3, &granted);
    let flag_masked = Or::new(&cdw1, &cdw1, &masked);
    let check_masked = If::new(&was_masked, vec![&flag_masked]);
    let store_granted = Store::new(&cdw3, &granted);
    let if_pcie = If::new(
        &is_pcie,
        vec![
            &create_cdw3,
            &grant,
            &check_revision,
            &check_masked,
            &store_granted,
        ],
    );
    let flag_uuid = Or::new(&cdw1, &cdw1, &unknown_uuid);
    let not_pcie = Else::new(vec![&flag_uuid]);
    let return_capabilities = Return::new(&capabilities);
    let osc = Method::new(
        Path::new("_OSC"),
        4,
        true,
        vec![&create_cdw1, &if_pcie, &not_pcie, &return_capabilities],
    );

    device(
        "\\_SB_.PCI0",
        &[
            ("_HID", &EISAName::new("PNP0A08")),
            ("_CID", &EISAName::new("PNP0A03")),
            ("_ADR", &zero),
            ("_SEG", &zero),
            ("_UID", &zero),
            ("_BBN", &zero),
            ("_CRS", &crs),
            ("_PRT", &prt),
        ],
        &[&osc],
        sink,
    );
}

/// Emits `Device(path) { Name(name, value) ... }`, followed by `extra` objects.
fn device(path: &str, names: &[(&str, &dyn Aml)], extra: &[&dyn Aml], sink: &mut Vec<u8>) {
    let names: Vec<Name> = names
        .iter()
        .map(|&(name, value)| Name::new(Path::new(name), value))
        .collect();
    let children: Vec<&dyn Aml> = names
        .iter()
        .map(|name| name as &dyn Aml)
        .chain(extra.iter().copied())
        .collect();
    Device::new(Path::new(path), children).to_aml_bytes(sink);
}

#[cfg(test)]
mod tests {
    use devices::legacy::acpi_pm::PM_BASE;

    use super::*;

    const BASE: u64 = 0xe_0000;
    const ECAM: (u64, u64) = (0xb000_0000, 0x1000_0000);

    fn config(cpus: u8) -> AcpiConfig {
        AcpiConfig {
            cpus,
            mmio32: (0xc000_0000, 0xfec0_0000),
            mmio64: (0x1_0000_0000, 0x9_0000_0000),
            ecam: ECAM,
            pm_base: PM_BASE,
        }
    }

    fn tables(cpus: u8) -> AcpiTables {
        build(&config(cpus)).place(BASE)
    }

    /// Runs the loader script the way OVMF does: allocates each file at a fixed
    /// address in one flat memory starting at `BASE`, then patches and checksums.
    fn load(files: &LoaderFiles) -> AcpiTables {
        let placed: [(&str, &[u8], u64); 2] = [
            (RSDP_FILE, &files.rsdp, BASE),
            (TABLES_FILE, &files.tables, BASE + 0x1000),
        ];
        let mut memory = vec![0u8; 0x1000 + files.tables.len()];
        let address = |name: &str| placed.iter().find(|p| p.0 == name).unwrap().2;
        let name = |field: &[u8]| {
            let end = field.iter().position(|&b| b == 0).unwrap();
            String::from_utf8(field[..end].to_vec()).unwrap()
        };
        let u32_le = |bytes: &[u8]| u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;

        assert_eq!(files.script.len() % 128, 0);
        for entry in files.script.chunks(128) {
            match u32_le(entry) {
                1 => {
                    let file = name(&entry[4..60]);
                    let (_, bytes, at) = placed.iter().find(|p| p.0 == file).unwrap();
                    assert_eq!(*at % u32_le(&entry[60..]) as u64, 0, "alignment");
                    let at = (*at - BASE) as usize;
                    memory[at..at + bytes.len()].copy_from_slice(bytes);
                }
                2 => {
                    let dest = address(&name(&entry[4..60])) - BASE;
                    let source = address(&name(&entry[60..116]));
                    let offset = dest as usize + u32_le(&entry[116..]);
                    let width = usize::from(entry[120]);
                    add_to_field(&mut memory[offset..offset + width], source);
                }
                3 => {
                    let file = (address(&name(&entry[4..60])) - BASE) as usize;
                    let (at, start, len) = (
                        u32_le(&entry[60..]),
                        u32_le(&entry[64..]),
                        u32_le(&entry[68..]),
                    );
                    let range = file + start..file + start + len;
                    memory[file + at] = checksum(&memory[range]);
                }
                command => panic!("unknown loader command {command}"),
            }
        }
        AcpiTables {
            bytes: memory,
            rsdp: BASE,
        }
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

    /// The XSDT entries, by signature, after checking the RSDP.
    fn xsdt_entries(tables: &AcpiTables) -> Vec<(String, u64)> {
        let rsdp = at(tables, tables.rsdp);
        assert_eq!(&rsdp[0..8], b"RSD PTR ");
        assert_eq!(rsdp[15], 2);
        assert_eq!(rsdp[..20].iter().fold(0u8, |s, &b| s.wrapping_add(b)), 0);
        assert_eq!(rsdp[..36].iter().fold(0u8, |s, &b| s.wrapping_add(b)), 0);
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

    fn dsdt(tables: &AcpiTables) -> &[u8] {
        let fadt = table(tables, find(tables, "FACP"), b"FACP");
        table(tables, u64_at(fadt, 140), b"DSDT")
    }

    #[test]
    fn rsdp_is_valid_and_points_at_the_xsdt() {
        let tables = tables(4);
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
        assert_eq!(u32_at(fadt, 56), 0x600);
        assert_eq!(u32_at(fadt, 64), 0x604);
        assert_eq!(u32_at(fadt, 76), 0x608);
        assert_eq!(u64_at(fadt, 148 + 4), 0x600, "X_PM1a_EVT_BLK");
        assert_eq!(u64_at(fadt, 208 + 4), 0x608, "X_PM_TMR_BLK");
        assert_eq!(u64_at(fadt, 116 + 4), 0xcf9, "reset register");
        assert_eq!(fadt[128], 6, "reset value");
    }

    #[test]
    fn fadt_follows_the_pm_block() {
        let tables = build(&AcpiConfig {
            pm_base: 0xb000,
            ..config(1)
        })
        .place(BASE);
        let fadt = table(&tables, find(&tables, "FACP"), b"FACP");
        assert_eq!(u32_at(fadt, 56), 0xb000);
        assert_eq!(u64_at(fadt, 208 + 4), 0xb008);
    }

    #[test]
    fn mcfg_describes_the_ecam_window() {
        let tables = tables(1);
        let mcfg = table(&tables, find(&tables, "MCFG"), b"MCFG");
        assert_eq!(u64_at(mcfg, 44), ECAM.0);
        assert_eq!((mcfg[54], mcfg[55]), (0, 255), "buses 0-255");

        let small = build(&AcpiConfig {
            ecam: (0xe000_0000, 64 << 20),
            ..config(1)
        })
        .place(BASE);
        let mcfg = table(&small, find(&small, "MCFG"), b"MCFG");
        assert_eq!(u64_at(mcfg, 44), 0xe000_0000);
        assert_eq!(mcfg[55], 63, "64 buses");
    }

    #[test]
    fn dsdt_declares_devices_routing_and_soft_off() {
        let tables = tables(2);
        let dsdt = dsdt(&tables);
        let contains = |needle: &[u8]| dsdt.windows(needle.len()).any(|w| w == needle);
        for name in [
            &b"PCI0"[..],
            b"ISA_",
            b"MBRS",
            b"COM1",
            b"KBD_",
            b"RTC_",
            b"_S5_",
            b"_CRS",
            b"_PRT",
            b"_OSC",
            b"C000",
            b"C001",
            b"ACPI0007",
        ] {
            assert!(contains(name), "{}", String::from_utf8_lossy(name));
        }
        assert!(!contains(b"C002"), "only two processors");
        // PNP0A08 as an EISA ID: 0x41d0 0a08, compressed to bytes 41 D0 0A 08.
        assert!(contains(&[0x41, 0xd0, 0x0a, 0x08]));
        // \_S5_ = Package(4) { 0, 0, 0, 0 }: NameOp, path, PackageOp, length, count.
        assert!(contains(&[
            b'_', b'S', b'5', b'_', 0x12, 0x06, 0x04, 0, 0, 0, 0
        ]));
    }

    #[test]
    fn intx_routing_spreads_slots_over_eight_gsis() {
        assert_eq!(intx_gsi(0, 0), 16);
        assert_eq!(intx_gsi(1, 0), 17);
        assert_eq!(intx_gsi(1, 3), 20);
        assert_eq!(intx_gsi(7, 1), 16);
        assert!((0..32).all(|slot| (0..4).all(|pin| (16..24).contains(&intx_gsi(slot, pin)))));
    }

    #[test]
    fn the_loader_script_produces_the_same_tables() {
        let blobs = build(&config(4));
        let files = blobs.loader();
        assert!(
            files.rsdp[8] == 0 && files.rsdp[32] == 0,
            "checksums are the loader's job"
        );
        let loaded = load(&files);
        let entries = xsdt_entries(&loaded);
        let signatures: Vec<&str> = entries.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(signatures, ["FACP", "APIC", "MCFG"]);
        for (signature, address) in &entries {
            let signature: [u8; 4] = signature.as_bytes().try_into().unwrap();
            table(&loaded, *address, &signature);
        }
        let fadt = table(&loaded, find(&loaded, "FACP"), b"FACP");
        let facs = u64::from(u32_at(fadt, 36));
        assert_eq!(&at(&loaded, facs)[0..4], b"FACS");
        assert_eq!(facs % 64, 0);
        assert_eq!(
            dsdt(&loaded),
            dsdt(&blobs.place(BASE)),
            "same DSDT either way"
        );
    }

    #[test]
    fn regenerating_with_new_windows_keeps_the_length() {
        // The loader files' sizes are fixed when the fw_cfg directory is built, but
        // the content is regenerated once firmware has assigned BARs.
        let before = build(&config(4));
        let after = build(&AcpiConfig {
            mmio64: (0x38_0000_0000, 0x40_0000_0000),
            ecam: (0xe000_0000, 0x1000_0000),
            pm_base: 0xb000,
            ..config(4)
        });
        assert_eq!(before.tables.len(), after.tables.len());
        assert_eq!(before.loader().script, after.loader().script);
    }
}
