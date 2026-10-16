//! MC146818 real-time clock and CMOS RAM.
//!
//! Time comes from the host's wall clock plus an offset the guest sets by writing the
//! time registers. The periodic interrupt drives IRQ 8: Windows builds its system clock
//! on it when there's no HPET. Register C's update-ended and periodic flags are right
//! for guests that poll them, but alarm and update-ended interrupts aren't emulated.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use vm_core::BusDevice;

use crate::clock::Clock;
use crate::irq::IrqLine;
use crate::snapshot::{self, Snapshot, StateError};
use crate::timer::{Ticker, TimerMode};

/// Index and data ports.
pub const RTC_BASE: u16 = 0x70;
pub const RTC_PORTS: u16 = 2;
/// The ISA interrupt the RTC raises.
pub const RTC_IRQ: u8 = 8;

const SECONDS: usize = 0x00;
const MINUTES: usize = 0x02;
const HOURS: usize = 0x04;
const DAY_OF_WEEK: usize = 0x06;
const DAY_OF_MONTH: usize = 0x07;
const MONTH: usize = 0x08;
const YEAR: usize = 0x09;
const REG_A: usize = 0x0a;
const REG_B: usize = 0x0b;
const REG_C: usize = 0x0c;
const REG_D: usize = 0x0d;
/// The IBM PC/AT century byte.
const CENTURY: usize = 0x32;

const A_UIP: u8 = 0x80;
/// Register A's divider bits; 010 runs the chain from a 32.768 kHz time base.
const A_DIVIDER: u8 = 0x70;
const A_DIVIDER_NORMAL: u8 = 0x20;
const A_RATE: u8 = 0x0f;
const B_SET: u8 = 0x80;
const B_PERIODIC: u8 = 0x40;
/// Periodic, alarm and update-ended interrupt enables. They line up with their flags
/// in register C.
const B_INTERRUPTS: u8 = 0x70;
const C_IRQF: u8 = 0x80;
const C_PERIODIC: u8 = 0x40;
const C_UPDATE: u8 = 0x10;
const B_BINARY: u8 = 0x04;
const B_24_HOUR: u8 = 0x02;
const D_VALID_RAM: u8 = 0x80;
const PM: u8 = 0x80;

/// The update cycle runs in the last 244 µs of each second; UIP is set during it.
const UIP_WINDOW: Duration = Duration::from_micros(244);

const SECONDS_PER_DAY: i64 = 86_400;

/// Register C and IRQ 8, shared between the RTC and the timer thread that raises its
/// periodic interrupt.
pub struct RtcInterrupt {
    flags: AtomicU8,
    /// Register B's interrupt enables.
    enables: AtomicU8,
    irq: IrqLine,
}

impl RtcInterrupt {
    pub fn new(irq: IrqLine) -> Arc<Self> {
        Arc::new(Self {
            flags: AtomicU8::new(0),
            enables: AtomicU8::new(0),
            irq,
        })
    }

    /// One period of the periodic interrupt has passed. The RTC's ticker calls this.
    pub fn periodic_tick(&self) {
        self.raise(C_PERIODIC);
    }

    fn set_enables(&self, enables: u8) {
        self.enables
            .store(enables & B_INTERRUPTS, Ordering::Release);
        // A flag that is already up interrupts as soon as it's enabled.
        self.raise(0);
    }

    /// Sets `flags` in register C. An enabled flag sets IRQF, and IRQF going up sends an
    /// edge to the IOAPIC: there are no more until the guest reads register C, as on the
    /// real part, whose interrupt output stays asserted until then.
    fn raise(&self, flags: u8) {
        let enables = self.enables.load(Ordering::Acquire);
        let mut edge = false;
        let _ = self
            .flags
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                let mut new = old | flags;
                if new & enables != 0 {
                    new |= C_IRQF;
                }
                edge = old & C_IRQF == 0 && new & C_IRQF != 0;
                Some(new)
            });
        if edge {
            self.irq.pulse();
        }
    }

    /// Reads and clears register C.
    fn take(&self) -> u8 {
        self.flags.swap(0, Ordering::AcqRel)
    }
}

/// The periodic interrupt's period for a register A value, or `None` when it's off: no
/// rate selected, or the divider isn't running from a 32.768 kHz time base.
fn periodic_period(reg_a: u8) -> Option<Duration> {
    if reg_a & A_DIVIDER != A_DIVIDER_NORMAL {
        return None;
    }
    // Rates 1 and 2 are 256 and 128 Hz; from 3 on, 32768 >> (rate - 1) Hz.
    let divisor: u64 = match reg_a & A_RATE {
        0 => return None,
        1 => 128,
        2 => 256,
        rate => 1 << (rate - 1),
    };
    Some(Duration::from_nanos(1_000_000_000 * divisor / 32_768))
}

pub struct Rtc {
    index: usize,
    ram: [u8; 128],
    clock: Arc<dyn Clock>,
    /// Guest time minus host time, in seconds.
    offset: i64,
    /// While register B's SET bit is on: the time registers as the guest is writing them.
    setting: Option<i64>,
    interrupt: Arc<RtcInterrupt>,
    /// Fires the periodic interrupt while it's enabled.
    periodic: Box<dyn Ticker>,
    /// The period `periodic` runs at, if it runs.
    programmed: Option<Duration>,
    /// The guest's second, and the host's monotonic time, when register C was last
    /// read: its flags for updates and periods that passed since are derived from them.
    c_read_second: i64,
    c_read_at: Duration,
}

#[derive(Serialize, Deserialize)]
struct RtcState {
    index: usize,
    ram: Vec<u8>,
    offset: i64,
    setting: Option<i64>,
    flags: u8,
    enables: u8,
    c_read_second: i64,
    c_read_at: Duration,
}

impl Snapshot for Rtc {
    fn save(&self) -> Vec<u8> {
        snapshot::encode(&RtcState {
            index: self.index,
            ram: self.ram.to_vec(),
            offset: self.offset,
            setting: self.setting,
            flags: self.interrupt.flags.load(Ordering::Acquire),
            enables: self.interrupt.enables.load(Ordering::Acquire),
            c_read_second: self.c_read_second,
            c_read_at: self.c_read_at,
        })
    }

    /// The guest's clock goes on from the host's: it stays as far from it as it was,
    /// and so, like a real RTC, it counts the time the VM was saved.
    fn restore(&mut self, state: &[u8]) -> Result<(), StateError> {
        let state: RtcState = snapshot::decode(state)?;
        self.index = state.index & 0x7f;
        self.ram = state
            .ram
            .try_into()
            .map_err(|_| StateError("the CMOS RAM was a different size".into()))?;
        self.offset = state.offset;
        self.setting = state.setting;
        self.interrupt.flags.store(state.flags, Ordering::Release);
        self.interrupt
            .enables
            .store(state.enables, Ordering::Release);
        self.c_read_second = state.c_read_second;
        self.c_read_at = state.c_read_at;
        self.programmed = None;
        self.reprogram();
        Ok(())
    }
}

impl Rtc {
    /// An RTC that raises its periodic interrupt through `interrupt`, timed by
    /// `periodic`, which must call [`RtcInterrupt::periodic_tick`] when it fires.
    pub fn new(
        clock: Arc<dyn Clock>,
        interrupt: Arc<RtcInterrupt>,
        periodic: Box<dyn Ticker>,
    ) -> Self {
        let mut ram = [0u8; 128];
        ram[REG_A] = 0x26; // 32.768 kHz time base, 1024 Hz rate
        ram[REG_B] = B_24_HOUR;
        ram[REG_D] = D_VALID_RAM;
        let c_read_second = clock.wall_time().as_secs() as i64;
        let c_read_at = clock.now();
        Self {
            index: 0,
            ram,
            clock,
            offset: 0,
            setting: None,
            interrupt,
            periodic,
            programmed: None,
            c_read_second,
            c_read_at,
        }
    }

    /// Runs the periodic interrupt's timer at the rate register A selects while
    /// register B enables it.
    fn reprogram(&mut self) {
        let period = if self.ram[REG_B] & B_PERIODIC != 0 {
            periodic_period(self.ram[REG_A])
        } else {
            None
        };
        if period != self.programmed {
            self.programmed = period;
            self.periodic.program(period.map(TimerMode::Periodic));
        }
    }

    /// Register C: the flags raised since it was last read, which reading clears.
    fn read_flags(&mut self) -> u8 {
        let second = self.guest_seconds();
        let now = self.clock.now();
        let mut derived = 0;
        if second != self.c_read_second {
            derived |= C_UPDATE;
        }
        // Without the timer running, work out whether a period passed.
        if self.programmed.is_none()
            && periodic_period(self.ram[REG_A])
                .is_some_and(|period| now.saturating_sub(self.c_read_at) >= period)
        {
            derived |= C_PERIODIC;
        }
        self.c_read_second = second;
        self.c_read_at = now;
        self.interrupt.take() | derived
    }

    /// Stores a byte of CMOS RAM, for platform code that passes information to
    /// firmware this way.
    pub fn set_cmos_byte(&mut self, index: u8, value: u8) {
        self.ram[usize::from(index & 0x7f)] = value;
    }

    fn guest_seconds(&self) -> i64 {
        self.clock.wall_time().as_secs() as i64 + self.offset
    }

    fn binary(&self) -> bool {
        self.ram[REG_B] & B_BINARY != 0
    }

    fn encode(&self, value: u8) -> u8 {
        if self.binary() { value } else { to_bcd(value) }
    }

    fn decode(&self, value: u8) -> u8 {
        if self.binary() {
            value
        } else {
            from_bcd(value)
        }
    }

    fn read_register(&mut self, index: usize) -> u8 {
        match index {
            SECONDS | MINUTES | HOURS | DAY_OF_WEEK | DAY_OF_MONTH | MONTH | YEAR | CENTURY => {
                let seconds = self.setting.unwrap_or_else(|| self.guest_seconds());
                self.time_field(index, seconds)
            }
            REG_A => {
                let subsecond =
                    Duration::from_nanos(u64::from(self.clock.wall_time().subsec_nanos()));
                let uip = if subsecond >= Duration::from_secs(1) - UIP_WINDOW {
                    A_UIP
                } else {
                    0
                };
                (self.ram[REG_A] & !A_UIP) | uip
            }
            REG_C => self.read_flags(),
            _ => self.ram[index],
        }
    }

    fn time_field(&self, index: usize, seconds: i64) -> u8 {
        let days = seconds.div_euclid(SECONDS_PER_DAY);
        let of_day = seconds.rem_euclid(SECONDS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        let value = match index {
            SECONDS => (of_day % 60) as u8,
            MINUTES => (of_day / 60 % 60) as u8,
            HOURS => {
                let hour = (of_day / 3600) as u8;
                if self.ram[REG_B] & B_24_HOUR != 0 {
                    hour
                } else {
                    // 12-hour mode: 12, 1..11, with the PM flag in bit 7.
                    let twelve = if hour.is_multiple_of(12) {
                        12
                    } else {
                        hour % 12
                    };
                    return self.encode(twelve) | if hour >= 12 { PM } else { 0 };
                }
            }
            // 1 = Sunday; 1970-01-01 was a Thursday.
            DAY_OF_WEEK => ((days + 4).rem_euclid(7) + 1) as u8,
            DAY_OF_MONTH => day,
            MONTH => month,
            YEAR => (year % 100) as u8,
            CENTURY => (year / 100) as u8,
            _ => unreachable!(),
        };
        self.encode(value)
    }

    /// Applies a guest write to a time register while SET is on.
    fn write_time_field(&mut self, index: usize, value: u8) {
        let Some(seconds) = self.setting else { return };
        let days = seconds.div_euclid(SECONDS_PER_DAY);
        let of_day = seconds.rem_euclid(SECONDS_PER_DAY);
        let (mut year, mut month, mut day) = civil_from_days(days);
        let (mut hour, mut minute, mut second) = (of_day / 3600, of_day / 60 % 60, of_day % 60);
        match index {
            SECONDS => second = i64::from(self.decode(value)),
            MINUTES => minute = i64::from(self.decode(value)),
            HOURS => {
                hour = if self.ram[REG_B] & B_24_HOUR != 0 {
                    i64::from(self.decode(value))
                } else {
                    let twelve = i64::from(self.decode(value & !PM)) % 12;
                    twelve + if value & PM != 0 { 12 } else { 0 }
                }
            }
            DAY_OF_MONTH => day = self.decode(value),
            MONTH => month = self.decode(value),
            YEAR => year = year / 100 * 100 + i64::from(self.decode(value)),
            CENTURY => year = i64::from(self.decode(value)) * 100 + year % 100,
            _ => return,
        }
        self.setting = Some(
            days_from_civil(year, month, day) * SECONDS_PER_DAY
                + hour * 3600
                + minute * 60
                + second,
        );
    }

    fn write_register(&mut self, index: usize, value: u8) {
        match index {
            SECONDS | MINUTES | HOURS | DAY_OF_MONTH | MONTH | YEAR | CENTURY => {
                self.write_time_field(index, value);
            }
            // Computed from the date.
            DAY_OF_WEEK => {}
            REG_A => {
                self.ram[REG_A] = value & !A_UIP;
                self.reprogram();
            }
            REG_B => {
                let was_setting = self.setting.is_some();
                if value & B_SET != 0 && !was_setting {
                    self.setting = Some(self.guest_seconds());
                } else if value & B_SET == 0
                    && let Some(seconds) = self.setting.take()
                {
                    self.offset = seconds - self.clock.wall_time().as_secs() as i64;
                }
                if value & B_INTERRUPTS & !B_PERIODIC != 0 {
                    tracing::debug!(value, "RTC alarm or update interrupts aren't emulated");
                }
                self.ram[REG_B] = value;
                self.interrupt.set_enables(value);
                self.reprogram();
            }
            REG_C | REG_D => {}
            _ => self.ram[index] = value,
        }
    }
}

impl BusDevice for Rtc {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let value = if offset == 0 {
            0xff
        } else {
            self.read_register(self.index)
        };
        data.fill(value);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if offset == 0 {
            // Bit 7 masks NMI; it isn't part of the index.
            self.index = usize::from(data[0] & 0x7f);
        } else {
            self.write_register(self.index, data[0]);
        }
    }
}

fn to_bcd(value: u8) -> u8 {
    ((value / 10) << 4) | (value % 10)
}

fn from_bcd(value: u8) -> u8 {
    (value >> 4) * 10 + (value & 0x0f)
}

/// (year, month, day) for a day count since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u8, u8) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Day count since 1970-01-01 for a date (inverse of `civil_from_days`).
fn days_from_civil(year: i64, month: u8, day: u8) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let month = i64::from(month);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::clock::FakeClock;
    use crate::irq::testing::RecordingSink;
    use crate::legacy::ioapic::IoApic;
    use crate::timer::testing::RecordingTicker;

    /// 2026-10-10 13:45:30 UTC.
    const WALL: u64 = 1_791_639_930;

    struct Fixture {
        rtc: Rtc,
        clock: Arc<FakeClock>,
        interrupt: Arc<RtcInterrupt>,
        ticker: RecordingTicker,
        sink: Arc<RecordingSink>,
    }

    fn fixture() -> Fixture {
        let clock = Arc::new(FakeClock::new(Duration::from_secs(WALL)));
        let sink = Arc::new(RecordingSink::default());
        let ioapic = Arc::new(Mutex::new(IoApic::new(0, sink.clone())));
        {
            // Pin 8: edge-triggered, unmasked, vector 0x38.
            let mut ioapic = ioapic.lock().unwrap();
            ioapic.write(0, &(0x10u32 + 2 * u32::from(RTC_IRQ)).to_le_bytes());
            ioapic.write(0x10, &0x38u32.to_le_bytes());
        }
        let interrupt = RtcInterrupt::new(IrqLine::new(ioapic, RTC_IRQ));
        let ticker = RecordingTicker::default();
        let rtc = Rtc::new(clock.clone(), interrupt.clone(), Box::new(ticker.clone()));
        Fixture {
            rtc,
            clock,
            interrupt,
            ticker,
            sink,
        }
    }

    fn rtc() -> (Rtc, Arc<FakeClock>) {
        let f = fixture();
        (f.rtc, f.clock)
    }

    fn read(rtc: &mut Rtc, index: u8) -> u8 {
        rtc.write(0, &[index]);
        let mut data = [0];
        rtc.read(1, &mut data);
        data[0]
    }

    fn write(rtc: &mut Rtc, index: u8, value: u8) {
        rtc.write(0, &[index]);
        rtc.write(1, &[value]);
    }

    #[test]
    fn a_restored_rtc_keeps_the_guests_time_and_periodic_interrupt() {
        use crate::snapshot::Snapshot;
        let mut f = fixture();
        // The guest sets the clock an hour ahead, in binary, and turns on 64 Hz.
        write(&mut f.rtc, REG_B as u8, B_SET | B_24_HOUR | B_BINARY);
        let hour = read(&mut f.rtc, HOURS as u8);
        write(&mut f.rtc, HOURS as u8, (hour + 1) % 24);
        write(&mut f.rtc, REG_B as u8, B_24_HOUR | B_BINARY | B_PERIODIC);
        write(&mut f.rtc, REG_A as u8, 0x2a);
        write(&mut f.rtc, 0x40, 0x5a);
        let state = f.rtc.save();

        let mut g = fixture();
        g.rtc.restore(&state).unwrap();
        assert_eq!(read(&mut g.rtc, HOURS as u8), (hour + 1) % 24);
        assert_eq!(read(&mut g.rtc, 0x40), 0x5a);
        assert_eq!(
            g.ticker.last(),
            Some(Some(TimerMode::Periodic(Duration::from_nanos(
                1_000_000_000 / 64
            ))))
        );
    }

    #[test]
    fn calendar_conversion_round_trips() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_736), (2026, 10, 10));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        for days in [-1, 0, 59, 365, 11_016, 20_736, 50_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }

    #[test]
    fn reads_host_time_in_bcd_24_hour() {
        let (mut rtc, _) = rtc();
        assert_eq!(read(&mut rtc, SECONDS as u8), 0x30);
        assert_eq!(read(&mut rtc, MINUTES as u8), 0x45);
        assert_eq!(read(&mut rtc, HOURS as u8), 0x13);
        assert_eq!(read(&mut rtc, DAY_OF_MONTH as u8), 0x10);
        assert_eq!(read(&mut rtc, MONTH as u8), 0x10);
        assert_eq!(read(&mut rtc, YEAR as u8), 0x26);
        assert_eq!(read(&mut rtc, CENTURY as u8), 0x20);
        assert_eq!(read(&mut rtc, DAY_OF_WEEK as u8), 7, "Saturday");
        assert_eq!(read(&mut rtc, REG_D as u8), D_VALID_RAM);
    }

    #[test]
    fn binary_and_12_hour_modes() {
        let (mut rtc, _) = rtc();
        write(&mut rtc, REG_B as u8, B_BINARY);
        assert_eq!(read(&mut rtc, HOURS as u8), 1 | PM, "13:00 is 1 PM");
        assert_eq!(read(&mut rtc, MINUTES as u8), 45);
    }

    #[test]
    fn guest_can_set_the_time() {
        let (mut rtc, clock) = rtc();
        write(&mut rtc, REG_B as u8, B_SET | B_24_HOUR);
        write(&mut rtc, HOURS as u8, 0x08);
        write(&mut rtc, YEAR as u8, 0x30);
        write(&mut rtc, REG_B as u8, B_24_HOUR);

        assert_eq!(read(&mut rtc, HOURS as u8), 0x08);
        assert_eq!(read(&mut rtc, YEAR as u8), 0x30);
        clock.advance(Duration::from_secs(60));
        assert_eq!(
            read(&mut rtc, MINUTES as u8),
            0x46,
            "keeps running from the new time"
        );
    }

    #[test]
    fn update_in_progress_only_at_the_end_of_a_second() {
        let (mut rtc, clock) = rtc();
        assert_eq!(read(&mut rtc, REG_A as u8) & A_UIP, 0);
        clock.advance(Duration::from_micros(999_900));
        assert_ne!(read(&mut rtc, REG_A as u8) & A_UIP, 0);
    }

    #[test]
    fn periodic_rates() {
        assert_eq!(periodic_period(0x26), Some(Duration::from_nanos(976_562)));
        assert_eq!(periodic_period(0x23), Some(Duration::from_nanos(122_070)));
        assert_eq!(periodic_period(0x21), Some(Duration::from_nanos(3_906_250)));
        assert_eq!(periodic_period(0x22), Some(Duration::from_nanos(7_812_500)));
        assert_eq!(periodic_period(0x2f), Some(Duration::from_millis(500)));
        assert_eq!(periodic_period(0x20), None, "no rate");
        assert_eq!(periodic_period(0x06), None, "divider stopped");
    }

    #[test]
    fn periodic_interrupt_runs_while_enabled() {
        let mut f = fixture();
        assert_eq!(f.ticker.last(), None, "off at reset");
        write(&mut f.rtc, REG_B as u8, B_24_HOUR | B_PERIODIC);
        let period = Duration::from_nanos(976_562);
        assert_eq!(f.ticker.last(), Some(Some(TimerMode::Periodic(period))));
        // Rewriting the same setup doesn't restart the timer.
        write(&mut f.rtc, REG_A as u8, 0x26);
        assert_eq!(f.ticker.0.lock().unwrap().len(), 1);
        write(&mut f.rtc, REG_A as u8, 0x2f);
        assert_eq!(
            f.ticker.last(),
            Some(Some(TimerMode::Periodic(Duration::from_millis(500))))
        );
        write(&mut f.rtc, REG_B as u8, B_24_HOUR);
        assert_eq!(f.ticker.last(), Some(None));
    }

    #[test]
    fn one_interrupt_until_register_c_is_read() {
        let mut f = fixture();
        write(&mut f.rtc, REG_B as u8, B_24_HOUR | B_PERIODIC);
        f.interrupt.periodic_tick();
        f.interrupt.periodic_tick();
        let sent = f.sink.take();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].vector(), 0x38);
        assert_eq!(read(&mut f.rtc, REG_C as u8), C_IRQF | C_PERIODIC);
        assert_eq!(read(&mut f.rtc, REG_C as u8), 0, "reading clears it");
        f.interrupt.periodic_tick();
        assert_eq!(f.sink.take().len(), 1);
    }

    #[test]
    fn pending_flag_interrupts_once_enabled() {
        let mut f = fixture();
        f.interrupt.periodic_tick();
        assert!(f.sink.take().is_empty(), "not enabled");
        write(&mut f.rtc, REG_B as u8, B_24_HOUR | B_PERIODIC);
        assert_eq!(f.sink.take().len(), 1);
    }

    #[test]
    fn polled_flags_follow_the_clock() {
        let mut f = fixture();
        assert_eq!(read(&mut f.rtc, REG_C as u8), 0);
        f.clock.advance(Duration::from_millis(2));
        assert_eq!(
            read(&mut f.rtc, REG_C as u8),
            C_PERIODIC,
            "1024 Hz period passed"
        );
        f.clock.advance(Duration::from_secs(1));
        assert_eq!(read(&mut f.rtc, REG_C as u8) & C_UPDATE, C_UPDATE);
        assert!(f.sink.take().is_empty(), "polling doesn't interrupt");
    }

    #[test]
    fn cmos_ram_is_read_write_and_nmi_bit_is_ignored() {
        let (mut rtc, _) = rtc();
        write(&mut rtc, 0x80 | 0x40, 0x5a);
        assert_eq!(read(&mut rtc, 0x40), 0x5a);
        rtc.set_cmos_byte(0x34, 0x12);
        assert_eq!(read(&mut rtc, 0x34), 0x12);
    }
}
