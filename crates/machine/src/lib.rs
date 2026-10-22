//! The vmmc x86 PC platform: memory layout, ACPI and SMBIOS tables, firmware and Linux
//! boot, and vCPU threads.
//!
//! [`Machine::new`] builds the VM and its devices. A machine either boots UEFI firmware
//! ([`MachineConfig::firmware`]) or a kernel prepared with [`Machine::load_linux`].
//! [`Machine::run`] runs one thread per vCPU until the guest powers off, resets, or
//! fails. [`Machine::snapshotter`] saves a running machine's whole state, and
//! [`Machine::restore`] starts a new one from it (see [`snapshot`]).

pub mod acpi;
pub mod boot;
pub mod layout;
mod msi;
pub mod smbios;
pub mod snapshot;

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use devices::chipset::lpc::{Ich9Lpc, LPC_FUNCTION, LPC_SLOT};
use devices::chipset::mch::{MCH_SLOT, Mch};
use devices::clock::{Clock, VmClock};
use devices::display::bochs::{
    BochsDisplay, FRAMEBUFFER_BAR, FRAMEBUFFER_SIZE, MMIO_BAR, MMIO_SIZE,
};
pub use devices::display::bochs::{Capture, Cursor, Display, Frame, MAX_PREFERRED, MIN_PREFERRED};
use devices::firmware::fw_cfg::{self, FW_CFG_BASE, FW_CFG_PORTS, FwCfg};
use devices::firmware::pflash::Pflash;
use devices::irq::{IrqLine, MsiSink};
use devices::legacy::acpi_pm::{AcpiPm, PM_BASE, PM_PORTS};
use devices::legacy::debugcon::{DebugConsole, OVMF_DEBUG_PORT};
pub use devices::legacy::i8042::PAUSE;
use devices::legacy::i8042::{I8042, I8042_COMMAND, I8042_DATA, I8042CommandPort};
use devices::legacy::ioapic::{IOAPIC_BASE, IOAPIC_SIZE, IoApic};
use devices::legacy::pic::{
    ELCR_BASE, ELCR_PORTS, Elcr, MASTER_BASE, PIC_PORTS, Pic8259, SLAVE_BASE,
};
use devices::legacy::pit::{PIT_BASE, PIT_PORTS, PORT_61, Pit, Port61};
use devices::legacy::port80::{POST_PORT, PostPort};
use devices::legacy::rtc::{RTC_BASE, RTC_IRQ, RTC_PORTS, Rtc, RtcInterrupt};
use devices::legacy::serial::{COM1_BASE, COM1_IRQ, COM2_BASE, COM2_IRQ, SERIAL_PORTS, Serial};
use devices::nic::e1000e::{
    E1000e, REGISTER_BAR as E1000E_BAR, REGISTER_BAR_SIZE as E1000E_BAR_SIZE,
};
use devices::pci::root::{CONFIG_IO_BASE, CONFIG_IO_PORTS, PciConfigIo, PciEcam, PciRoot};
use devices::pci::{PciDevice, SharedPciDevice};
use devices::snapshot::Snapshot;
pub use devices::storage::ahci::DiscDrive;
use devices::storage::ahci::{
    ABAR, ABAR_SIZE, AHCI_FUNCTION, AHCI_SLOT, Ahci, Drive, PORTS as AHCI_PORTS,
};
use devices::storage::ata::AtaDisk;
use devices::storage::atapi::Cdrom;
use devices::storage::nvme::{NVME_BAR, NVME_BAR_SIZE, Namespace, Nvme};
use devices::timer::ThreadTicker;
use devices::tpm::{Crb, TPM_BASE, TPM_SIZE};
pub use devices::tpm::{SharedTpm, TpmBackend};
pub use devices::usb::hid::TABLET_MAX;
use devices::usb::hid::{Keyboard as UsbKeyboard, KeyboardInput, Tablet, TabletInput};
use devices::usb::xhci::{XHCI_BAR, XHCI_BAR_SIZE, Xhci};
use devices::virtio::blk::VirtioBlk;
use devices::virtio::net::VirtioNet;
use devices::virtio::pci::{CONFIG_BAR, CONFIG_BAR_SIZE, MSIX_BAR, MSIX_BAR_SIZE, VirtioPci};
use devices::{PowerEvent, PowerHandler};
use hv::{Hypervisor, LocalApicMode, Partition, PartitionConfig};
pub use net::{LocalService, LocalStream};
use net::{Nat, NatConfig};
use vm_core::{BusDevice, StopHandle, StopReason, Vm, VmExit, VmVcpu};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

pub use crate::boot::BootEntry;
use crate::layout::{
    BIOS_AREA_START, ECAM_BASE, ECAM_SIZE, FLASH_MAX, GIB, HIGH_MEMORY_START, MIB, MIN_MEMORY,
    MemoryLayout, Mmio32Allocator, RAM_ABOVE_4G_START, mmio32_window,
};
pub use crate::msi::HvMsiSink;
use crate::snapshot::{Pause, SavedDevice};
pub use crate::snapshot::{STATE_FORMAT, Snapshotter};

/// Most vCPUs a machine can have (xAPIC physical destination IDs).
pub const MAX_CPUS: u8 = 32;
/// The PIT's IRQ 0 reaches IOAPIC pin 2 (see the MADT override).
const PIT_IOAPIC_PIN: u8 = 2;
const KEYBOARD_IRQ: u8 = 1;
/// The PS/2 mouse's ISA interrupt.
pub const MOUSE_IRQ: u8 = 12;
/// PCI slot of the display adapter.
pub const DISPLAY_SLOT: u8 = 1;
/// PCI slot of the NVMe controller.
pub const NVME_SLOT: u8 = 2;
/// PCI slot of the e1000e network adapter.
pub const E1000E_SLOT: u8 = 3;
/// PCI slot of the xHCI USB controller (with the tablet and keyboard).
pub const XHCI_SLOT: u8 = 4;
/// PCI slot of the first virtio disk. Slots 1 to 4 are for the display, NVMe, network
/// and USB controllers.
pub const FIRST_VIRTIO_SLOT: u8 = 5;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Vm(#[from] vm_core::Error),
    #[error("guest memory: {0}")]
    GuestMemory(#[from] vm_memory::GuestMemoryError),
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Boot(String),
    #[error("disk {path}: {source}")]
    Disk { path: PathBuf, source: io::Error },
    #[error("firmware variable store {path}: {source}")]
    FirmwareVars { path: PathBuf, source: io::Error },
    #[error("firmware flash: {0}")]
    Flash(io::Error),
    #[error("display memory: {0}")]
    Display(io::Error),
    #[error("can't start a thread: {0}")]
    Thread(io::Error),
    #[error("saved state: {0}")]
    State(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// How a disk is attached.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiskInterface {
    /// Its own virtio-blk device (slots 5 and up). Linux has the driver built in;
    /// Windows needs one installed.
    #[default]
    Virtio,
    /// A namespace of the machine's NVMe controller (slot 2). Every OS and the firmware
    /// have the driver.
    Nvme,
    /// An ATA disk on a port of the AHCI controller (00:1f.2).
    Sata,
    /// A DVD-ROM drive on a port of the AHCI controller, with this image (ISO 9660 or
    /// UDF, 2048-byte sectors) as its disc. Always read-only.
    Cdrom,
}

pub struct DiskConfig {
    /// A raw disk image.
    pub path: PathBuf,
    pub read_only: bool,
    pub interface: DiskInterface,
}

/// UEFI firmware to boot (OVMF built for the Q35 platform).
pub struct FirmwareConfig {
    /// The firmware code volume (`OVMF_CODE.fd`).
    pub code: Vec<u8>,
    /// The variable store: this VM's copy of `OVMF_VARS.fd`. It's opened read-write,
    /// and the guest's variable writes go straight back to it.
    pub vars: PathBuf,
    /// Where the firmware's debug log (port 0x402) goes. Without it the firmware sees
    /// no debug port and stays quiet.
    pub debug_log: Option<Box<dyn Write + Send>>,
    /// The SMBIOS system UUID, in RFC 4122 byte order. It should stay the same for the
    /// life of the VM.
    pub uuid: [u8; 16],
}

/// Which network adapter the guest sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NicModel {
    /// virtio-net (slots after the virtio disks). Linux has the driver built in.
    Virtio,
    /// An Intel 82574L (slot 3). Windows and Linux have drivers for it.
    E1000e,
}

/// A network adapter, connected to its own user-mode NAT (10.0.2.0/24).
#[derive(Clone, Debug)]
pub struct NicConfig {
    pub model: NicModel,
    pub mac: [u8; 6],
}

pub struct MachineConfig {
    pub cpus: u8,
    /// RAM in bytes; at least 32 MiB, a multiple of 1 MiB.
    pub memory: u64,
    /// Disks, attached as virtio-blk devices in order.
    pub disks: Vec<DiskConfig>,
    pub nics: Vec<NicConfig>,
    /// Where the guest's serial console output goes.
    pub serial_output: Box<dyn Write + Send>,
    /// Where the guest's second serial port (COM2) writes: vmmc's agent in the guest
    /// talks to the host there.
    pub agent_output: Box<dyn Write + Send>,
    /// A port on each network adapter's gateway that the host answers in-process: where
    /// vmmc's agent sends files.
    pub nat_service: Option<net::LocalService>,
    /// Boot this firmware. Without it, load a kernel with [`Machine::load_linux`].
    pub firmware: Option<FirmwareConfig>,
    /// A TPM 2.0, behind a CRB. It outlives the machine: each new machine resets it,
    /// as a platform reset would.
    pub tpm: Option<SharedTpm>,
    /// Expose the hypervisor's Hyper-V enlightenments to the guest.
    pub enlightenments: bool,
    /// The screen resolution the display's EDID prefers, which the firmware boots in.
    pub resolution: (u32, u32),
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    PowerOff,
    Reset,
    TripleFault,
    /// The host asked to stop, with this code.
    Requested(u32),
    Error(String),
}

/// Host keyboard and mouse input for the guest's PS/2 devices, from any thread.
#[derive(Clone)]
pub struct Ps2Input(Arc<Mutex<I8042>>);

impl Ps2Input {
    /// A key went down or up: its scan code in set 1, `0xE0nn` for extended keys,
    /// [`PAUSE`] for Pause.
    pub fn key(&self, scancode: u16, pressed: bool) {
        lock(&self.0).key(scancode, pressed);
    }

    /// The pointer moved (right and down positive) and the wheel turned (towards the
    /// user positive), with these buttons held: bit 0 left, 1 right, 2 middle.
    pub fn mouse_motion(&self, dx: i32, dy: i32, dz: i32, buttons: u8) {
        lock(&self.0).mouse_motion(dx, dy, dz, buttons);
    }
}

/// Host input for the guest's USB tablet and keyboard, from any thread.
#[derive(Clone)]
pub struct UsbInput {
    xhci: Arc<Mutex<Xhci>>,
    tablet: TabletInput,
    keyboard: KeyboardInput,
}

impl UsbInput {
    /// The pointer is at `(x, y)`, each 0 to [`TABLET_MAX`] across the screen, with
    /// these buttons held (bit 0 left, 1 right, 2 middle), and the wheel turned by
    /// `wheel` notches (away from the user positive).
    pub fn pointer(&self, x: u16, y: u16, buttons: u8, wheel: i32) {
        self.tablet.update(x, y, buttons, wheel);
        lock(&self.xhci).service();
    }

    /// A key went down or up: its scan code in set 1, `0xE0nn` for extended keys.
    pub fn key(&self, scancode: u16, pressed: bool) {
        self.keyboard.key(scancode, pressed);
        lock(&self.xhci).service();
    }
}

/// Feeds host input to the guest's serial console.
#[derive(Clone)]
pub struct SerialInput(Arc<Mutex<Serial>>);

impl SerialInput {
    /// Queues `bytes`; returns how many fit. Offer the rest again later.
    pub fn send(&self, bytes: &[u8]) -> usize {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .enqueue_input(bytes)
    }
}

/// Presses the guest's ACPI power button. A guest with ACPI support shuts down cleanly.
#[derive(Clone)]
pub struct PowerButton(Arc<Mutex<AcpiPm>>);

impl PowerButton {
    pub fn press(&self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .press_power_button();
    }
}

pub struct Machine<P: Partition> {
    vm: Vm<P>,
    layout: MemoryLayout,
    cpus: u8,
    /// Where the ACPI tables were placed for direct boot; `None` with firmware, which
    /// installs them itself.
    rsdp: Option<u64>,
    serial: Arc<Mutex<Serial>>,
    com2: Arc<Mutex<Serial>>,
    acpi_pm: Arc<Mutex<AcpiPm>>,
    display: Display,
    ps2: Ps2Input,
    usb: UsbInput,
    disc_drives: Vec<DiscDrive>,
    /// The writable disks, in the configuration's order, whose images can switch.
    disks: Vec<Arc<disk::Switchable>>,
    boot: Option<BootEntry>,
    /// What went wrong, if a vCPU stopped the machine with an error.
    error: Arc<Mutex<Option<String>>>,
    /// The devices' clock, which stands still while the machine is paused.
    clock: Arc<VmClock>,
    /// What a snapshot saves, by name.
    saved_devices: Arc<Vec<SavedDevice>>,
    pause: Arc<Pause>,
    snapshotting: Arc<Mutex<()>>,
    /// The vCPUs' states, after a restore: the run starts them from these.
    restored: Mutex<Option<Vec<hv::VcpuState>>>,
}

/// `device`, as a snapshot saves it.
fn saved<T: Snapshot + 'static>(name: &'static str, device: &Arc<Mutex<T>>) -> SavedDevice {
    (name, device.clone() as Arc<Mutex<dyn Snapshot>>)
}

fn shared<T: BusDevice + 'static>(device: T) -> Arc<Mutex<T>> {
    Arc::new(Mutex::new(device))
}

impl<P: Partition + 'static> Machine<P> {
    pub fn new<H>(hypervisor: &H, mut config: MachineConfig) -> Result<Self>
    where
        H: Hypervisor<Partition = P>,
    {
        if !(1..=MAX_CPUS).contains(&config.cpus) {
            return Err(Error::Config(format!("between 1 and {MAX_CPUS} vCPUs")));
        }
        if config.memory < MIN_MEMORY || !config.memory.is_multiple_of(MIB) {
            return Err(Error::Config(format!(
                "memory must be at least {} MiB, in whole MiB",
                MIN_MEMORY / MIB
            )));
        }
        let firmware = config.firmware.take();
        let direct_boot = firmware.is_none();
        let layout = if direct_boot {
            MemoryLayout::new(config.memory)
        } else {
            MemoryLayout::firmware(config.memory)
        };
        let mut vm = Vm::new(
            hypervisor,
            &PartitionConfig {
                vcpu_count: config.cpus.into(),
                local_apic: LocalApicMode::XApic,
                enlightenments: config.enlightenments,
            },
            &layout.ram,
        )?;

        let vm_clock = Arc::new(VmClock::new());
        let clock: Arc<dyn Clock> = vm_clock.clone();
        let mut saved_devices: Vec<SavedDevice> = Vec::new();
        let sink: Arc<dyn MsiSink> = Arc::new(HvMsiSink::new(vm.interrupts()));
        let stop = vm.stop_handle();
        let power: PowerHandler = Arc::new(move |event| {
            stop.request_stop(match event {
                PowerEvent::PowerOff => StopReason::PowerOff,
                PowerEvent::Reset => StopReason::Reset,
            });
        });

        let ioapic = shared(IoApic::new(acpi::IOAPIC_ID, sink.clone()));
        vm.mmio().insert(IOAPIC_BASE, IOAPIC_SIZE, ioapic.clone())?;
        saved_devices.push(saved("ioapic", &ioapic));
        let tpm = config.tpm.take();
        let mut crb = None;
        if let Some(tpm) = &tpm {
            tpm.lock().unwrap_or_else(PoisonError::into_inner).reset();
            let device = shared(Crb::new(tpm.clone()));
            vm.mmio().insert(TPM_BASE, TPM_SIZE, device.clone())?;
            crb = Some(device);
        }
        vm.set_eoi_handler({
            let ioapic = ioapic.clone();
            move |vector| {
                ioapic
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .end_of_interrupt(vector);
            }
        });
        let irq = |pin| IrqLine::new(ioapic.clone(), pin);
        let pio = vm.pio().clone();

        let irq0 = irq(PIT_IOAPIC_PIN);
        let ticker = ThreadTicker::new("pit-irq0", move || irq0.pulse()).map_err(Error::Thread)?;
        let pit = shared(Pit::new(clock.clone(), Box::new(ticker)));
        pio.insert(PIT_BASE.into(), PIT_PORTS.into(), pit.clone())?;
        pio.insert(PORT_61.into(), 1, shared(Port61(pit.clone())))?;

        let master = shared(Pic8259::default());
        pio.insert(MASTER_BASE.into(), PIC_PORTS.into(), master.clone())?;
        let slave = shared(Pic8259::default());
        pio.insert(SLAVE_BASE.into(), PIC_PORTS.into(), slave.clone())?;
        let elcr = shared(Elcr::default());
        pio.insert(ELCR_BASE.into(), ELCR_PORTS.into(), elcr.clone())?;
        saved_devices.extend([
            saved("pic-master", &master),
            saved("pic-slave", &slave),
            saved("elcr", &elcr),
            saved("pit", &pit),
        ]);
        let rtc_interrupt = RtcInterrupt::new(irq(RTC_IRQ));
        let rtc_ticker = ThreadTicker::new("rtc-irq8", {
            let interrupt = rtc_interrupt.clone();
            move || interrupt.periodic_tick()
        })
        .map_err(Error::Thread)?;
        let rtc = shared(Rtc::new(clock.clone(), rtc_interrupt, Box::new(rtc_ticker)));
        pio.insert(RTC_BASE.into(), RTC_PORTS.into(), rtc.clone())?;

        // The ACPI PM block decodes wherever the LPC bridge puts it.
        let acpi_pm = shared(AcpiPm::new(clock, irq(acpi::SCI_IRQ), power.clone()));

        let i8042 = shared(I8042::new(irq(KEYBOARD_IRQ), irq(MOUSE_IRQ), power.clone()));
        let ps2 = Ps2Input(i8042.clone());
        pio.insert(I8042_DATA.into(), 1, i8042.clone())?;
        pio.insert(
            I8042_COMMAND.into(),
            1,
            shared(I8042CommandPort(i8042.clone())),
        )?;

        let serial = shared(Serial::new(irq(COM1_IRQ), config.serial_output));
        pio.insert(COM1_BASE.into(), SERIAL_PORTS.into(), serial.clone())?;
        let com2 = shared(Serial::new(irq(COM2_IRQ), config.agent_output));
        pio.insert(COM2_BASE.into(), SERIAL_PORTS.into(), com2.clone())?;
        let post = shared(PostPort::default());
        pio.insert(POST_PORT.into(), 1, post.clone())?;

        let root = Arc::new(Mutex::new(PciRoot::new(vm.mmio().clone())));
        let display;
        let usb;
        let disc_drives;
        let mut switchable_disks;
        let config_io = shared(PciConfigIo::new(root.clone(), power));
        pio.insert(
            CONFIG_IO_BASE.into(),
            CONFIG_IO_PORTS.into(),
            config_io.clone(),
        )?;
        saved_devices.extend([
            saved("rtc", &rtc),
            saved("acpi-pm", &acpi_pm),
            saved("i8042", &i8042),
            saved("serial0", &serial),
            saved("serial1", &com2),
            saved("post", &post),
            saved("pci-config", &config_io),
        ]);
        // ECAM starts where firmware would put it; firmware may move it, and the PM
        // block too.
        let ecam = shared(PciEcam(Arc::downgrade(&root)));
        let mch = Arc::new(Mutex::new(Mch::new(vm.mmio(), ecam, Some(ECAM_BASE))));
        let lpc = Arc::new(Mutex::new(Ich9Lpc::new(
            &pio,
            acpi_pm.clone(),
            PM_PORTS,
            direct_boot.then_some(PM_BASE),
        )));
        {
            let mut root = root.lock().unwrap();
            root.add_device(MCH_SLOT, mch.clone());
            root.add_function(LPC_SLOT, LPC_FUNCTION, lpc.clone());
            let mut mmio32 = Mmio32Allocator::new(mmio32_window(layout.low_ram_end, ECAM_BASE));
            let mut allocate = |size| {
                mmio32
                    .allocate(size)
                    .ok_or_else(|| Error::Config("out of PCI address space".into()))
            };

            let mut adapter =
                BochsDisplay::new(vm.ram_mapper(), config.resolution).map_err(Error::Display)?;
            display = adapter.display();
            if direct_boot {
                for (bar, size) in [(FRAMEBUFFER_BAR, FRAMEBUFFER_SIZE), (MMIO_BAR, MMIO_SIZE)] {
                    let address = allocate(size)?;
                    adapter.config_mut().set_bar_address(bar.into(), address);
                }
            }
            root.add_device(DISPLAY_SLOT, Arc::new(Mutex::new(adapter)));

            let mut namespaces = Vec::new();
            let mut virtio_disks = Vec::new();
            let mut first_nvme_id = None;
            switchable_disks = Vec::new();
            let mut sata_drives = Vec::new();
            for disk in &config.disks {
                let read_only = disk.read_only || disk.interface == DiskInterface::Cdrom;
                // An empty DVD drive has no path.
                let empty =
                    disk.interface == DiskInterface::Cdrom && disk.path.as_os_str().is_empty();
                let backend = if empty {
                    None
                } else {
                    Some(
                        disk::open(&disk.path, read_only).map_err(|source| Error::Disk {
                            path: disk.path.clone(),
                            source,
                        })?,
                    )
                };
                // Named by the image the disk started as, which snapshots keep the
                // same: the guest sees the same disk whatever delta it writes to now.
                let base = backend
                    .as_ref()
                    .and_then(|backend| backend.base_image())
                    .unwrap_or_else(|| disk.path.clone());
                let id = disk_id(&base);
                if disk.interface == DiskInterface::Nvme && first_nvme_id.is_none() {
                    first_nvme_id = Some(id);
                }
                // A writable disk can switch to a new delta, for a snapshot.
                let backend = backend.map(|backend| {
                    if read_only {
                        return backend;
                    }
                    let switchable = disk::Switchable::new(&disk.path, backend);
                    switchable_disks.push(switchable.clone());
                    switchable as Arc<dyn disk::BlockBackend>
                });
                tracing::debug!(
                    path = %disk.path.display(),
                    read_only,
                    backend_read_only = backend.as_ref().map(|backend| backend.is_read_only()),
                    "disk"
                );
                let serial: String = id[..6].iter().map(|b| format!("{b:02X}")).collect();
                let Some(backend) = backend else {
                    sata_drives.push(Drive::Cdrom(Cdrom::new(None, &format!("VMMC{serial}"))));
                    continue;
                };
                match disk.interface {
                    DiskInterface::Nvme => namespaces.push(Namespace { backend, nguid: id }),
                    DiskInterface::Virtio => virtio_disks.push(backend),
                    DiskInterface::Sata => sata_drives.push(Drive::Disk(AtaDisk {
                        backend,
                        serial: format!("VMMC{serial}"),
                    })),
                    DiskInterface::Cdrom => sata_drives.push(Drive::Cdrom(Cdrom::new(
                        Some(backend),
                        &format!("VMMC{serial}"),
                    ))),
                }
            }
            if sata_drives.len() > AHCI_PORTS {
                return Err(Error::Config(format!(
                    "at most {AHCI_PORTS} SATA disks and CD-ROMs"
                )));
            }
            // The ICH9 AHCI controller is always there, as on a real Q35 board.
            let intx = irq(acpi::intx_gsi(AHCI_SLOT, 0) as u8);
            let mut ahci = Ahci::new(sata_drives, vm.memory().clone(), sink.clone(), Some(intx))
                .map_err(Error::Thread)?;
            disc_drives = ahci.disc_drives();
            if direct_boot {
                ahci.config_mut()
                    .set_bar_address(ABAR.into(), allocate(ABAR_SIZE)?);
            }
            root.add_function(AHCI_SLOT, AHCI_FUNCTION, Arc::new(Mutex::new(ahci)));
            if let Some(first) = config
                .disks
                .iter()
                .find(|d| d.interface == DiskInterface::Nvme)
            {
                let id = first_nvme_id.unwrap_or_else(|| disk_id(&first.path));
                let serial: String = id[..8].iter().map(|b| format!("{b:02X}")).collect();
                let mut nvme = Nvme::new(
                    namespaces,
                    &format!("VMMC{serial}"),
                    vm.memory().clone(),
                    sink.clone(),
                );
                if direct_boot {
                    nvme.config_mut()
                        .set_bar_address(NVME_BAR.into(), allocate(NVME_BAR_SIZE)?);
                }
                root.add_device(NVME_SLOT, Arc::new(Mutex::new(nvme)));
            }
            // USB: a tablet, so the pointer needs no capture, and a keyboard.
            let (tablet, tablet_input) = Tablet::new();
            let (keyboard, keyboard_input) = UsbKeyboard::new();
            let mut xhci = Xhci::new(
                vm.memory().clone(),
                sink.clone(),
                Some(irq(acpi::intx_gsi(XHCI_SLOT, 0) as u8)),
                vec![Box::new(tablet), Box::new(keyboard)],
            );
            if direct_boot {
                xhci.config_mut()
                    .set_bar_address(XHCI_BAR.into(), allocate(XHCI_BAR_SIZE)?);
            }
            let xhci = Arc::new(Mutex::new(xhci));
            root.add_device(XHCI_SLOT, xhci.clone());
            usb = UsbInput {
                xhci,
                tablet: tablet_input,
                keyboard: keyboard_input,
            };
            let mut next_virtio_slot = FIRST_VIRTIO_SLOT;
            for (index, backend) in virtio_disks.into_iter().enumerate() {
                let blk = VirtioBlk::new(backend, &format!("vmmc-disk{index}"));
                let mut pci = VirtioPci::new(Box::new(blk), vm.memory().clone(), sink.clone());
                // Firmware assigns BARs itself; for direct boot, we do it the same way.
                if direct_boot {
                    for (bar, size) in [(CONFIG_BAR, CONFIG_BAR_SIZE), (MSIX_BAR, MSIX_BAR_SIZE)] {
                        pci.config_mut()
                            .set_bar_address(bar.into(), allocate(size)?);
                    }
                }
                let slot = FIRST_VIRTIO_SLOT + index as u8;
                let device: SharedPciDevice = Arc::new(Mutex::new(pci));
                root.add_device(slot, device);
                next_virtio_slot = slot + 1;
            }
            let mut e1000e_added = false;
            let nat_config = || NatConfig {
                service: config.nat_service.clone(),
                ..NatConfig::default()
            };
            for nic in &config.nics {
                match nic.model {
                    NicModel::Virtio => {
                        let mut device = VirtioNet::new(nic.mac);
                        let receiver = device.receiver();
                        let nat =
                            Nat::start(nat_config(), Arc::new(receiver)).map_err(Error::Thread)?;
                        device.set_backend(Arc::new(nat));
                        let mut pci =
                            VirtioPci::new(Box::new(device), vm.memory().clone(), sink.clone());
                        if direct_boot {
                            for (bar, size) in
                                [(CONFIG_BAR, CONFIG_BAR_SIZE), (MSIX_BAR, MSIX_BAR_SIZE)]
                            {
                                pci.config_mut()
                                    .set_bar_address(bar.into(), allocate(size)?);
                            }
                        }
                        if next_virtio_slot >= LPC_SLOT {
                            return Err(Error::Config("too many virtio devices".into()));
                        }
                        root.add_device(next_virtio_slot, Arc::new(Mutex::new(pci)));
                        next_virtio_slot += 1;
                    }
                    NicModel::E1000e => {
                        if e1000e_added {
                            return Err(Error::Config("one e1000e adapter at most".into()));
                        }
                        e1000e_added = true;
                        let intx = irq(acpi::intx_gsi(E1000E_SLOT, 0) as u8);
                        let mut device =
                            E1000e::new(nic.mac, vm.memory().clone(), sink.clone(), Some(intx));
                        let receiver = device.receiver();
                        let nat =
                            Nat::start(nat_config(), Arc::new(receiver)).map_err(Error::Thread)?;
                        device.set_backend(Arc::new(nat));
                        if direct_boot {
                            device
                                .config_mut()
                                .set_bar_address(E1000E_BAR.into(), allocate(E1000E_BAR_SIZE)?);
                        }
                        root.add_device(E1000E_SLOT, Arc::new(Mutex::new(device)));
                    }
                }
            }
        }

        saved_devices.push(saved("pci", &root));
        if let Some(crb) = &crb {
            saved_devices.push(saved("tpm", crb));
        }
        let platform = Arc::new(PlatformState {
            cpus: config.cpus,
            tpm: tpm.is_some(),
            low_ram_end: layout.low_ram_end,
            mmio64: layout.mmio64,
            root,
            mch,
            lpc,
        });
        let rsdp = match firmware {
            None => {
                let tables = acpi::build(&platform.acpi_config()).place(BIOS_AREA_START);
                if BIOS_AREA_START + tables.bytes.len() as u64 > HIGH_MEMORY_START {
                    return Err(Error::Config("ACPI tables don't fit the BIOS area".into()));
                }
                vm.memory()
                    .write_slice(&tables.bytes, GuestAddress(BIOS_AREA_START))?;
                Some(tables.rsdp)
            }
            Some(firmware) => {
                saved_devices.extend(add_firmware(&vm, &layout, &rtc, &platform, firmware)?);
                None
            }
        };

        Ok(Self {
            vm,
            layout,
            cpus: config.cpus,
            rsdp,
            serial,
            com2,
            acpi_pm,
            display,
            ps2,
            usb,
            disc_drives,
            disks: switchable_disks,
            boot: None,
            error: Arc::default(),
            clock: vm_clock,
            saved_devices: Arc::new(saved_devices),
            pause: Arc::default(),
            snapshotting: Arc::default(),
            restored: Mutex::new(None),
        })
    }

    /// RAM, as (guest-physical address, length) ranges.
    fn ram(&self) -> Vec<(u64, u64)> {
        self.layout
            .ram
            .iter()
            .map(|&(address, size)| (address, size as u64))
            .collect()
    }

    /// The writable disks, in the configuration's order: a snapshot switches them to
    /// new deltas while the machine is paused.
    pub fn disks(&self) -> Vec<Arc<disk::Switchable>> {
        self.disks.clone()
    }

    /// A handle that saves the machine's whole state while it runs, from any thread.
    pub fn snapshotter(&self) -> Snapshotter {
        Snapshotter {
            pause: self.pause.clone(),
            stop: self.vm.stop_handle(),
            time: self.vm.time_control(),
            clock: self.clock.clone(),
            memory: self.vm.memory().clone(),
            ram: self.ram(),
            cpus: self.cpus,
            devices: self.saved_devices.clone(),
            saving: self.snapshotting.clone(),
        }
    }

    /// Before [`Machine::run`]: puts back the state a [`Snapshotter`] saved to
    /// `input`, from a machine built with the same configuration. The run then carries
    /// on from it rather than booting.
    pub fn restore(&mut self, input: &mut dyn Read) -> Result<()> {
        let restored = snapshot::restore(input, &self.snapshotter(), |state| {
            Ok(self.vm.restore_state(state)?)
        })?;
        // The devices' clock waits for the guest's, which the run resumes.
        self.clock.pause();
        self.boot = None;
        *lock(&self.restored) = Some(restored.vcpus);
        Ok(())
    }

    /// Prepares a direct boot of `kernel` (a bzImage) with `initrd` and `cmdline`.
    pub fn load_linux(
        &mut self,
        kernel: &[u8],
        initrd: Option<&[u8]>,
        cmdline: &str,
    ) -> Result<()> {
        let rsdp = self
            .rsdp
            .ok_or_else(|| Error::Boot("this machine boots firmware, not a kernel".into()))?;
        self.boot = Some(boot::load_linux(
            self.vm.memory(),
            &self.layout,
            kernel,
            initrd,
            cmdline,
            rsdp,
        )?);
        Ok(())
    }

    pub fn memory(&self) -> &GuestMemoryMmap {
        self.vm.memory()
    }

    pub fn stop_handle(&self) -> StopHandle {
        self.vm.stop_handle()
    }

    pub fn serial_input(&self) -> SerialInput {
        SerialInput(self.serial.clone())
    }

    /// Feeds the guest's second serial port (COM2), where vmmc's agent listens.
    pub fn agent_input(&self) -> SerialInput {
        SerialInput(self.com2.clone())
    }

    /// A handle that presses the guest's ACPI power button from any thread.
    /// A handle that reads what the display adapter shows, from any thread.
    pub fn display(&self) -> Display {
        self.display.clone()
    }

    pub fn power_button(&self) -> PowerButton {
        PowerButton(self.acpi_pm.clone())
    }

    /// A handle that moves the guest's USB tablet and types on its USB keyboard.
    pub fn usb_input(&self) -> UsbInput {
        self.usb.clone()
    }

    /// The DVD drives (on the AHCI controller, CD-ROMs first), whose discs can change
    /// while the guest runs.
    pub fn disc_drives(&self) -> Vec<DiscDrive> {
        self.disc_drives.clone()
    }

    /// A handle that types on the guest's PS/2 keyboard and moves its PS/2 mouse.
    pub fn ps2_input(&self) -> Ps2Input {
        self.ps2.clone()
    }

    /// Runs the guest until it powers off, resets or fails, or the host stops it.
    pub fn run(&self) -> Result<RunOutcome> {
        let stop = self.vm.stop_handle();
        let restored = lock(&self.restored).take();
        let mut vcpus = Vec::with_capacity(self.cpus.into());
        for index in 0..u32::from(self.cpus) {
            let mut vcpu = self.vm.create_vcpu(index)?;
            if let Some(states) = &restored {
                vcpu.restore_state(&states[index as usize])?;
            } else if index == 0 {
                let (regs, sregs) = match &self.boot {
                    Some(entry) => boot::boot_cpu_registers(entry, &vcpu.sregs()?),
                    None => boot::reset_registers(),
                };
                vcpu.set_sregs(&sregs)?;
                vcpu.set_regs(&regs)?;
            } else {
                vcpu.set_waiting_for_sipi()?;
            }
            vcpus.push(vcpu);
        }
        if restored.is_some() {
            self.vm.resume_time()?;
            self.clock.resume();
        }

        thread::scope(|scope| -> Result<()> {
            let mut threads = Vec::with_capacity(vcpus.len());
            for vcpu in vcpus {
                let vcpu_stop = stop.clone();
                let error = self.error.clone();
                let pause = self.pause.clone();
                let thread = thread::Builder::new()
                    .name(format!("vcpu{}", vcpu.index()))
                    .spawn_scoped(scope, move || {
                        pause.enter();
                        run_vcpu(vcpu, &vcpu_stop, &error, &pause);
                        pause.leave();
                    });
                match thread {
                    Ok(thread) => threads.push(thread),
                    Err(e) => {
                        stop.request_stop(StopReason::Error);
                        record_error(&self.error, format!("can't start a vCPU thread: {e}"));
                        break;
                    }
                }
            }
            while stop.wait(Duration::from_secs(1)).is_none() {}
            // A kick can land just before a vCPU enters the guest, so keep kicking
            // until every vCPU thread has returned.
            while !threads.iter().all(|thread| thread.is_finished()) {
                stop.kick_all();
                thread::sleep(Duration::from_millis(5));
            }
            for thread in threads {
                if thread.join().is_err() {
                    record_error(&self.error, "a vCPU thread panicked".into());
                }
            }
            Ok(())
        })?;

        let error = self
            .error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Ok(match (error, stop.reason()) {
            (Some(message), _) => RunOutcome::Error(message),
            (None, Some(StopReason::PowerOff)) => RunOutcome::PowerOff,
            (None, Some(StopReason::Reset)) => RunOutcome::Reset,
            (None, Some(StopReason::TripleFault)) => RunOutcome::TripleFault,
            (None, Some(StopReason::Requested(code))) => RunOutcome::Requested(code),
            (None, Some(StopReason::Error) | None) => {
                RunOutcome::Error("stopped without a reason".into())
            }
        })
    }
}

/// The parts of the platform the ACPI tables describe that the guest can change.
struct PlatformState {
    cpus: u8,
    tpm: bool,
    low_ram_end: u64,
    /// The default 64-bit PCI window.
    mmio64: (u64, u64),
    root: Arc<Mutex<PciRoot>>,
    mch: Arc<Mutex<Mch>>,
    lpc: Arc<Mutex<Ich9Lpc>>,
}

impl PlatformState {
    /// The ACPI description of the platform as it is now. Firmware moves the ECAM
    /// window and the PM block, and assigns BARs, before it reads the tables.
    fn acpi_config(&self) -> acpi::AcpiConfig {
        let ecam = lock(&self.mch)
            .ecam_window()
            .unwrap_or((ECAM_BASE, ECAM_SIZE));
        let pm_base = lock(&self.lpc).pm_base().unwrap_or(PM_BASE);
        // The 32-bit window runs from the end of low RAM (or 2 GiB) up to the ECAM
        // window, as OVMF's does; the 64-bit one covers the default range. Each also
        // stretches over any BAR the firmware put outside it (in whole MiB or GiB).
        let mut mmio32 = mmio32_window(self.low_ram_end, ecam.0.min(RAM_ABOVE_4G_START));
        let (mut start, mut end) = self.mmio64;
        for (address, size) in lock(&self.root).assigned_bar_ranges() {
            if address >= RAM_ABOVE_4G_START {
                start = start.min(address - address % GIB);
                end = end.max((address + size).next_multiple_of(GIB));
            } else if address >= self.low_ram_end && address + size <= ecam.0 {
                mmio32.0 = mmio32.0.min(address - address % MIB);
                mmio32.1 = mmio32.1.max((address + size).next_multiple_of(MIB));
            }
        }
        acpi::AcpiConfig {
            cpus: self.cpus,
            mmio32,
            mmio64: (start, end),
            ecam,
            pm_base,
            tpm: self.tpm,
        }
    }
}

/// A stable 16-byte ID for the disk image at `path` (FNV-1a over the path, twice), used
/// for the NVMe NGUID and serial number. It stays the same as long as the path does.
fn disk_id(path: &std::path::Path) -> [u8; 16] {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let bytes = path.to_string_lossy().into_owned().into_bytes();
    let fnv = |seed: u64| {
        bytes.iter().fold(seed, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    };
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&fnv(0xcbf2_9ce4_8422_2325).to_be_bytes());
    id[8..].copy_from_slice(&fnv(0x6c62_272e_07bb_0142).to_be_bytes());
    id
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Adds what UEFI firmware needs: its flash, fw_cfg (memory map, CPU count, ACPI and
/// SMBIOS tables), the CMOS memory size bytes and, optionally, the debug port.
fn add_firmware<P: Partition + 'static>(
    vm: &Vm<P>,
    layout: &MemoryLayout,
    rtc: &Mutex<Rtc>,
    platform: &Arc<PlatformState>,
    firmware: FirmwareConfig,
) -> Result<Vec<SavedDevice>> {
    let vars_error = |source| Error::FirmwareVars {
        path: firmware.vars.clone(),
        source,
    };
    let mut vars_file = File::options()
        .read(true)
        .write(true)
        .open(&firmware.vars)
        .map_err(vars_error)?;
    let mut vars = Vec::new();
    vars_file.read_to_end(&mut vars).map_err(vars_error)?;

    // Code at the top of 4 GiB, so the reset vector is its last 16 bytes, and the
    // variable store right below it.
    for (what, len) in [
        ("code", firmware.code.len()),
        ("variable store", vars.len()),
    ] {
        if len == 0 || len % 0x1000 != 0 {
            return Err(Error::Config(format!(
                "firmware {what} size {len:#x} isn't a whole number of 4 KiB blocks"
            )));
        }
    }
    let flash_len = (firmware.code.len() + vars.len()) as u64;
    if flash_len > FLASH_MAX {
        return Err(Error::Config(format!(
            "firmware is {flash_len:#x} bytes; at most {FLASH_MAX:#x} fit below 4 GiB"
        )));
    }
    let code_base = RAM_ABOVE_4G_START - firmware.code.len() as u64;
    let vars_base = code_base - vars.len() as u64;
    let mapper = vm.ram_mapper();
    let vars_flash = shared(
        Pflash::new(vars_base, &vars, mapper.clone(), Some(vars_file)).map_err(Error::Flash)?,
    );
    vm.mmio()
        .insert(vars_base, vars.len() as u64, vars_flash.clone())?;
    let code_flash =
        shared(Pflash::read_only(code_base, &firmware.code, mapper).map_err(Error::Flash)?);
    vm.mmio()
        .insert(code_base, firmware.code.len() as u64, code_flash.clone())?;

    if let Some(log) = firmware.debug_log {
        vm.pio()
            .insert(OVMF_DEBUG_PORT.into(), 1, shared(DebugConsole::new(log)))?;
    }

    // CMOS memory size bytes, the way firmware has always read them.
    {
        let below_4g = layout.low_ram_end;
        let above_4g: u64 = layout
            .ram
            .iter()
            .filter(|&&(address, _)| address >= RAM_ABOVE_4G_START)
            .map(|&(_, size)| size as u64)
            .sum();
        let mut rtc = lock(rtc);
        let mut set16 = |index: u8, value: u64| {
            let value = value.min(0xffff) as u16;
            rtc.set_cmos_byte(index, value as u8);
            rtc.set_cmos_byte(index + 1, (value >> 8) as u8);
        };
        // Base memory in KiB, then extended memory (1 MiB to 64 MiB) in KiB, twice.
        set16(0x15, 640);
        set16(0x17, (below_4g - MIB) >> 10);
        set16(0x30, (below_4g - MIB) >> 10);
        // RAM between 16 MiB and the low RAM end, in 64 KiB units.
        set16(0x34, below_4g.saturating_sub(16 * MIB) >> 16);
        let high = above_4g >> 16;
        rtc.set_cmos_byte(0x5b, high as u8);
        rtc.set_cmos_byte(0x5c, (high >> 8) as u8);
        rtc.set_cmos_byte(0x5d, (high >> 16) as u8);
        rtc.set_cmos_byte(0x5f, platform.cpus - 1);
    }

    let mut fw_cfg = FwCfg::new(vm.memory().clone());
    let total: u64 = layout.ram.iter().map(|&(_, size)| size as u64).sum();
    fw_cfg.add_item(fw_cfg::RAM_SIZE, total.to_le_bytes().to_vec());
    let cpus = u16::from(platform.cpus).to_le_bytes().to_vec();
    fw_cfg.add_item(fw_cfg::NB_CPUS, cpus.clone());
    fw_cfg.add_item(fw_cfg::MAX_CPUS, cpus);
    fw_cfg.add_file("etc/e820", layout.e820_bytes());

    // ACPI: generated when firmware reads it, which is after it has assigned BARs.
    for (name, part) in [
        (acpi::RSDP_FILE, 0),
        (acpi::TABLES_FILE, 1),
        (acpi::LOADER_FILE, 2),
    ] {
        let platform = platform.clone();
        fw_cfg.add_generated_file(
            name,
            Arc::new(move || {
                let files = acpi::build(&platform.acpi_config()).loader();
                match part {
                    0 => files.rsdp,
                    1 => files.tables,
                    _ => files.script,
                }
            }),
        );
    }

    let smbios = smbios::build(&smbios::SmbiosConfig {
        uuid: firmware.uuid,
        cpus: platform.cpus,
        ram: layout
            .ram
            .iter()
            .map(|&(address, size)| (address, size as u64))
            .collect(),
    });
    fw_cfg.add_file(smbios::ANCHOR_FILE, smbios.anchor);
    fw_cfg.add_file(smbios::TABLES_FILE, smbios.tables);

    let fw_cfg = shared(fw_cfg);
    vm.pio()
        .insert(FW_CFG_BASE.into(), FW_CFG_PORTS.into(), fw_cfg.clone())?;
    Ok(vec![
        saved("flash-code", &code_flash),
        saved("flash-vars", &vars_flash),
        saved("fw-cfg", &fw_cfg),
    ])
}

fn record_error(slot: &Mutex<Option<String>>, message: String) {
    tracing::error!("{message}");
    slot.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert(message);
}

/// Runs one vCPU until the machine stops, stopping it on errors and triple faults.
fn run_vcpu<P: Partition>(
    mut vcpu: VmVcpu<'_, P>,
    stop: &StopHandle,
    error: &Mutex<Option<String>>,
    pause: &Pause,
) {
    let index = vcpu.index();
    let mut exit = vcpu.run();
    // A pause (for a snapshot) parks the vCPU, then it runs on.
    while let Ok(VmExit::Paused) = exit {
        pause.park(&vcpu, stop);
        exit = vcpu.run();
    }
    match exit {
        Ok(VmExit::Paused) => unreachable!("handled above"),
        Ok(VmExit::Stopped(_)) => {
            // Where the guest was, for debugging hangs.
            if tracing::enabled!(tracing::Level::DEBUG)
                && let (Ok(regs), Ok(sregs)) = (vcpu.regs(), vcpu.sregs())
            {
                tracing::debug!(
                    index,
                    rip = format_args!("{:#x}", regs.rip),
                    rflags = format_args!("{:#x}", regs.rflags),
                    cs = format_args!("{:#x}/{:#x}", sregs.cs.selector, sregs.cs.base),
                    cr0 = format_args!("{:#x}", sregs.cr0),
                    efer = format_args!("{:#x}", sregs.efer),
                    "vCPU stopped"
                );
            }
        }
        Ok(VmExit::Halt) => {
            // Halted with interrupts off: only INIT or NMI could wake it, and nothing
            // sends those. Wait for the rest of the machine to stop (parking for a
            // snapshot meanwhile).
            tracing::debug!(index, "vCPU halted with interrupts disabled");
            while stop.reason().is_none() {
                if stop.is_paused() {
                    pause.park(&vcpu, stop);
                } else {
                    stop.wait(Duration::from_millis(100));
                }
            }
        }
        Ok(VmExit::Shutdown) => {
            tracing::warn!(index, "vCPU triple-faulted");
            stop.request_stop(StopReason::TripleFault);
        }
        Ok(VmExit::Unhandled(reason)) => {
            record_error(error, format!("vCPU {index}: {reason}"));
            stop.request_stop(StopReason::Error);
        }
        Err(e) => {
            record_error(error, format!("vCPU {index}: {e}"));
            stop.request_stop(StopReason::Error);
        }
    }
}
