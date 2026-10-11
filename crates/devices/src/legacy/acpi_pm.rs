//! ACPI fixed-hardware power management registers: PM1a event and control blocks and
//! the PM timer, at ICH9's usual I/O base.

use std::sync::Arc;

use vm_core::BusDevice;

use crate::clock::Clock;
use crate::irq::IrqLine;
use crate::{PowerEvent, PowerHandler};

/// I/O base of the PM block, as on Q35/ICH9.
pub const PM_BASE: u16 = 0x600;
pub const PM_PORTS: u16 = 0x0c;
/// PM1a_EVT_BLK: status (2 bytes) then enable (2 bytes).
pub const PM1A_EVT: u16 = PM_BASE;
pub const PM1A_EVT_LEN: u8 = 4;
/// PM1a_CNT_BLK.
pub const PM1A_CNT: u16 = PM_BASE + 4;
pub const PM1A_CNT_LEN: u8 = 2;
/// PM_TMR_BLK: 24-bit free-running counter.
pub const PM_TMR: u16 = PM_BASE + 8;
pub const PM_TMR_LEN: u8 = 4;
/// SLP_TYP value the DSDT's `_S5` object gives for soft-off.
pub const S5_SLEEP_TYPE: u16 = 5;
/// PM timer frequency in Hz.
pub const PM_TIMER_HZ: u128 = 3_579_545;

// PM1 status bits; the enable register uses the same positions.
const STS_TMR: u16 = 1 << 0;
const STS_GBL: u16 = 1 << 5;
const STS_PWRBTN: u16 = 1 << 8;
const STS_RTC: u16 = 1 << 10;
const STS_WAK: u16 = 1 << 15;
/// Status bits the guest clears by writing 1.
const STS_CLEARABLE: u16 = STS_TMR | STS_GBL | STS_PWRBTN | STS_RTC | STS_WAK;
/// Events the guest can enable. ACPI checks each fixed event it uses by writing its
/// enable bit and reading it back, so these must stick even though only the power
/// button ever fires.
const EN_WRITABLE: u16 = STS_TMR | STS_GBL | STS_PWRBTN | STS_RTC;
const CNT_SCI_EN: u16 = 1 << 0;
const CNT_SLP_EN: u16 = 1 << 13;

pub struct AcpiPm {
    status: u16,
    enable: u16,
    control: u16,
    clock: Arc<dyn Clock>,
    sci: IrqLine,
    power: PowerHandler,
}

impl AcpiPm {
    pub fn new(clock: Arc<dyn Clock>, sci: IrqLine, power: PowerHandler) -> Self {
        Self {
            status: 0,
            enable: 0,
            // The platform is always in ACPI mode (FADT SMI_CMD is 0).
            control: CNT_SCI_EN,
            clock,
            sci,
            power,
        }
    }

    /// Presses the power button. A guest with ACPI support shuts down cleanly.
    pub fn press_power_button(&mut self) {
        self.status |= STS_PWRBTN;
        self.update_sci();
    }

    fn update_sci(&self) {
        self.sci.set_level(self.status & self.enable != 0);
    }

    fn timer(&self) -> u32 {
        (self.clock.now().as_nanos() * PM_TIMER_HZ / 1_000_000_000) as u32 & 0x00ff_ffff
    }

    fn register(&self, offset: u64) -> u32 {
        match offset {
            0..=1 => u32::from(self.status),
            2..=3 => u32::from(self.enable),
            4..=5 => u32::from(self.control),
            8..=11 => self.timer(),
            _ => 0,
        }
    }

    fn write_control(&mut self, value: u16) {
        if value & CNT_SLP_EN != 0 {
            let sleep_type = (value >> 10) & 7;
            if sleep_type == S5_SLEEP_TYPE {
                (self.power)(PowerEvent::PowerOff);
            } else {
                tracing::warn!(sleep_type, "guest asked for an unsupported sleep state");
            }
        }
        // SLP_EN reads as 0, and SCI_EN stays set.
        self.control = (value & !CNT_SLP_EN) | CNT_SCI_EN;
    }
}

impl BusDevice for AcpiPm {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let base = offset & !1;
        let base = if offset >= 8 { 8 } else { base };
        let bytes = self.register(offset).to_le_bytes();
        let start = (offset - base) as usize;
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = bytes.get(start + i).copied().unwrap_or(0);
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut bytes = [0u8; 2];
        let len = data.len().min(2);
        bytes[..len].copy_from_slice(&data[..len]);
        let value = u16::from_le_bytes(bytes);
        match offset {
            // Write 1 to clear.
            0 => {
                self.status &= !(value & STS_CLEARABLE);
                self.update_sci();
            }
            2 => {
                self.enable = value & EN_WRITABLE;
                self.update_sci();
            }
            4 => self.write_control(value),
            _ => tracing::debug!(offset, ?data, "ACPI PM write ignored"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::clock::FakeClock;
    use crate::irq::testing::RecordingSink;
    use crate::legacy::ioapic::IoApic;

    struct Fixture {
        pm: AcpiPm,
        clock: Arc<FakeClock>,
        events: Arc<Mutex<Vec<PowerEvent>>>,
        ioapic: Arc<Mutex<IoApic>>,
        sink: Arc<RecordingSink>,
    }

    fn fixture() -> Fixture {
        let clock = Arc::new(FakeClock::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(RecordingSink::default());
        let ioapic = Arc::new(Mutex::new(IoApic::new(0, sink.clone())));
        let power: PowerHandler = Arc::new({
            let events = events.clone();
            move |event| events.lock().unwrap().push(event)
        });
        let pm = AcpiPm::new(clock.clone(), IrqLine::new(ioapic.clone(), 9), power);
        Fixture {
            pm,
            clock,
            events,
            ioapic,
            sink,
        }
    }

    fn read_u32(pm: &mut AcpiPm, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        pm.read(offset, &mut data);
        u32::from_le_bytes(data)
    }

    #[test]
    fn s5_powers_off_other_sleep_states_are_ignored() {
        let mut f = fixture();
        f.pm.write(4, &((3 << 10) | CNT_SLP_EN).to_le_bytes());
        assert!(f.events.lock().unwrap().is_empty());
        f.pm.write(4, &((S5_SLEEP_TYPE << 10) | CNT_SLP_EN).to_le_bytes());
        assert_eq!(*f.events.lock().unwrap(), [PowerEvent::PowerOff]);
        assert_eq!(
            read_u32(&mut f.pm, 4) as u16 & CNT_SLP_EN,
            0,
            "SLP_EN reads 0"
        );
    }

    #[test]
    fn fixed_event_enables_read_back() {
        // Linux's ACPI core checks the global lock and RTC events exist this way.
        let mut f = fixture();
        f.pm.write(2, &(STS_GBL | STS_RTC).to_le_bytes());
        assert_eq!(read_u32(&mut f.pm, 2) as u16, STS_GBL | STS_RTC);
        f.pm.write(2, &0xffffu16.to_le_bytes());
        assert_eq!(read_u32(&mut f.pm, 2) as u16, EN_WRITABLE);
    }

    #[test]
    fn sci_enable_is_always_on() {
        let mut f = fixture();
        f.pm.write(4, &0u16.to_le_bytes());
        assert_eq!(read_u32(&mut f.pm, 4) as u16 & CNT_SCI_EN, CNT_SCI_EN);
    }

    #[test]
    fn pm_timer_runs_at_3_58_mhz_and_wraps_at_24_bits() {
        let mut f = fixture();
        f.clock.advance(Duration::from_millis(1));
        assert_eq!(read_u32(&mut f.pm, 8), 3579);
        f.clock.advance(Duration::from_secs(5));
        assert_eq!(
            read_u32(&mut f.pm, 8),
            (5_001u64 * 3_579_545 / 1000) as u32 & 0xff_ffff
        );
    }

    #[test]
    fn power_button_raises_sci_when_enabled_and_clears_on_ack() {
        let mut f = fixture();
        // Route pin 9 level-triggered to vector 0x29.
        {
            let mut ioapic = f.ioapic.lock().unwrap();
            ioapic.write(0, &(0x10u32 + 18).to_le_bytes());
            ioapic.write(0x10, &(0x8000u32 | 0x29).to_le_bytes());
        }
        f.pm.press_power_button();
        assert!(f.sink.take().is_empty(), "not enabled yet");
        f.pm.write(2, &STS_PWRBTN.to_le_bytes());
        assert_eq!(f.sink.take().len(), 1);
        assert_eq!(read_u32(&mut f.pm, 0) as u16 & STS_PWRBTN, STS_PWRBTN);

        f.pm.write(0, &STS_PWRBTN.to_le_bytes());
        assert_eq!(
            read_u32(&mut f.pm, 0) as u16 & STS_PWRBTN,
            0,
            "write 1 to clear"
        );
        f.ioapic.lock().unwrap().end_of_interrupt(0x29);
        assert!(f.sink.take().is_empty(), "SCI dropped");
    }
}
