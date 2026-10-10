//! MC146818 real-time clock and CMOS RAM.
//!
//! Time comes from the host's wall clock plus an offset the guest sets by writing the
//! time registers. Periodic, alarm and update interrupts are not emulated.

use std::sync::Arc;
use std::time::Duration;

use vm_core::BusDevice;

use crate::clock::Clock;

/// Index and data ports.
pub const RTC_BASE: u16 = 0x70;
pub const RTC_PORTS: u16 = 2;

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
const B_SET: u8 = 0x80;
const B_INTERRUPTS: u8 = 0x70;
const B_BINARY: u8 = 0x04;
const B_24_HOUR: u8 = 0x02;
const D_VALID_RAM: u8 = 0x80;
const PM: u8 = 0x80;

/// The update cycle runs in the last 244 µs of each second; UIP is set during it.
const UIP_WINDOW: Duration = Duration::from_micros(244);

const SECONDS_PER_DAY: i64 = 86_400;

pub struct Rtc {
    index: usize,
    ram: [u8; 128],
    clock: Arc<dyn Clock>,
    /// Guest time minus host time, in seconds.
    offset: i64,
    /// While register B's SET bit is on: the time registers as the guest is writing them.
    setting: Option<i64>,
}

impl Rtc {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        let mut ram = [0u8; 128];
        ram[REG_A] = 0x26; // 32.768 kHz time base, 1024 Hz rate
        ram[REG_B] = B_24_HOUR;
        ram[REG_D] = D_VALID_RAM;
        Self {
            index: 0,
            ram,
            clock,
            offset: 0,
            setting: None,
        }
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

    fn read_register(&self, index: usize) -> u8 {
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
            // No interrupt flags are ever raised.
            REG_C => 0,
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
            REG_A => self.ram[REG_A] = value & !A_UIP,
            REG_B => {
                let was_setting = self.setting.is_some();
                if value & B_SET != 0 && !was_setting {
                    self.setting = Some(self.guest_seconds());
                } else if value & B_SET == 0
                    && let Some(seconds) = self.setting.take()
                {
                    self.offset = seconds - self.clock.wall_time().as_secs() as i64;
                }
                if value & B_INTERRUPTS != 0 {
                    tracing::debug!(value, "RTC interrupts enabled but not emulated");
                }
                self.ram[REG_B] = value;
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
    use super::*;
    use crate::clock::FakeClock;

    /// 2026-10-10 13:45:30 UTC.
    const WALL: u64 = 1_791_639_930;

    fn rtc() -> (Rtc, Arc<FakeClock>) {
        let clock = Arc::new(FakeClock::new(Duration::from_secs(WALL)));
        (Rtc::new(clock.clone()), clock)
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
    fn cmos_ram_is_read_write_and_nmi_bit_is_ignored() {
        let (mut rtc, _) = rtc();
        write(&mut rtc, 0x80 | 0x40, 0x5a);
        assert_eq!(read(&mut rtc, 0x40), 0x5a);
        rtc.set_cmos_byte(0x34, 0x12);
        assert_eq!(read(&mut rtc, 0x34), 0x12);
    }
}
