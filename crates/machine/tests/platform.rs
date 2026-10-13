//! End-to-end platform tests on a fake hypervisor: scripted "guests" make the port and
//! memory accesses a real guest would, and the tests check what the platform did.

#[path = "support/fake_hv.rs"]
mod fake_hv;
#[path = "support/guest_io.rs"]
mod guest_io;

use std::io;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use fake_hv::{FakeHv, FakePartition};
use guest_io::Guest;
use machine::layout::MIB;
use machine::{
    DiskConfig, DiskInterface, FIRST_VIRTIO_SLOT, FirmwareConfig, Machine, MachineConfig,
    NVME_SLOT, RunOutcome,
};
use vm_core::StopReason;

const TIMEOUT: Duration = Duration::from_secs(10);

fn config(cpus: u8, disks: Vec<DiskConfig>) -> MachineConfig {
    MachineConfig {
        cpus,
        memory: 64 * MIB,
        disks,
        nics: Vec::new(),
        serial_output: Box::new(io::sink()),
        firmware: None,
        tpm: None,
        enlightenments: false,
    }
}

fn disk(sectors: u64) -> (tempfile::NamedTempFile, DiskConfig) {
    let file = tempfile::NamedTempFile::new().unwrap();
    file.as_file().set_len(sectors * 512).unwrap();
    let config = DiskConfig {
        path: file.path().to_owned(),
        read_only: false,
        interface: DiskInterface::Virtio,
    };
    (file, config)
}

/// Runs the machine, with a watchdog in case the guest program never stops it.
fn run(machine: &Machine<FakePartition>) -> RunOutcome {
    let stop = machine.stop_handle();
    let watchdog = thread::spawn(move || {
        if stop.wait(TIMEOUT).is_none() {
            stop.request_stop(StopReason::Requested(0xdead));
        }
    });
    let outcome = machine.run().unwrap();
    watchdog.join().unwrap();
    outcome
}

#[test]
fn pci_enumeration_through_ports_and_ecam() {
    let ids = Arc::new(Mutex::new(Vec::new()));
    let seen = ids.clone();
    let hv = FakeHv::new(move |io| {
        let g = Guest(io);
        let mut ids = seen.lock().unwrap();
        ids.push(g.config_read32_io(0, 0));
        ids.push(g.config_read32_io(FIRST_VIRTIO_SLOT, 0));
        ids.push(g.config_read32_io(NVME_SLOT, 0));
        ids.push(g.config_read32_io(NVME_SLOT, 8) >> 8);
        ids.push(g.config_read32_io(3, 0));
        ids.push(g.read32(Guest::ecam(FIRST_VIRTIO_SLOT, 0)));
        ids.push(g.config_read32_io(0x1f, 0));
        g.acpi_power_off();
    });
    let (_file, disk) = disk(8);
    let (_nvme_file, mut nvme) = self::disk(8);
    nvme.interface = DiskInterface::Nvme;
    let machine = Machine::new(&hv, config(1, vec![disk, nvme])).unwrap();

    assert_eq!(run(&machine), RunOutcome::PowerOff);
    assert_eq!(
        *ids.lock().unwrap(),
        [
            0x29c0_8086,
            0x1042_1af4,
            0x0010_1b36,
            0x01_08_02,
            0xffff_ffff,
            0x1042_1af4,
            0x2918_8086
        ],
        "Q35 host bridge, virtio-blk, NVMe and its class, empty slot,          virtio-blk through ECAM, ICH9 LPC"
    );
}

/// Guest RAM used by the virtio test.
const DESC: u64 = 0x10_0000;
const AVAIL: u64 = 0x10_1000;
const USED: u64 = 0x10_2000;
const HEADER: u64 = 0x11_0000;
const WRITE_DATA: u64 = 0x11_1000;
const WRITE_STATUS: u64 = 0x11_2000;
const READ_HEADER: u64 = 0x11_3000;
const READ_DATA: u64 = 0x11_4000;
const READ_STATUS: u64 = 0x11_5000;
const QUEUE_VECTOR: u32 = 0x45;

#[derive(Debug, Default)]
struct VirtioResult {
    features_ok: bool,
    max_queue_size: u16,
    write_status: Option<u8>,
    read_status: Option<u8>,
    read_back: Vec<u8>,
}

/// A minimal virtio-blk driver: set up queue 0, write sector 2, read it back.
fn virtio_blk_driver(g: &Guest, result: &mut VirtioResult) {
    let dev = g.probe_virtio(FIRST_VIRTIO_SLOT);
    let c = dev.common;

    // MSI-X entry 0 -> vector 0x45 on APIC 0, unmasked; then enable MSI-X.
    g.write32(dev.msix_table, 0xfee0_0000);
    g.write32(dev.msix_table + 4, 0);
    g.write32(dev.msix_table + 8, QUEUE_VECTOR);
    g.write32(dev.msix_table + 12, 0);
    let control = Guest::ecam(FIRST_VIRTIO_SLOT, dev.msix_capability + 2);
    g.write16(control, g.read16(control) | 0x8000);

    g.write8(c + 0x14, 0); // reset
    g.write8(c + 0x14, 1 | 2); // ACKNOWLEDGE | DRIVER
    g.write32(c, 1); // device_feature_select = 1
    let high_features = g.read32(c + 0x04);
    if high_features & 1 == 0 {
        return; // no VERSION_1
    }
    g.write32(c + 0x08, 0);
    g.write32(c + 0x0c, 0);
    g.write32(c + 0x08, 1);
    g.write32(c + 0x0c, 1); // VERSION_1 only
    g.write8(c + 0x14, 1 | 2 | 8);
    result.features_ok = g.read8(c + 0x14) & 8 != 0;
    if !result.features_ok {
        return;
    }

    g.write16(c + 0x16, 0);
    result.max_queue_size = g.read16(c + 0x18);
    g.write16(c + 0x18, 8);
    g.write16(c + 0x1a, 0); // MSI-X vector 0
    g.write64(c + 0x20, DESC);
    g.write64(c + 0x28, AVAIL);
    g.write64(c + 0x30, USED);
    let notify_off = u64::from(g.read16(c + 0x1e));
    g.write16(c + 0x1c, 1);
    g.write8(c + 0x14, 1 | 2 | 8 | 4); // DRIVER_OK
    let notify = dev.notify + notify_off * u64::from(dev.notify_multiplier);

    let descriptor = |index: u64, addr: u64, len: u32, flags: u16, next: u16| {
        let at = DESC + 16 * index;
        g.write64(at, addr);
        g.write32(at + 8, len);
        g.write16(at + 12, flags);
        g.write16(at + 14, next);
    };
    let header = |at: u64, request: u32, sector: u64| {
        g.write32(at, request);
        g.write32(at + 4, 0);
        g.write64(at + 8, sector);
    };
    const NEXT: u16 = 1;
    const WRITE: u16 = 2;

    // Write 512 bytes of 0xab to sector 2.
    header(HEADER, 1, 2);
    g.write_bytes(WRITE_DATA, &[0xab; 512]);
    g.write8(WRITE_STATUS, 0xff);
    descriptor(0, HEADER, 16, NEXT, 1);
    descriptor(1, WRITE_DATA, 512, NEXT, 2);
    descriptor(2, WRITE_STATUS, 1, WRITE, 0);
    g.write16(AVAIL + 4, 0);
    g.write16(AVAIL + 2, 1);
    g.write16(notify, 0);
    if !g.wait_until(TIMEOUT, || g.read16(USED + 2) == 1) {
        return;
    }
    result.write_status = Some(g.read8(WRITE_STATUS));

    // Read sector 2 back.
    header(READ_HEADER, 0, 2);
    g.write8(READ_STATUS, 0xff);
    descriptor(3, READ_HEADER, 16, NEXT, 4);
    descriptor(4, READ_DATA, 512, NEXT | WRITE, 5);
    descriptor(5, READ_STATUS, 1, WRITE, 0);
    g.write16(AVAIL + 6, 3);
    g.write16(AVAIL + 2, 2);
    g.write16(notify, 0);
    if !g.wait_until(TIMEOUT, || g.read16(USED + 2) == 2) {
        return;
    }
    result.read_status = Some(g.read8(READ_STATUS));
    result.read_back = g.read_bytes(READ_DATA, 512);
}

#[test]
fn virtio_blk_round_trip_through_the_whole_platform() {
    let result = Arc::new(Mutex::new(VirtioResult::default()));
    let out = result.clone();
    let hv = FakeHv::new(move |io| {
        let g = Guest(io);
        virtio_blk_driver(&g, &mut out.lock().unwrap());
        g.acpi_power_off();
    });
    let (file, disk) = disk(16);
    let machine = Machine::new(&hv, config(1, vec![disk])).unwrap();

    assert_eq!(run(&machine), RunOutcome::PowerOff);
    let result = result.lock().unwrap();
    assert!(result.features_ok, "{result:?}");
    assert_eq!(result.max_queue_size, 256);
    assert_eq!(result.write_status, Some(0), "write request");
    assert_eq!(result.read_status, Some(0), "read request");
    assert_eq!(result.read_back, [0xab; 512]);

    let on_disk = std::fs::read(file.path()).unwrap();
    assert_eq!(
        &on_disk[1024..1536],
        &[0xab; 512][..],
        "sector 2 in the image"
    );
    assert!(on_disk[..1024].iter().all(|&b| b == 0));

    let interrupts = &hv.recorded.lock().unwrap().interrupts;
    assert!(
        interrupts
            .iter()
            .any(|i| u32::from(i.vector) == QUEUE_VECTOR && i.destination == 0),
        "queue interrupt delivered via MSI-X: {interrupts:?}"
    );
}

#[test]
fn com1_input_interrupts_through_the_ioapic() {
    let (ready_tx, ready_rx) = mpsc::channel();
    let received = Arc::new(Mutex::new(None));
    let out = received.clone();
    let hv = FakeHv::new(move |io| {
        let g = Guest(io);
        // IOAPIC pin 4 -> vector 0x24, edge, unmasked, APIC 0.
        g.write32(0xfec0_0000, 0x10 + 2 * 4 + 1);
        g.write32(0xfec0_0010, 0);
        g.write32(0xfec0_0000, 0x10 + 2 * 4);
        g.write32(0xfec0_0010, 0x24);
        g.outb(0x3f9, 1); // UART: interrupt on received data
        ready_tx.send(()).unwrap();
        if g.wait_until(TIMEOUT, || g.inb(0x3fd) & 1 != 0) {
            *out.lock().unwrap() = Some(g.inb(0x3f8));
        }
        g.acpi_power_off();
    });
    let machine = Machine::new(&hv, config(1, Vec::new())).unwrap();

    let outcome = thread::scope(|scope| {
        let running = scope.spawn(|| run(&machine));
        ready_rx.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(machine.serial_input().send(b"z"), 1);
        running.join().unwrap()
    });
    assert_eq!(outcome, RunOutcome::PowerOff);
    assert_eq!(*received.lock().unwrap(), Some(b'z'));
    let interrupts = &hv.recorded.lock().unwrap().interrupts;
    assert!(
        interrupts.iter().any(|i| i.vector == 0x24),
        "COM1 interrupt: {interrupts:?}"
    );
}

#[test]
fn keyboard_controller_and_cf9_reset_the_machine() {
    for program in [
        |g: &Guest| g.outb(0x64, 0xfe),
        |g: &Guest| g.outb(0xcf9, 0x06),
    ] {
        let hv = FakeHv::new(move |io| program(&Guest(io)));
        let machine = Machine::new(&hv, config(1, Vec::new())).unwrap();
        assert_eq!(run(&machine), RunOutcome::Reset);
    }
}

/// A minimal bzImage the loader accepts.
fn fake_bzimage() -> Vec<u8> {
    let mut image = vec![0u8; 5 * 512];
    let mut put = |offset: usize, bytes: &[u8]| {
        image[offset..offset + bytes.len()].copy_from_slice(bytes);
    };
    put(0x1f1, &[4]);
    put(0x1fe, &0xaa55u16.to_le_bytes());
    put(0x202, b"HdrS");
    put(0x206, &0x020fu16.to_le_bytes());
    put(0x211, &[1]);
    put(0x214, &0x10_0000u32.to_le_bytes());
    put(0x22c, &0x7fff_ffffu32.to_le_bytes());
    put(0x236, &3u16.to_le_bytes());
    put(0x238, &2047u32.to_le_bytes());
    put(0x260, &0x40_0000u32.to_le_bytes());
    image.extend_from_slice(b"kernel");
    image
}

#[test]
fn linux_boot_programs_the_boot_cpu_and_parks_the_others() {
    let hv = FakeHv::new(|io| Guest(io).acpi_power_off());
    let mut machine = Machine::new(&hv, config(3, Vec::new())).unwrap();
    machine
        .load_linux(&fake_bzimage(), Some(b"initrd"), "console=ttyS0")
        .unwrap();

    assert_eq!(run(&machine), RunOutcome::PowerOff);
    let recorded = hv.recorded.lock().unwrap();
    let regs = recorded.boot_regs.expect("boot CPU registers set");
    let sregs = recorded.boot_sregs.expect("boot CPU segments set");
    assert_eq!((regs.rip, regs.rsi), (0x10_0200, 0x7000));
    assert_eq!(sregs.efer & 0x500, 0x500);
    let mut waiting = recorded.waiting_for_sipi.clone();
    waiting.sort();
    assert_eq!(waiting, [1, 2]);
}

#[test]
fn host_stop_request_ends_the_run() {
    let hv = FakeHv::new(|_| {});
    let machine = Machine::new(&hv, config(2, Vec::new())).unwrap();
    let stop = machine.stop_handle();
    let outcome = thread::scope(|scope| {
        let running = scope.spawn(|| machine.run().unwrap());
        thread::sleep(Duration::from_millis(50));
        stop.request_stop(StopReason::Requested(7));
        running.join().unwrap()
    });
    assert_eq!(outcome, RunOutcome::Requested(7));
}

#[test]
fn rejects_bad_configurations() {
    let hv = FakeHv::new(|_| {});
    let error = |config| Machine::new(&hv, config).err().unwrap().to_string();
    assert!(error(config(0, Vec::new())).contains("vCPUs"));
    let mut small = config(1, Vec::new());
    small.memory = 16 * MIB;
    assert!(error(small).contains("memory"));
    let mut missing_disk = config(1, Vec::new());
    missing_disk.disks.push(DiskConfig {
        path: "does-not-exist.img".into(),
        read_only: false,
        interface: DiskInterface::Nvme,
    });
    assert!(error(missing_disk).contains("does-not-exist.img"));
}

/// 64 KiB of firmware code whose last 16 bytes (the reset vector) are recognizable.
fn fake_firmware(vars: &std::path::Path) -> FirmwareConfig {
    let mut code = vec![0x90u8; 0x1_0000];
    code[0xfff0..].copy_from_slice(b"VMMC reset vec!!");
    FirmwareConfig {
        code,
        vars: vars.to_owned(),
        debug_log: None,
        uuid: [7; 16],
    }
}

#[derive(Debug, Default)]
struct FirmwareView {
    signature: Vec<u8>,
    cpus: Vec<u8>,
    e820: Vec<u8>,
    loader_len: usize,
    smbios_anchor: Vec<u8>,
    cmos_34: [u8; 2],
    reset_vector: Vec<u8>,
    vars_start: u32,
    pm_timer_moves: bool,
}

/// What OVMF does first: reads the platform through fw_cfg and CMOS, finds its flash,
/// writes a variable, sets up the ACPI PM block through the LPC bridge, powers off.
fn firmware_probe(g: &Guest, view: &mut FirmwareView) {
    view.signature = g.fw_cfg_read(0, 4);
    view.cpus = g.fw_cfg_read(0x05, 2);
    if let Some((key, len)) = g.fw_cfg_file("etc/e820") {
        view.e820 = g.fw_cfg_read(key, len);
    }
    view.loader_len = g.fw_cfg_file("etc/table-loader").map_or(0, |(_, len)| len);
    if let Some((key, len)) = g.fw_cfg_file("etc/smbios/smbios-anchor") {
        view.smbios_anchor = g.fw_cfg_read(key, len);
    }
    view.cmos_34 = [g.cmos(0x34), g.cmos(0x35)];
    view.reset_vector = (0..16).map(|i| g.read8(0xffff_fff0 + i)).collect();

    // The 8 KiB variable store sits right below the 64 KiB of code. Program a byte.
    let vars = 0x1_0000_0000 - 0x1_0000 - 0x2000;
    view.vars_start = g.read32(vars);
    g.write8(vars + 0x10, 0x40);
    g.write8(vars + 0x10, 0xa5);
    g.write8(vars, 0xff);

    // PMBASE = 0x600, then ACPI_EN.
    g.config_write32_io(0x1f, 0, 0x40, 0x600);
    g.config_write32_io(0x1f, 0, 0x44, 0x80);
    let first = g.inl(0x608);
    view.pm_timer_moves = g.wait_until(TIMEOUT, || g.inl(0x608) != first);
    g.acpi_power_off();
}

#[test]
fn firmware_boot_presents_fw_cfg_flash_cmos_and_the_lpc() {
    let vars = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(vars.path(), vec![0xffu8; 0x2000]).unwrap();
    let view = Arc::new(Mutex::new(FirmwareView::default()));
    let seen = view.clone();
    let hv = FakeHv::new(move |io| firmware_probe(&Guest(io), &mut seen.lock().unwrap()));
    let mut config = config(2, Vec::new());
    config.firmware = Some(fake_firmware(vars.path()));
    let mut machine = Machine::new(&hv, config).unwrap();
    assert!(
        machine.load_linux(&fake_bzimage(), None, "").is_err(),
        "no direct boot alongside firmware"
    );

    assert_eq!(run(&machine), RunOutcome::PowerOff);
    let view = view.lock().unwrap();
    assert_eq!(view.signature, b"QEMU");
    assert_eq!(view.cpus, [2, 0]);
    // One RAM entry: 0 to 64 MiB, type 1.
    assert_eq!(view.e820.len(), 20);
    assert_eq!(&view.e820[8..16], &(64 * MIB).to_le_bytes());
    assert!(view.loader_len > 0 && view.loader_len % 128 == 0);
    assert_eq!(&view.smbios_anchor[..5], b"_SM3_");
    // (64 MiB - 16 MiB) / 64 KiB = 768.
    assert_eq!(view.cmos_34, [0x00, 0x03]);
    assert_eq!(view.reset_vector, b"VMMC reset vec!!");
    assert_eq!(view.vars_start, 0xffff_ffff);
    assert!(view.pm_timer_moves, "PM timer decodes at 0x608");
    drop(machine);
    assert_eq!(
        std::fs::read(vars.path()).unwrap()[0x10],
        0xa5,
        "variable persisted"
    );
}

#[test]
fn firmware_flash_must_be_whole_blocks() {
    let vars = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(vars.path(), vec![0xffu8; 0x1800]).unwrap();
    let hv = FakeHv::new(|_| {});
    let mut config = config(1, Vec::new());
    config.firmware = Some(fake_firmware(vars.path()));
    let error = Machine::new(&hv, config).err().unwrap().to_string();
    assert!(error.contains("variable store"), "{error}");
}

/// Loads the real test kernel (from `cargo xtask fetch-guests`) when it's there.
#[test]
fn real_test_kernel_and_initramfs_load() {
    let guests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/guests");
    let (Ok(kernel), Ok(initramfs)) = (
        std::fs::read(guests.join("vmlinuz")),
        std::fs::read(guests.join("initramfs.cpio")),
    ) else {
        eprintln!("skipping: run `cargo xtask fetch-guests` first");
        return;
    };
    let hv = FakeHv::new(|io| Guest(io).acpi_power_off());
    let mut config = config(4, Vec::new());
    config.memory = 1024 * MIB;
    let mut machine = Machine::new(&hv, config).unwrap();
    machine
        .load_linux(&kernel, Some(&initramfs), "console=ttyS0 panic=-1")
        .unwrap();
    assert_eq!(run(&machine), RunOutcome::PowerOff);
    let regs = hv.recorded.lock().unwrap().boot_regs.unwrap();
    assert_eq!(regs.rip, 0x10_0200);
}
