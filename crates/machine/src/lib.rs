//! The vmmc x86 PC platform: memory layout, ACPI tables, Linux boot and vCPU threads.
//!
//! [`Machine::new`] builds the VM and its devices; [`Machine::load_linux`] prepares a
//! direct kernel boot; [`Machine::run`] runs one thread per vCPU until the guest powers
//! off, resets, or fails.

pub mod acpi;
pub mod boot;
pub mod layout;
mod msi;

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use devices::clock::{Clock, HostClock};
use devices::irq::{IrqLine, MsiSink};
use devices::legacy::acpi_pm::{AcpiPm, PM_BASE, PM_PORTS};
use devices::legacy::i8042::{I8042, I8042_COMMAND, I8042_DATA, I8042CommandPort};
use devices::legacy::ioapic::{IOAPIC_BASE, IOAPIC_SIZE, IoApic};
use devices::legacy::pic::{
    ELCR_BASE, ELCR_PORTS, Elcr, MASTER_BASE, PIC_PORTS, Pic8259, SLAVE_BASE,
};
use devices::legacy::pit::{PIT_BASE, PIT_PORTS, PORT_61, Pit, Port61};
use devices::legacy::port80::{POST_PORT, PostPort};
use devices::legacy::rtc::{RTC_BASE, RTC_PORTS, Rtc};
use devices::legacy::serial::{COM1_BASE, COM1_IRQ, SERIAL_PORTS, Serial};
use devices::pci::host_bridge::{HOST_BRIDGE_SLOT, HostBridge};
use devices::pci::root::{
    CONFIG_IO_BASE, CONFIG_IO_PORTS, ECAM_SIZE, PciConfigIo, PciEcam, PciRoot,
};
use devices::pci::{PciDevice, SharedPciDevice};
use devices::timer::ThreadTicker;
use devices::virtio::blk::{RawFile, VirtioBlk};
use devices::virtio::pci::{CONFIG_BAR, CONFIG_BAR_SIZE, MSIX_BAR, MSIX_BAR_SIZE, VirtioPci};
use devices::{PowerEvent, PowerHandler};
use hv::{Hypervisor, LocalApicMode, Partition, PartitionConfig};
use vm_core::{BusDevice, StopHandle, StopReason, Vm, VmExit, VmVcpu};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

pub use crate::boot::BootEntry;
use crate::layout::{
    BIOS_AREA_START, ECAM_BASE, HIGH_MEMORY_START, MIB, MIN_MEMORY, MemoryLayout, Mmio32Allocator,
};
pub use crate::msi::HvMsiSink;

/// Most vCPUs a machine can have (xAPIC physical destination IDs).
pub const MAX_CPUS: u8 = 32;
/// The PIT's IRQ 0 reaches IOAPIC pin 2 (see the MADT override).
const PIT_IOAPIC_PIN: u8 = 2;
const KEYBOARD_IRQ: u8 = 1;
/// PCI slot of the first disk.
const FIRST_DISK_SLOT: u8 = 1;

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
    #[error("can't start a thread: {0}")]
    Thread(io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct DiskConfig {
    /// A raw disk image.
    pub path: PathBuf,
    pub read_only: bool,
}

pub struct MachineConfig {
    pub cpus: u8,
    /// RAM in bytes; at least 32 MiB, a multiple of 1 MiB.
    pub memory: u64,
    /// Disks, attached as virtio-blk devices in order.
    pub disks: Vec<DiskConfig>,
    /// Where the guest's serial console output goes.
    pub serial_output: Box<dyn Write + Send>,
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

pub struct Machine<P: Partition> {
    vm: Vm<P>,
    layout: MemoryLayout,
    cpus: u8,
    rsdp: u64,
    serial: Arc<Mutex<Serial>>,
    acpi_pm: Arc<Mutex<AcpiPm>>,
    boot: Option<BootEntry>,
    /// What went wrong, if a vCPU stopped the machine with an error.
    error: Arc<Mutex<Option<String>>>,
}

fn shared<T: BusDevice + 'static>(device: T) -> Arc<Mutex<T>> {
    Arc::new(Mutex::new(device))
}

impl<P: Partition> Machine<P> {
    pub fn new<H>(hypervisor: &H, config: MachineConfig) -> Result<Self>
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
        let layout = MemoryLayout::new(config.memory);
        let mut vm = Vm::new(
            hypervisor,
            &PartitionConfig {
                vcpu_count: config.cpus.into(),
                local_apic: LocalApicMode::XApic,
            },
            &layout.ram,
        )?;

        let clock: Arc<dyn Clock> = Arc::new(HostClock::new());
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
        pio.insert(PORT_61.into(), 1, shared(Port61(pit)))?;

        pio.insert(
            MASTER_BASE.into(),
            PIC_PORTS.into(),
            shared(Pic8259::default()),
        )?;
        pio.insert(
            SLAVE_BASE.into(),
            PIC_PORTS.into(),
            shared(Pic8259::default()),
        )?;
        pio.insert(ELCR_BASE.into(), ELCR_PORTS.into(), shared(Elcr::default()))?;
        pio.insert(
            RTC_BASE.into(),
            RTC_PORTS.into(),
            shared(Rtc::new(clock.clone())),
        )?;

        let acpi_pm = shared(AcpiPm::new(clock, irq(acpi::SCI_IRQ), power.clone()));
        pio.insert(PM_BASE.into(), PM_PORTS.into(), acpi_pm.clone())?;

        let i8042 = shared(I8042::new(irq(KEYBOARD_IRQ), power.clone()));
        pio.insert(I8042_DATA.into(), 1, i8042.clone())?;
        pio.insert(I8042_COMMAND.into(), 1, shared(I8042CommandPort(i8042)))?;

        let serial = shared(Serial::new(irq(COM1_IRQ), config.serial_output));
        pio.insert(COM1_BASE.into(), SERIAL_PORTS.into(), serial.clone())?;
        pio.insert(POST_PORT.into(), 1, shared(PostPort::default()))?;

        let root = Arc::new(Mutex::new(PciRoot::new(vm.mmio().clone())));
        pio.insert(
            CONFIG_IO_BASE.into(),
            CONFIG_IO_PORTS.into(),
            shared(PciConfigIo::new(root.clone(), power)),
        )?;
        vm.mmio()
            .insert(ECAM_BASE, ECAM_SIZE, shared(PciEcam(root.clone())))?;
        {
            let mut root = root.lock().unwrap();
            root.add_device(HOST_BRIDGE_SLOT, Arc::new(Mutex::new(HostBridge::new())));
            let mut mmio32 = Mmio32Allocator::default();
            for (index, disk) in config.disks.iter().enumerate() {
                let backend =
                    RawFile::open(&disk.path, disk.read_only).map_err(|source| Error::Disk {
                        path: disk.path.clone(),
                        source,
                    })?;
                let blk = VirtioBlk::new(Arc::new(backend), &format!("vmmc-disk{index}"));
                let mut pci = VirtioPci::new(Box::new(blk), vm.memory().clone(), sink.clone());
                for (bar, size) in [(CONFIG_BAR, CONFIG_BAR_SIZE), (MSIX_BAR, MSIX_BAR_SIZE)] {
                    let address = mmio32
                        .allocate(size)
                        .ok_or_else(|| Error::Config("out of PCI address space".into()))?;
                    pci.config_mut().set_bar_address(bar.into(), address);
                }
                let slot = FIRST_DISK_SLOT + index as u8;
                let device: SharedPciDevice = Arc::new(Mutex::new(pci));
                root.add_device(slot, device);
            }
        }

        let tables = acpi::build(
            &acpi::AcpiConfig {
                cpus: config.cpus,
                mmio64: layout.mmio64,
            },
            BIOS_AREA_START,
        );
        if BIOS_AREA_START + tables.bytes.len() as u64 > HIGH_MEMORY_START {
            return Err(Error::Config("ACPI tables don't fit the BIOS area".into()));
        }
        vm.memory()
            .write_slice(&tables.bytes, GuestAddress(BIOS_AREA_START))?;

        Ok(Self {
            vm,
            layout,
            cpus: config.cpus,
            rsdp: tables.rsdp,
            serial,
            acpi_pm,
            boot: None,
            error: Arc::default(),
        })
    }

    /// Prepares a direct boot of `kernel` (a bzImage) with `initrd` and `cmdline`.
    pub fn load_linux(
        &mut self,
        kernel: &[u8],
        initrd: Option<&[u8]>,
        cmdline: &str,
    ) -> Result<()> {
        self.boot = Some(boot::load_linux(
            self.vm.memory(),
            &self.layout,
            kernel,
            initrd,
            cmdline,
            self.rsdp,
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

    /// Presses the ACPI power button; a guest with ACPI support shuts down cleanly.
    pub fn press_power_button(&self) {
        self.acpi_pm
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .press_power_button();
    }

    /// Runs the guest until it powers off, resets or fails, or the host stops it.
    pub fn run(&self) -> Result<RunOutcome> {
        let stop = self.vm.stop_handle();
        let mut vcpus = Vec::with_capacity(self.cpus.into());
        for index in 0..u32::from(self.cpus) {
            let mut vcpu = self.vm.create_vcpu(index)?;
            if index == 0 {
                if let Some(entry) = &self.boot {
                    let reset = vcpu.sregs()?;
                    let (regs, sregs) = boot::boot_cpu_registers(entry, &reset);
                    vcpu.set_sregs(&sregs)?;
                    vcpu.set_regs(&regs)?;
                }
            } else {
                vcpu.set_waiting_for_sipi()?;
            }
            vcpus.push(vcpu);
        }

        thread::scope(|scope| -> Result<()> {
            let mut threads = Vec::with_capacity(vcpus.len());
            for vcpu in vcpus {
                let vcpu_stop = stop.clone();
                let error = self.error.clone();
                let thread = thread::Builder::new()
                    .name(format!("vcpu{}", vcpu.index()))
                    .spawn_scoped(scope, move || run_vcpu(vcpu, &vcpu_stop, &error));
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
) {
    let index = vcpu.index();
    match vcpu.run() {
        Ok(VmExit::Stopped(_)) => {}
        Ok(VmExit::Halt) => {
            // Halted with interrupts off: only INIT or NMI could wake it, and nothing
            // sends those. Wait for the rest of the machine to stop.
            tracing::debug!(index, "vCPU halted with interrupts disabled");
            while stop.wait(Duration::from_secs(1)).is_none() {}
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
