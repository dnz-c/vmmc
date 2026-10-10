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
use machine::{DiskConfig, Machine, MachineConfig, RunOutcome};
use vm_core::StopReason;

const TIMEOUT: Duration = Duration::from_secs(10);

fn config(cpus: u8, disks: Vec<DiskConfig>) -> MachineConfig {
    MachineConfig {
        cpus,
        memory: 64 * MIB,
        disks,
        serial_output: Box::new(io::sink()),
    }
}

fn disk(sectors: u64) -> (tempfile::NamedTempFile, DiskConfig) {
    let file = tempfile::NamedTempFile::new().unwrap();
    file.as_file().set_len(sectors * 512).unwrap();
    let config = DiskConfig {
        path: file.path().to_owned(),
        read_only: false,
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
        ids.push(g.config_read32_io(1, 0));
        ids.push(g.config_read32_io(2, 0));
        ids.push(g.read32(Guest::ecam(1, 0)));
        g.acpi_power_off();
    });
    let (_file, disk) = disk(8);
    let machine = Machine::new(&hv, config(1, vec![disk])).unwrap();

    assert_eq!(run(&machine), RunOutcome::PowerOff);
    assert_eq!(
        *ids.lock().unwrap(),
        [0x29c0_8086, 0x1042_1af4, 0xffff_ffff, 0x1042_1af4],
        "host bridge, virtio-blk, empty slot, virtio-blk through ECAM"
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
    let dev = g.probe_virtio(1);
    let c = dev.common;

    // MSI-X entry 0 -> vector 0x45 on APIC 0, unmasked; then enable MSI-X.
    g.write32(dev.msix_table, 0xfee0_0000);
    g.write32(dev.msix_table + 4, 0);
    g.write32(dev.msix_table + 8, QUEUE_VECTOR);
    g.write32(dev.msix_table + 12, 0);
    let control = Guest::ecam(1, dev.msix_capability + 2);
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
    });
    assert!(error(missing_disk).contains("does-not-exist.img"));
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
