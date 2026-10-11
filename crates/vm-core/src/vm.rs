use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use hv::{
    CpuIo, Hypervisor, MemoryAccess, Partition, PartitionConfig, SpecialRegisters,
    StandardRegisters, Vcpu, VcpuExit, VcpuKicker,
};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion};

use crate::ram::{MapRegions, RamMapper, Shared};
use crate::{Bus, Result};

/// Why a VM stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The guest powered off.
    PowerOff,
    /// The guest asked for a reset.
    Reset,
    /// A vCPU triple-faulted.
    TripleFault,
    /// The VMM hit an error it can't continue from.
    Error,
    /// A device or the host asked to stop, with this code.
    Requested(u32),
}

type Kick = Box<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct StopState {
    reason: OnceLock<StopReason>,
    /// Paired with `stopped` so threads can wait for a stop.
    lock: Mutex<()>,
    stopped: Condvar,
    kickers: Mutex<Vec<Kick>>,
}

/// Lets devices or the host stop a VM. The first request's reason wins.
#[derive(Clone, Default)]
pub struct StopHandle(Arc<StopState>);

impl StopHandle {
    /// Makes every vCPU's [`VmVcpu::run`] return [`VmExit::Stopped`] with `reason`.
    pub fn request_stop(&self, reason: StopReason) {
        if self.0.reason.set(reason).is_ok() {
            drop(self.0.lock.lock().unwrap_or_else(PoisonError::into_inner));
            self.0.stopped.notify_all();
            self.kick_all();
        }
    }

    /// The reason the VM is stopping, once a stop has been requested.
    pub fn reason(&self) -> Option<StopReason> {
        self.0.reason.get().copied()
    }

    /// Blocks until a stop is requested, or `timeout` passes. Returns the reason, if any.
    pub fn wait(&self, timeout: Duration) -> Option<StopReason> {
        let guard = self.0.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let _guard = self
            .0
            .stopped
            .wait_timeout_while(guard, timeout, |_| self.reason().is_none())
            .unwrap_or_else(PoisonError::into_inner);
        self.reason()
    }

    /// Interrupts every running vCPU so it notices a stop request. Callers that must be
    /// sure every vCPU has stopped should repeat this until they have: a kick can land
    /// just before a vCPU enters the guest.
    pub fn kick_all(&self) {
        for kick in self.lock_kickers().iter() {
            kick();
        }
    }

    fn add_kicker(&self, kick: Kick) {
        self.lock_kickers().push(kick);
    }

    fn lock_kickers(&self) -> std::sync::MutexGuard<'_, Vec<Kick>> {
        self.0
            .kickers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl std::fmt::Debug for StopHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("StopHandle").field(&self.reason()).finish()
    }
}

/// Why [`VmVcpu::run`] returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VmExit {
    /// The VM is stopping for this reason.
    Stopped(StopReason),
    /// The guest halted and nothing inside the hypervisor will wake it.
    Halt,
    /// The guest cannot continue, for example after a triple fault.
    Shutdown,
    /// The backend hit an exit it doesn't handle yet.
    Unhandled(String),
}

type EoiHandler = Box<dyn Fn(u8) + Send + Sync>;

/// A VM: its RAM, its hypervisor partition and the buses its devices sit on.
pub struct Vm<P: Partition> {
    // Field order matters: `shared` (the partition) is dropped before `memory`. Devices
    // may hold clones of `memory`, which only keeps the host memory alive longer, and
    // `RamMapper`s, which hold `shared` only weakly.
    shared: Arc<Shared<P>>,
    memory: GuestMemoryMmap,
    pio: Arc<Bus>,
    mmio: Arc<Bus>,
    stop: StopHandle,
    eoi: Option<EoiHandler>,
}

impl<P: Partition> Vm<P> {
    /// Creates a VM with RAM at each `(guest-physical address, size)` range. Ranges must
    /// be sorted, must not overlap, and must be page-aligned.
    pub fn new<H>(hypervisor: &H, config: &PartitionConfig, ram: &[(u64, usize)]) -> Result<Self>
    where
        H: Hypervisor<Partition = P>,
    {
        let ranges: Vec<_> = ram
            .iter()
            .map(|&(gpa, size)| (GuestAddress(gpa), size))
            .collect();
        let memory = GuestMemoryMmap::from_ranges(&ranges)?;
        let partition = hypervisor.create_partition(config)?;
        for region in memory.iter() {
            // SAFETY: the region's host memory is reference-counted by `memory`, which
            // this Vm owns. The partition is dropped before `memory` (field order
            // above), and vCPUs borrow the Vm, so the memory outlives the partition and
            // every vCPU.
            unsafe {
                partition.map_memory(
                    region.start_addr().0,
                    region.as_ptr(),
                    region.len(),
                    MemoryAccess::ALL,
                )?;
            }
        }
        Ok(Self {
            shared: Arc::new(Shared::new(partition)),
            memory,
            pio: Arc::default(),
            mmio: Arc::default(),
            stop: StopHandle::default(),
            eoi: None,
        })
    }

    /// Guest RAM. Clones share the same memory.
    pub fn memory(&self) -> &GuestMemoryMmap {
        &self.memory
    }

    /// Devices reached through I/O ports.
    pub fn pio(&self) -> &Arc<Bus> {
        &self.pio
    }

    /// Devices reached through guest-physical addresses that have no RAM behind them.
    pub fn mmio(&self) -> &Arc<Bus> {
        &self.mmio
    }

    /// A handle that injects interrupts into the guest from any thread.
    pub fn interrupts(&self) -> P::Interrupts {
        self.shared.partition.interrupts()
    }

    /// A handle devices use to map their own host memory into the guest at runtime.
    pub fn ram_mapper(&self) -> RamMapper
    where
        P: 'static,
    {
        let target: Weak<dyn MapRegions> = Arc::downgrade(&self.shared) as Weak<dyn MapRegions>;
        RamMapper::new(target)
    }

    pub fn stop_handle(&self) -> StopHandle {
        self.stop.clone()
    }

    /// Sets what happens when a guest local APIC finishes a level-triggered interrupt.
    pub fn set_eoi_handler(&mut self, handler: impl Fn(u8) + Send + Sync + 'static) {
        self.eoi = Some(Box::new(handler));
    }

    /// Creates the vCPU with the given index. It borrows the VM, so it can't outlive it.
    pub fn create_vcpu(&self, index: u32) -> Result<VmVcpu<'_, P>> {
        let vcpu = self.shared.partition.create_vcpu(index)?;
        let kicker = vcpu.kicker();
        self.stop.add_kicker(Box::new(move || kicker.kick()));
        Ok(VmVcpu { vm: self, vcpu })
    }
}

/// A vCPU of a [`Vm`], wired to the VM's memory and buses.
pub struct VmVcpu<'vm, P: Partition> {
    vm: &'vm Vm<P>,
    // Deliberately never handed out as `&mut P::Vcpu`: swapping vCPUs between VMs would
    // let one outlive the memory its partition maps.
    vcpu: P::Vcpu,
}

impl<P: Partition> VmVcpu<'_, P> {
    pub fn index(&self) -> u32 {
        self.vcpu.index()
    }

    pub fn regs(&self) -> Result<StandardRegisters> {
        Ok(self.vcpu.regs()?)
    }

    pub fn set_regs(&mut self, regs: &StandardRegisters) -> Result<()> {
        Ok(self.vcpu.set_regs(regs)?)
    }

    pub fn sregs(&self) -> Result<SpecialRegisters> {
        Ok(self.vcpu.sregs()?)
    }

    pub fn set_sregs(&mut self, sregs: &SpecialRegisters) -> Result<()> {
        Ok(self.vcpu.set_sregs(sregs)?)
    }

    pub fn set_waiting_for_sipi(&mut self) -> Result<()> {
        Ok(self.vcpu.set_waiting_for_sipi()?)
    }

    pub fn kicker(&self) -> <P::Vcpu as Vcpu>::Kicker {
        self.vcpu.kicker()
    }

    /// Runs the guest, handing port and memory accesses to the VM's memory and buses,
    /// until the guest does something only the caller can handle or a stop is
    /// requested.
    pub fn run(&mut self) -> Result<VmExit> {
        let io = VmIo { vm: self.vm };
        loop {
            if let Some(reason) = self.vm.stop.reason() {
                return Ok(VmExit::Stopped(reason));
            }
            match self.vcpu.run(&io)? {
                // A kick with no stop request behind it just re-enters the guest.
                VcpuExit::Handled | VcpuExit::Canceled => {}
                VcpuExit::Halt => return Ok(VmExit::Halt),
                VcpuExit::Shutdown => return Ok(VmExit::Shutdown),
                VcpuExit::Unhandled(reason) => return Ok(VmExit::Unhandled(reason)),
            }
        }
    }
}

/// Serves a vCPU's accesses from the VM's RAM and buses.
///
/// Reads nobody claims return all-ones, like an empty bus on real hardware. Writes
/// nobody claims are dropped.
struct VmIo<'vm, P: Partition> {
    vm: &'vm Vm<P>,
}

impl<P: Partition> CpuIo for VmIo<'_, P> {
    fn io_read(&self, port: u16, data: &mut [u8]) {
        if !self.vm.pio.read(port.into(), data) {
            tracing::trace!(port, len = data.len(), "unclaimed port read");
            data.fill(0xff);
        }
    }

    fn io_write(&self, port: u16, data: &[u8]) {
        if !self.vm.pio.write(port.into(), data) {
            tracing::trace!(port, ?data, "unclaimed port write");
        }
    }

    fn mem_read(&self, gpa: u64, data: &mut [u8]) {
        if self.vm.memory.read_slice(data, GuestAddress(gpa)).is_ok() {
            return;
        }
        if !self.vm.mmio.read(gpa, data) {
            tracing::debug!(gpa, len = data.len(), "unclaimed MMIO read");
            data.fill(0xff);
        }
    }

    fn mem_write(&self, gpa: u64, data: &[u8]) {
        if self.vm.memory.write_slice(data, GuestAddress(gpa)).is_ok() {
            return;
        }
        if !self.vm.mmio.write(gpa, data) {
            tracing::debug!(gpa, ?data, "unclaimed MMIO write");
        }
    }

    fn eoi(&self, vector: u8) {
        if let Some(handler) = &self.vm.eoi {
            handler(vector);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use hv::{Interrupt, InterruptController};

    use super::*;
    use crate::BusDevice;

    /// One scripted guest action. Each runs in its own call to `FakeVcpu::run`.
    #[derive(Clone)]
    enum Step {
        Out(u16, Vec<u8>),
        In(u16, usize),
        MemWrite(u64, Vec<u8>),
        MemRead(u64, usize),
        Eoi(u8),
        Halt,
    }

    /// What the VMM returned for each `In` and `MemRead`, in order.
    type Reads = Arc<Mutex<Vec<Vec<u8>>>>;

    struct FakeHv;
    struct FakePartition;
    struct FakeVcpu {
        script: VecDeque<Step>,
        reads: Reads,
    }
    #[derive(Clone)]
    struct FakeKicker;
    #[derive(Clone)]
    struct FakeInterrupts;

    impl Hypervisor for FakeHv {
        type Partition = FakePartition;
        fn create_partition(&self, _: &PartitionConfig) -> hv::Result<FakePartition> {
            Ok(FakePartition)
        }
    }

    impl Partition for FakePartition {
        type Vcpu = FakeVcpu;
        type Interrupts = FakeInterrupts;
        unsafe fn map_memory(&self, _: u64, _: *mut u8, _: u64, _: MemoryAccess) -> hv::Result<()> {
            Ok(())
        }
        fn unmap_memory(&self, _: u64, _: u64) -> hv::Result<()> {
            Ok(())
        }
        fn create_vcpu(&self, _: u32) -> hv::Result<FakeVcpu> {
            Ok(FakeVcpu {
                script: VecDeque::new(),
                reads: Reads::default(),
            })
        }
        fn interrupts(&self) -> FakeInterrupts {
            FakeInterrupts
        }
    }

    impl VcpuKicker for FakeKicker {
        fn kick(&self) {}
    }

    impl InterruptController for FakeInterrupts {
        fn request(&self, _: &Interrupt) -> hv::Result<()> {
            Ok(())
        }
    }

    impl Vcpu for FakeVcpu {
        type Kicker = FakeKicker;
        fn index(&self) -> u32 {
            0
        }
        fn regs(&self) -> hv::Result<StandardRegisters> {
            Ok(StandardRegisters::default())
        }
        fn set_regs(&mut self, _: &StandardRegisters) -> hv::Result<()> {
            Ok(())
        }
        fn sregs(&self) -> hv::Result<SpecialRegisters> {
            Ok(SpecialRegisters::default())
        }
        fn set_sregs(&mut self, _: &SpecialRegisters) -> hv::Result<()> {
            Ok(())
        }
        fn set_waiting_for_sipi(&mut self) -> hv::Result<()> {
            Ok(())
        }
        fn run(&mut self, io: &dyn CpuIo) -> hv::Result<VcpuExit> {
            let record = |data: Vec<u8>| self.reads.lock().unwrap().push(data);
            match self.script.pop_front().expect("script ran out") {
                Step::Out(port, data) => io.io_write(port, &data),
                Step::In(port, size) => {
                    let mut data = vec![0; size];
                    io.io_read(port, &mut data);
                    record(data);
                }
                Step::MemWrite(gpa, data) => io.mem_write(gpa, &data),
                Step::MemRead(gpa, size) => {
                    let mut data = vec![0; size];
                    io.mem_read(gpa, &mut data);
                    record(data);
                }
                Step::Eoi(vector) => io.eoi(vector),
                Step::Halt => return Ok(VcpuExit::Halt),
            }
            Ok(VcpuExit::Handled)
        }
        fn kicker(&self) -> FakeKicker {
            FakeKicker
        }
    }

    struct Latch(Vec<u8>);

    impl BusDevice for Latch {
        fn read(&mut self, _: u64, data: &mut [u8]) {
            data.copy_from_slice(&self.0[..data.len()]);
        }
        fn write(&mut self, _: u64, data: &[u8]) {
            self.0 = data.to_vec();
        }
    }

    struct StopOnWrite(StopHandle);

    impl BusDevice for StopOnWrite {
        fn read(&mut self, _: u64, _: &mut [u8]) {}
        fn write(&mut self, _: u64, data: &[u8]) {
            self.0.request_stop(StopReason::Requested(data[0].into()));
        }
    }

    fn vm() -> Vm<FakePartition> {
        Vm::new(&FakeHv, &PartitionConfig::default(), &[(0, 0x1000)]).unwrap()
    }

    fn scripted<'vm>(
        vm: &'vm Vm<FakePartition>,
        script: &[Step],
    ) -> (VmVcpu<'vm, FakePartition>, Reads) {
        let mut vcpu = vm.create_vcpu(0).unwrap();
        vcpu.vcpu.script = script.iter().cloned().collect();
        let reads = vcpu.vcpu.reads.clone();
        (vcpu, reads)
    }

    #[test]
    fn port_accesses_reach_devices_and_unclaimed_reads_float_high() {
        let vm = vm();
        vm.pio()
            .insert(0x80, 2, Arc::new(Mutex::new(Latch(vec![0; 2]))))
            .unwrap();

        let (mut vcpu, reads) = scripted(
            &vm,
            &[
                Step::Out(0x80, vec![0x12, 0x34]),
                Step::In(0x80, 2),
                Step::In(0x1234, 1),
                Step::Halt,
            ],
        );
        assert_eq!(vcpu.run().unwrap(), VmExit::Halt);
        assert_eq!(*reads.lock().unwrap(), [vec![0x12, 0x34], vec![0xff]]);
    }

    #[test]
    fn memory_accesses_hit_ram_first_then_mmio() {
        let vm = vm();
        vm.mmio()
            .insert(0xd_0000, 0x1000, Arc::new(Mutex::new(Latch(vec![0; 4]))))
            .unwrap();

        let (mut vcpu, reads) = scripted(
            &vm,
            &[
                Step::MemWrite(0x10, vec![1, 2, 3, 4]),
                Step::MemWrite(0xd_0000, vec![5, 6]),
                Step::MemRead(0xd_0000, 2),
                Step::MemRead(0xe_0000, 4),
                Step::Halt,
            ],
        );
        assert_eq!(vcpu.run().unwrap(), VmExit::Halt);

        let mut ram = [0u8; 4];
        vm.memory()
            .read_slice(&mut ram, GuestAddress(0x10))
            .unwrap();
        assert_eq!(ram, [1, 2, 3, 4], "RAM writes land in guest memory");
        assert_eq!(
            *reads.lock().unwrap(),
            [vec![5, 6], vec![0xff; 4]],
            "MMIO goes to the bus; unclaimed reads float high"
        );
    }

    #[test]
    fn a_device_can_stop_the_vm() {
        let vm = vm();
        vm.pio()
            .insert(0xf4, 1, Arc::new(Mutex::new(StopOnWrite(vm.stop_handle()))))
            .unwrap();

        let (mut vcpu, _) = scripted(&vm, &[Step::Out(0xf4, vec![3]), Step::Halt]);
        assert_eq!(
            vcpu.run().unwrap(),
            VmExit::Stopped(StopReason::Requested(3))
        );
        assert_eq!(vm.stop_handle().reason(), Some(StopReason::Requested(3)));
    }

    #[test]
    fn the_first_stop_reason_wins() {
        let stop = StopHandle::default();
        stop.request_stop(StopReason::PowerOff);
        stop.request_stop(StopReason::Reset);
        assert_eq!(stop.reason(), Some(StopReason::PowerOff));
    }

    #[test]
    fn wait_returns_once_a_stop_is_requested() {
        let stop = StopHandle::default();
        assert_eq!(stop.wait(Duration::from_millis(10)), None, "times out");
        let other = stop.clone();
        let waiter = std::thread::spawn(move || other.wait(Duration::from_secs(10)));
        std::thread::sleep(Duration::from_millis(20));
        stop.request_stop(StopReason::Reset);
        assert_eq!(waiter.join().unwrap(), Some(StopReason::Reset));
    }

    #[test]
    fn eois_reach_the_handler() {
        let mut vm = vm();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        vm.set_eoi_handler(move |vector| sink.lock().unwrap().push(vector));

        let (mut vcpu, _) = scripted(&vm, &[Step::Eoi(0x31), Step::Eoi(0x40), Step::Halt]);
        assert_eq!(vcpu.run().unwrap(), VmExit::Halt);
        assert_eq!(*seen.lock().unwrap(), [0x31, 0x40]);
    }

    #[test]
    fn ram_mapper_tracks_mappings_and_outliving_the_vm_is_safe() {
        use crate::{Error, RamRegion};
        let vm = vm();
        let mapper = vm.ram_mapper();
        let flash = RamRegion::new(0x2000).unwrap();
        mapper
            .map(0xffc0_0000, flash.clone(), MemoryAccess::READ)
            .unwrap();
        assert!(
            mapper
                .map(
                    0xffc0_1000,
                    RamRegion::new(0x1000).unwrap(),
                    MemoryAccess::READ
                )
                .is_err(),
            "overlaps"
        );
        mapper.unmap(0xffc0_0000).unwrap();
        mapper
            .map(0xffc0_0000, flash, MemoryAccess::ALL)
            .expect("remapping after unmap works");

        drop(vm);
        assert!(matches!(
            mapper.map(0x1000, RamRegion::new(0x1000).unwrap(), MemoryAccess::READ),
            Err(Error::VmGone)
        ));
    }

    #[test]
    fn guest_memory_is_usable_on_this_host() {
        let vm = Vm::new(&FakeHv, &PartitionConfig::default(), &[(0, 0x20_0000)]).unwrap();
        vm.memory()
            .write_slice(b"vmmc", GuestAddress(0x1f_fffc))
            .unwrap();
        let mut back = [0u8; 4];
        vm.memory()
            .read_slice(&mut back, GuestAddress(0x1f_fffc))
            .unwrap();
        assert_eq!(&back, b"vmmc");
        assert!(
            vm.memory()
                .write_slice(b"x", GuestAddress(0x20_0000))
                .is_err()
        );
    }
}
