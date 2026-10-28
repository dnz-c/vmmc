//! The virtio-pci transport for modern (virtio 1.x) devices.
//!
//! BAR 0 holds the common configuration, ISR status, device configuration and notify
//! regions. BAR 2 holds the MSI-X table and PBA. Interrupts are MSI-X only.

use std::sync::atomic::{AtomicU8, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use virtio_queue::{Queue, QueueT};
use vm_memory::GuestMemoryMmap;

use super::{
    QueueInterrupt, SavedQueue, VIRTIO_F_RING_EVENT_IDX, VIRTIO_F_VERSION_1, VirtioDevice,
};
use crate::irq::MsiSink;
use crate::pci::msix::{self, MsixState, MsixTable};
use crate::pci::{BarKind, ConfigSpace, Identity, PciDevice};
use crate::snapshot::{self, Snapshot, StateError};

pub const VIRTIO_VENDOR_ID: u16 = 0x1af4;
/// Modern device IDs are 0x1040 plus the virtio device type.
const MODERN_DEVICE_ID_BASE: u16 = 0x1040;
/// "No MSI-X vector" in the vector registers.
pub const NO_VECTOR: u16 = 0xffff;

const VENDOR_CAPABILITY: u8 = 0x09;
const CAP_COMMON_CFG: u8 = 1;
const CAP_NOTIFY_CFG: u8 = 2;
const CAP_ISR_CFG: u8 = 3;
const CAP_DEVICE_CFG: u8 = 4;
const CAP_PCI_CFG: u8 = 5;

pub const CONFIG_BAR: u8 = 0;
pub const MSIX_BAR: u8 = 2;
pub const CONFIG_BAR_SIZE: u64 = 0x4000;
pub const MSIX_BAR_SIZE: u64 = 0x2000;

const COMMON_OFFSET: u64 = 0x0000;
const COMMON_SIZE: u64 = 0x38;
const ISR_OFFSET: u64 = 0x1000;
const DEVICE_OFFSET: u64 = 0x2000;
const DEVICE_SIZE: u64 = 0x1000;
const NOTIFY_OFFSET: u64 = 0x3000;
const NOTIFY_MULTIPLIER: u32 = 4;
const MSIX_TABLE_OFFSET: u32 = 0;
const MSIX_PBA_OFFSET: u32 = 0x1000;

const STATUS_FEATURES_OK: u8 = 8;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_NEEDS_RESET: u8 = 64;

/// A queue's registers, as the driver programs them.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct QueueConfig {
    max_size: u16,
    size: u16,
    ready: bool,
    desc: u64,
    avail: u64,
    used: u64,
}

impl QueueConfig {
    fn new(max_size: u16) -> Self {
        Self {
            max_size,
            size: max_size,
            ready: false,
            desc: 0,
            avail: 0,
            used: 0,
        }
    }

    fn build(&self, event_idx: bool) -> Result<Queue, String> {
        let mut queue = Queue::new(self.max_size).map_err(|e| e.to_string())?;
        queue.set_size(self.size);
        queue.set_desc_table_address(Some(self.desc as u32), Some((self.desc >> 32) as u32));
        queue.set_avail_ring_address(Some(self.avail as u32), Some((self.avail >> 32) as u32));
        queue.set_used_ring_address(Some(self.used as u32), Some((self.used >> 32) as u32));
        queue.set_event_idx(event_idx);
        queue.set_ready(self.ready);
        Ok(queue)
    }
}

/// The MSI-X vectors the driver assigned.
struct Vectors {
    config: AtomicU16,
    queues: Vec<AtomicU16>,
}

/// How a running device interrupts the driver.
struct Interrupt {
    msix: Arc<Mutex<MsixTable>>,
    vectors: Arc<Vectors>,
    isr: Arc<AtomicU8>,
}

impl Interrupt {
    fn notify(&self, vector: u16) {
        if vector != NO_VECTOR {
            self.msix.lock().unwrap().notify(vector);
        }
    }
}

impl QueueInterrupt for Interrupt {
    fn signal_queue(&self, queue: u16) {
        self.isr.fetch_or(1, Ordering::SeqCst);
        if let Some(vector) = self.vectors.queues.get(usize::from(queue)) {
            self.notify(vector.load(Ordering::SeqCst));
        }
    }

    fn signal_config(&self) {
        self.isr.fetch_or(2, Ordering::SeqCst);
        self.notify(self.vectors.config.load(Ordering::SeqCst));
    }
}

pub struct VirtioPci {
    config: ConfigSpace,
    device: Box<dyn VirtioDevice>,
    memory: GuestMemoryMmap,
    msix: Arc<Mutex<MsixTable>>,
    msix_capability: u16,
    pci_cfg_capability: u16,
    isr: Arc<AtomicU8>,
    vectors: Arc<Vectors>,
    device_feature_select: u32,
    driver_feature_select: u32,
    driver_features: u64,
    status: u8,
    queue_select: u16,
    queues: Vec<QueueConfig>,
    active: bool,
}

impl VirtioPci {
    pub fn new(
        device: Box<dyn VirtioDevice>,
        memory: GuestMemoryMmap,
        sink: Arc<dyn MsiSink>,
    ) -> Self {
        let device_type = device.device_type();
        let queues: Vec<QueueConfig> = device
            .queue_max_sizes()
            .iter()
            .map(|&max| QueueConfig::new(max))
            .collect();
        let num_queues = queues.len() as u16;

        let mut config = ConfigSpace::new(Identity {
            vendor_id: VIRTIO_VENDOR_ID,
            device_id: MODERN_DEVICE_ID_BASE + device_type,
            revision: 1,
            class: class_code(device_type),
            subsystem_vendor_id: VIRTIO_VENDOR_ID,
            subsystem_id: 0x1100,
        });
        config.add_bar(
            usize::from(CONFIG_BAR),
            BarKind::Memory64,
            CONFIG_BAR_SIZE,
            false,
        );
        config.add_bar(
            usize::from(MSIX_BAR),
            BarKind::Memory64,
            MSIX_BAR_SIZE,
            false,
        );

        virtio_capability(&mut config, CAP_COMMON_CFG, COMMON_OFFSET, COMMON_SIZE, &[]);
        virtio_capability(
            &mut config,
            CAP_NOTIFY_CFG,
            NOTIFY_OFFSET,
            u64::from(num_queues) * u64::from(NOTIFY_MULTIPLIER),
            &NOTIFY_MULTIPLIER.to_le_bytes(),
        );
        virtio_capability(&mut config, CAP_ISR_CFG, ISR_OFFSET, 1, &[]);
        virtio_capability(&mut config, CAP_DEVICE_CFG, DEVICE_OFFSET, DEVICE_SIZE, &[]);
        // The PCI configuration access capability: a window into the BARs through config
        // space. Its bar, offset, length and data fields are writable.
        let pci_cfg_capability = virtio_capability(&mut config, CAP_PCI_CFG, 0, 0, &[0; 4]);
        config.set_wmask_u16(pci_cfg_capability + 4, 0x00ff);
        config.set_wmask_u32(pci_cfg_capability + 8, u32::MAX);
        config.set_wmask_u32(pci_cfg_capability + 12, u32::MAX);
        config.set_wmask_u32(pci_cfg_capability + 16, u32::MAX);

        // One vector per queue plus one for configuration changes.
        let vectors = num_queues + 1;
        let msix_capability = msix::add_capability(
            &mut config,
            vectors,
            (MSIX_BAR, MSIX_TABLE_OFFSET),
            (MSIX_BAR, MSIX_PBA_OFFSET),
        );
        assert!(u64::from(vectors) * msix::ENTRY_SIZE <= u64::from(MSIX_PBA_OFFSET));

        Self {
            config,
            device,
            memory,
            msix: Arc::new(Mutex::new(MsixTable::new(vectors, sink))),
            msix_capability,
            pci_cfg_capability,
            isr: Arc::new(AtomicU8::new(0)),
            vectors: Arc::new(Vectors {
                config: AtomicU16::new(NO_VECTOR),
                queues: (0..num_queues).map(|_| AtomicU16::new(NO_VECTOR)).collect(),
            }),
            device_feature_select: 0,
            driver_feature_select: 0,
            driver_features: 0,
            status: 0,
            queue_select: 0,
            queues,
            active: false,
        }
    }

    fn selected_queue(&self) -> Option<&QueueConfig> {
        self.queues.get(usize::from(self.queue_select))
    }

    fn selected_queue_mut(&mut self) -> Option<&mut QueueConfig> {
        // Queues can't change once the device runs.
        if self.active {
            return None;
        }
        self.queues.get_mut(usize::from(self.queue_select))
    }

    fn valid_vector(&self, vector: u16) -> u16 {
        if vector < self.msix.lock().unwrap().len() {
            vector
        } else {
            NO_VECTOR
        }
    }

    fn read_common(&self, offset: u64, data: &mut [u8]) {
        let queue = self.selected_queue();
        let features = self.device.features();
        let dword = |value: u64, select: u32| match select {
            0 => value as u32,
            1 => (value >> 32) as u32,
            _ => 0,
        };
        let value: u64 = match offset {
            0x00 => self.device_feature_select.into(),
            0x04 => dword(features, self.device_feature_select).into(),
            0x08 => self.driver_feature_select.into(),
            0x0c => dword(self.driver_features, self.driver_feature_select).into(),
            0x10 => self.vectors.config.load(Ordering::SeqCst).into(),
            0x12 => self.queues.len() as u64,
            0x14 => self.status.into(),
            // What changes in a configuration (virtio-gpu's events) is read in one
            // access, so its generation stays 0.
            0x15 => 0,
            0x16 => self.queue_select.into(),
            0x18 => queue.map_or(0, |q| q.size.into()),
            0x1a => self
                .vectors
                .queues
                .get(usize::from(self.queue_select))
                .map_or(u64::from(NO_VECTOR), |v| v.load(Ordering::SeqCst).into()),
            0x1c => queue.map_or(0, |q| q.ready.into()),
            0x1e => self.queue_select.into(),
            0x20 => queue.map_or(0, |q| q.desc),
            0x24 => queue.map_or(0, |q| q.desc >> 32),
            0x28 => queue.map_or(0, |q| q.avail),
            0x2c => queue.map_or(0, |q| q.avail >> 32),
            0x30 => queue.map_or(0, |q| q.used),
            0x34 => queue.map_or(0, |q| q.used >> 32),
            _ => {
                tracing::debug!(offset, "read of unknown virtio common config field");
                0
            }
        };
        let bytes = value.to_le_bytes();
        let len = data.len().min(8);
        data[..len].copy_from_slice(&bytes[..len]);
    }

    fn write_common(&mut self, offset: u64, data: &[u8]) {
        let mut bytes = [0u8; 8];
        let len = data.len().min(8);
        bytes[..len].copy_from_slice(&data[..len]);
        let value = u64::from_le_bytes(bytes);
        let whole = data.len() == 8;
        /// Writes a 64-bit address register: all of it, or its low or high half.
        fn set_address(field: &mut u64, high: bool, whole: bool, value: u64) {
            *field = if whole {
                value
            } else if high {
                (*field & 0xffff_ffff) | ((value & 0xffff_ffff) << 32)
            } else {
                (*field & !0xffff_ffff) | (value & 0xffff_ffff)
            };
        }
        match offset {
            0x00 => self.device_feature_select = value as u32,
            0x08 => self.driver_feature_select = value as u32,
            0x0c => {
                if self.status & STATUS_FEATURES_OK == 0 {
                    let shift = match self.driver_feature_select {
                        0 => 0,
                        1 => 32,
                        _ => return,
                    };
                    self.driver_features = (self.driver_features & !(0xffff_ffff << shift))
                        | ((value & 0xffff_ffff) << shift);
                }
            }
            0x10 => {
                let vector = self.valid_vector(value as u16);
                self.vectors.config.store(vector, Ordering::SeqCst);
            }
            0x14 => self.write_status(value as u8),
            0x16 => self.queue_select = value as u16,
            0x18 => {
                if let Some(queue) = self.selected_queue_mut() {
                    let size = value as u16;
                    if size.is_power_of_two() && size <= queue.max_size {
                        queue.size = size;
                    } else {
                        tracing::warn!(size, "invalid virtqueue size");
                    }
                }
            }
            0x1a => {
                let vector = self.valid_vector(value as u16);
                if let Some(slot) = self.vectors.queues.get(usize::from(self.queue_select)) {
                    slot.store(vector, Ordering::SeqCst);
                }
            }
            0x1c => {
                if let Some(queue) = self.selected_queue_mut() {
                    queue.ready = value & 1 != 0;
                }
            }
            0x20 | 0x24 => {
                if let Some(queue) = self.selected_queue_mut() {
                    set_address(&mut queue.desc, offset == 0x24, whole, value);
                }
            }
            0x28 | 0x2c => {
                if let Some(queue) = self.selected_queue_mut() {
                    set_address(&mut queue.avail, offset == 0x2c, whole, value);
                }
            }
            0x30 | 0x34 => {
                if let Some(queue) = self.selected_queue_mut() {
                    set_address(&mut queue.used, offset == 0x34, whole, value);
                }
            }
            _ => tracing::debug!(
                offset,
                ?data,
                "write to read-only virtio common config field"
            ),
        }
    }

    fn write_status(&mut self, value: u8) {
        if value == 0 {
            self.reset();
            return;
        }
        let mut status = value;
        if status & STATUS_FEATURES_OK != 0 && self.status & STATUS_FEATURES_OK == 0 {
            let offered = self.device.features();
            let unknown = self.driver_features & !offered;
            if unknown != 0 || self.driver_features & VIRTIO_F_VERSION_1 == 0 {
                tracing::warn!(
                    driver = self.driver_features,
                    offered,
                    "driver features rejected"
                );
                status &= !STATUS_FEATURES_OK;
            }
        }
        if status & STATUS_DRIVER_OK != 0
            && !self.active
            && let Err(e) = self.activate()
        {
            tracing::error!(%e, "virtio device failed to start");
            status |= STATUS_NEEDS_RESET;
        }
        self.status = status;
    }

    fn activate(&mut self) -> Result<(), String> {
        let event_idx = self.driver_features & VIRTIO_F_RING_EVENT_IDX != 0;
        let queues = self
            .queues
            .iter()
            .map(|queue| queue.build(event_idx))
            .collect::<Result<Vec<_>, _>>()?;
        self.start(queues)
    }

    /// Starts the device on `queues`.
    fn start(&mut self, queues: Vec<Queue>) -> Result<(), String> {
        let interrupt = Arc::new(Interrupt {
            msix: self.msix.clone(),
            vectors: self.vectors.clone(),
            isr: self.isr.clone(),
        });
        self.device
            .activate(self.memory.clone(), self.driver_features, queues, interrupt)?;
        self.active = true;
        Ok(())
    }

    fn reset(&mut self) {
        if self.active {
            self.device.reset();
            self.active = false;
        }
        self.status = 0;
        self.device_feature_select = 0;
        self.driver_feature_select = 0;
        self.driver_features = 0;
        self.queue_select = 0;
        for queue in &mut self.queues {
            *queue = QueueConfig::new(queue.max_size);
        }
        self.vectors.config.store(NO_VECTOR, Ordering::SeqCst);
        for vector in &self.vectors.queues {
            vector.store(NO_VECTOR, Ordering::SeqCst);
        }
        self.isr.store(0, Ordering::SeqCst);
    }

    fn config_bar_read(&mut self, offset: u64, data: &mut [u8]) {
        match offset {
            o if o < COMMON_OFFSET + COMMON_SIZE => self.read_common(o - COMMON_OFFSET, data),
            o if o == ISR_OFFSET => {
                // Reading the ISR acknowledges it.
                data.fill(0);
                data[0] = self.isr.swap(0, Ordering::SeqCst);
            }
            o if (DEVICE_OFFSET..DEVICE_OFFSET + DEVICE_SIZE).contains(&o) => {
                self.device.read_config(o - DEVICE_OFFSET, data);
            }
            _ => data.fill(0),
        }
    }

    fn config_bar_write(&mut self, offset: u64, data: &[u8]) {
        match offset {
            o if o < COMMON_OFFSET + COMMON_SIZE => self.write_common(o - COMMON_OFFSET, data),
            o if (DEVICE_OFFSET..DEVICE_OFFSET + DEVICE_SIZE).contains(&o) => {
                self.device.write_config(o - DEVICE_OFFSET, data);
            }
            o if o >= NOTIFY_OFFSET => {
                let queue = ((o - NOTIFY_OFFSET) / u64::from(NOTIFY_MULTIPLIER)) as u16;
                if self.active && usize::from(queue) < self.queues.len() {
                    self.device.queue_notify(queue);
                }
            }
            _ => tracing::debug!(offset, "write to read-only virtio register"),
        }
    }

    /// The BAR access a `PCI_CFG` capability window currently describes.
    fn pci_cfg_window(&self) -> (u8, u64, usize) {
        let cap = self.pci_cfg_capability;
        let bar = self.config.u8_at(cap + 4);
        let offset = u64::from(self.config.u32_at(cap + 8));
        let length = (self.config.u32_at(cap + 12) as usize).min(4);
        (bar, offset, length)
    }
}

/// Adds a `virtio_pci_cap` (plus `extra` trailing bytes) pointing into BAR 0.
fn virtio_capability(
    config: &mut ConfigSpace,
    cfg_type: u8,
    offset: u64,
    length: u64,
    extra: &[u8],
) -> u16 {
    let mut body = vec![16 + extra.len() as u8, cfg_type, CONFIG_BAR, 0, 0, 0];
    body.extend_from_slice(&(offset as u32).to_le_bytes());
    body.extend_from_slice(&(length as u32).to_le_bytes());
    body.extend_from_slice(extra);
    config.add_capability(VENDOR_CAPABILITY, &body)
}

fn class_code(device_type: u16) -> u32 {
    match u32::from(device_type) {
        // Mass storage, SCSI (what other virtio-blk implementations report).
        virtio_bindings::virtio_ids::VIRTIO_ID_BLOCK => 0x01_00_00,
        // Network, Ethernet.
        virtio_bindings::virtio_ids::VIRTIO_ID_NET => 0x02_00_00,
        // Display controller, other.
        virtio_bindings::virtio_ids::VIRTIO_ID_GPU => 0x03_80_00,
        _ => 0xff_00_00,
    }
}

impl PciDevice for VirtioPci {
    fn config(&self) -> &ConfigSpace {
        &self.config
    }

    fn config_mut(&mut self) -> &mut ConfigSpace {
        &mut self.config
    }

    fn config_read(&mut self, offset: u16, data: &mut [u8]) {
        if offset == self.pci_cfg_capability + 16 {
            let (bar, bar_offset, length) = self.pci_cfg_window();
            let mut window = [0u8; 4];
            self.bar_read(bar, bar_offset, &mut window[..length]);
            let len = data.len().min(4);
            data[..len].copy_from_slice(&window[..len]);
            return;
        }
        self.config.read(offset, data);
    }

    fn config_write(&mut self, offset: u16, data: &[u8]) {
        self.config.write(offset, data);
        if offset == self.pci_cfg_capability + 16 {
            let (bar, bar_offset, length) = self.pci_cfg_window();
            let len = length.min(data.len());
            self.bar_write(bar, bar_offset, &data[..len]);
        }
        let control = self.msix_capability + 2;
        let end = offset + data.len() as u16;
        if offset < control + 2 && end > control {
            let (enabled, masked) = msix::control(&self.config, self.msix_capability);
            self.msix.lock().unwrap().set_control(enabled, masked);
        }
    }

    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        match bar {
            CONFIG_BAR => self.config_bar_read(offset, data),
            MSIX_BAR => {
                let msix = self.msix.lock().unwrap();
                if offset >= u64::from(MSIX_PBA_OFFSET) {
                    msix.read_pba(offset - u64::from(MSIX_PBA_OFFSET), data);
                } else {
                    msix.read_table(offset, data);
                }
            }
            _ => data.fill(0xff),
        }
    }

    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        match bar {
            CONFIG_BAR => self.config_bar_write(offset, data),
            MSIX_BAR if offset < u64::from(MSIX_PBA_OFFSET) => {
                self.msix.lock().unwrap().write_table(offset, data);
            }
            _ => {}
        }
    }
}

#[derive(Serialize, Deserialize)]
struct VirtioPciState {
    config: Vec<u8>,
    msix: MsixState,
    isr: u8,
    config_vector: u16,
    queue_vectors: Vec<u16>,
    device_feature_select: u32,
    driver_feature_select: u32,
    driver_features: u64,
    status: u8,
    queue_select: u16,
    queues: Vec<QueueConfig>,
    /// While the device runs: where it is in each queue.
    running: Option<Vec<SavedQueue>>,
    /// The device's own state ([`VirtioDevice::save_device`]).
    device: Vec<u8>,
}

impl Snapshot for VirtioPci {
    fn save(&self) -> Vec<u8> {
        snapshot::encode(&VirtioPciState {
            config: self.config.save(),
            msix: self.msix.lock().unwrap().save(),
            isr: self.isr.load(Ordering::SeqCst),
            config_vector: self.vectors.config.load(Ordering::SeqCst),
            queue_vectors: self
                .vectors
                .queues
                .iter()
                .map(|vector| vector.load(Ordering::SeqCst))
                .collect(),
            device_feature_select: self.device_feature_select,
            driver_feature_select: self.driver_feature_select,
            driver_features: self.driver_features,
            status: self.status,
            queue_select: self.queue_select,
            queues: self.queues.clone(),
            running: self.active.then(|| {
                self.device
                    .queue_states()
                    .into_iter()
                    .map(SavedQueue::from)
                    .collect()
            }),
            device: self.device.save_device(),
        })
    }

    fn restore(&mut self, state: &[u8]) -> Result<(), StateError> {
        let state: VirtioPciState = snapshot::decode(state)?;
        if state.queues.len() != self.queues.len()
            || state.queue_vectors.len() != self.vectors.queues.len()
        {
            return Err(StateError(format!(
                "a virtio device with {} queues, not {}",
                state.queues.len(),
                self.queues.len()
            )));
        }
        self.reset();
        self.config.restore(&state.config)?;
        self.msix.lock().unwrap().restore(state.msix)?;
        self.isr.store(state.isr, Ordering::SeqCst);
        self.vectors
            .config
            .store(state.config_vector, Ordering::SeqCst);
        for (slot, vector) in self.vectors.queues.iter().zip(state.queue_vectors) {
            slot.store(vector, Ordering::SeqCst);
        }
        self.device_feature_select = state.device_feature_select;
        self.driver_feature_select = state.driver_feature_select;
        self.driver_features = state.driver_features;
        self.status = state.status;
        self.queue_select = state.queue_select;
        self.queues = state.queues;
        self.device
            .restore_device(&state.device)
            .map_err(StateError)?;
        if let Some(running) = state.running {
            let queues = running
                .iter()
                .map(SavedQueue::queue)
                .collect::<Result<Vec<_>, _>>()
                .map_err(StateError)?;
            self.start(queues).map_err(StateError)?;
        }
        Ok(())
    }

    fn quiesce(&mut self) {
        self.device.quiesce();
    }

    fn resume(&mut self) {
        self.device.resume();
    }
}

impl Drop for VirtioPci {
    fn drop(&mut self) {
        if self.active {
            self.device.reset();
        }
    }
}
