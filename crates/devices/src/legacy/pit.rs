//! 8254 programmable interval timer, plus the port 0x61 bits that belong to it.
//!
//! Counters aren't ticked. Their value is computed from the clock when read, so readback
//! is exact however rarely the guest looks. Channel 0 drives IRQ0 through a host
//! [`Ticker`]. Channel 2's gate and output appear in port 0x61, which is what Linux
//! uses to calibrate the TSC.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use vm_core::BusDevice;

use crate::clock::Clock;
use crate::snapshot::{self, Snapshot, StateError};
use crate::timer::{Ticker, TimerMode};

/// First of the four PIT ports (counters 0–2, then the mode/command register).
pub const PIT_BASE: u16 = 0x40;
pub const PIT_PORTS: u16 = 4;
/// The system control port: speaker, NMI status and PIT channel 2 gate/output.
pub const PORT_61: u16 = 0x61;

/// Input clock of the PIT, in Hz.
pub const FREQUENCY: u64 = 1_193_182;

/// Port 0x61 bit 0: channel 2 gate.
const GATE2: u8 = 1 << 0;
/// Port 0x61 bit 1: speaker data enable (stored, otherwise ignored).
const SPEAKER: u8 = 1 << 1;
/// Port 0x61 bit 4: toggles with every DRAM refresh, about every 15 µs.
const REFRESH: u8 = 1 << 4;
/// Port 0x61 bit 5: channel 2 output.
const OUT2: u8 = 1 << 5;
const REFRESH_PERIOD_NS: u128 = 15_085;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Access {
    LowByte,
    HighByte,
    LowThenHigh,
}

impl Access {
    fn from_bits(bits: u8) -> Self {
        match bits & 3 {
            1 => Self::LowByte,
            2 => Self::HighByte,
            _ => Self::LowThenHigh,
        }
    }

    fn bits(self) -> u8 {
        match self {
            Self::LowByte => 1,
            Self::HighByte => 2,
            Self::LowThenHigh => 3,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Channel {
    mode: u8,
    access: Access,
    bcd: bool,
    /// The programmed count; 0 means 65536.
    count: u16,
    /// Set once a count has been loaded since the last mode write.
    loaded: bool,
    /// When counting started, on the clock's timeline.
    start: Duration,
    /// For a gated channel that is stopped: ticks counted before the gate went low.
    frozen_ticks: Option<u64>,
    gate: bool,
    /// Low byte of a count being written in low-then-high mode.
    pending_low: Option<u8>,
    /// A latched count, as the bytes still to be read.
    latched_count: Option<(u16, Access, bool)>,
    latched_status: Option<u8>,
    /// Which byte of a low-then-high read comes next.
    read_high_next: bool,
}

impl Channel {
    fn new(gate: bool) -> Self {
        Self {
            mode: 0,
            access: Access::LowThenHigh,
            bcd: false,
            count: 0,
            loaded: false,
            start: Duration::ZERO,
            frozen_ticks: None,
            gate,
            pending_low: None,
            latched_count: None,
            latched_status: None,
            read_high_next: false,
        }
    }

    fn reload(&self) -> u64 {
        if self.count == 0 {
            0x1_0000
        } else {
            u64::from(self.count)
        }
    }

    fn ticks(&self, now: Duration) -> u64 {
        self.frozen_ticks.unwrap_or_else(|| {
            let elapsed = now.saturating_sub(self.start).as_nanos();
            (elapsed * u128::from(FREQUENCY) / 1_000_000_000) as u64
        })
    }

    /// The counter value and output level at `now`.
    fn state(&self, now: Duration) -> (u16, bool) {
        if !self.loaded {
            return (self.count, self.mode != 0);
        }
        let ticks = self.ticks(now);
        let reload = self.reload();
        match self.mode {
            // Modes 2 and 3 repeat every `reload` ticks.
            2 => {
                let count = reload - ticks % reload;
                // Output drops for the one tick where the count is 1.
                (count as u16, count != 1)
            }
            3 => {
                let phase = ticks % reload;
                let half = reload.div_ceil(2);
                let count = reload - 2 * (phase % half);
                (count as u16, phase < half)
            }
            // Modes 0, 1, 4 and 5 count down once and then wrap through 0xffff.
            mode => {
                let count = (reload.wrapping_sub(ticks) & 0xffff) as u16;
                let done = ticks >= reload;
                let out = match mode {
                    0 | 1 => done,
                    // Strobe modes: the output pulses low for the one terminal tick.
                    _ => ticks != reload,
                };
                (count, out)
            }
        }
    }

    fn status(&self, now: Duration) -> u8 {
        let (_, out) = self.state(now);
        (u8::from(out) << 7)
            | (u8::from(!self.loaded) << 6)
            | (self.access.bits() << 4)
            | (self.mode << 1)
            | u8::from(self.bcd)
    }

    fn set_gate(&mut self, gate: bool, now: Duration) {
        if gate == self.gate {
            return;
        }
        self.gate = gate;
        if !gate {
            self.frozen_ticks = Some(self.ticks(now));
        } else if matches!(self.mode, 1 | 2 | 3 | 5) {
            // A rising gate restarts these modes from the reload value.
            self.frozen_ticks = None;
            self.start = now;
        } else if let Some(ticks) = self.frozen_ticks.take() {
            // Modes 0 and 4 just resume.
            let elapsed = Duration::from_nanos(ticks * 1_000_000_000 / FREQUENCY);
            self.start = now.saturating_sub(elapsed);
        }
    }

    fn load(&mut self, count: u16, now: Duration) {
        self.count = count;
        self.loaded = true;
        self.start = now;
        self.frozen_ticks = (!self.gate).then_some(0);
    }

    fn write_data(&mut self, value: u8, now: Duration) -> bool {
        let count = match self.access {
            Access::LowByte => u16::from(value),
            Access::HighByte => u16::from(value) << 8,
            Access::LowThenHigh => match self.pending_low.take() {
                None => {
                    self.pending_low = Some(value);
                    return false;
                }
                Some(low) => u16::from_le_bytes([low, value]),
            },
        };
        self.load(count, now);
        true
    }

    fn read_data(&mut self, now: Duration) -> u8 {
        if let Some(status) = self.latched_status.take() {
            return status;
        }
        let (count, access, high_next) = match self.latched_count {
            Some(latched) => latched,
            None => (self.state(now).0, self.access, self.read_high_next),
        };
        let [low, high] = count.to_le_bytes();
        match access {
            Access::LowByte => {
                self.latched_count = None;
                low
            }
            Access::HighByte => {
                self.latched_count = None;
                high
            }
            Access::LowThenHigh => {
                if high_next {
                    self.latched_count = None;
                    self.read_high_next = false;
                    high
                } else {
                    if let Some(latched) = &mut self.latched_count {
                        latched.2 = true;
                    } else {
                        self.read_high_next = true;
                    }
                    low
                }
            }
        }
    }

    fn latch_count(&mut self, now: Duration) {
        if self.latched_count.is_none() {
            self.latched_count = Some((self.state(now).0, self.access, false));
        }
    }

    fn latch_status(&mut self, now: Duration) {
        if self.latched_status.is_none() {
            self.latched_status = Some(self.status(now));
        }
    }
}

pub struct Pit {
    channels: [Channel; 3],
    port61: u8,
    clock: Arc<dyn Clock>,
    /// Fires IRQ0 for channel 0.
    irq0: Box<dyn Ticker>,
}

impl Pit {
    pub fn new(clock: Arc<dyn Clock>, irq0: Box<dyn Ticker>) -> Self {
        Self {
            // Channels 0 and 1 have their gates tied high; channel 2's is in port 0x61.
            channels: [Channel::new(true), Channel::new(true), Channel::new(false)],
            port61: 0,
            clock,
            irq0,
        }
    }

    fn command(&mut self, value: u8) {
        let now = self.clock.now();
        let select = value >> 6;
        if select == 3 {
            // Read-back: bit 5 clear latches counts, bit 4 clear latches status.
            for (index, channel) in self.channels.iter_mut().enumerate() {
                if value & (2 << index) != 0 {
                    if value & (1 << 5) == 0 {
                        channel.latch_count(now);
                    }
                    if value & (1 << 4) == 0 {
                        channel.latch_status(now);
                    }
                }
            }
            return;
        }
        let channel = &mut self.channels[usize::from(select)];
        let access = (value >> 4) & 3;
        if access == 0 {
            channel.latch_count(now);
            return;
        }
        channel.access = Access::from_bits(access);
        channel.mode = match (value >> 1) & 7 {
            6 => 2,
            7 => 3,
            mode => mode,
        };
        channel.bcd = value & 1 != 0;
        channel.loaded = false;
        channel.pending_low = None;
        channel.latched_count = None;
        channel.read_high_next = false;
        if select == 0 {
            self.irq0.program(None);
        }
    }

    /// Sets the IRQ0 timer going as a restored channel 0 has it: periodic modes on
    /// their period, a one-shot count for the rest of it, if it hasn't run out.
    fn restore_irq0(&self) {
        let channel = &self.channels[0];
        if !channel.loaded {
            self.irq0.program(None);
            return;
        }
        if matches!(channel.mode, 2 | 3) {
            return self.program_irq0();
        }
        let left = channel
            .reload()
            .saturating_sub(channel.ticks(self.clock.now()));
        self.irq0.program(
            (left > 0).then(|| {
                TimerMode::OneShot(Duration::from_nanos(left * 1_000_000_000 / FREQUENCY))
            }),
        );
    }

    /// Reprograms the IRQ0 timer after channel 0 got a new count.
    fn program_irq0(&self) {
        let channel = &self.channels[0];
        let period = Duration::from_nanos(channel.reload() * 1_000_000_000 / FREQUENCY);
        let mode = match channel.mode {
            2 | 3 => TimerMode::Periodic(period),
            _ => TimerMode::OneShot(period),
        };
        self.irq0.program(Some(mode));
    }

    fn read_port61(&self) -> u8 {
        let now = self.clock.now();
        let refresh = if (now.as_nanos() / REFRESH_PERIOD_NS) % 2 == 1 {
            REFRESH
        } else {
            0
        };
        let out2 = if self.channels[2].state(now).1 {
            OUT2
        } else {
            0
        };
        (self.port61 & (GATE2 | SPEAKER)) | refresh | out2
    }

    fn write_port61(&mut self, value: u8) {
        self.port61 = value & (GATE2 | SPEAKER);
        let now = self.clock.now();
        self.channels[2].set_gate(value & GATE2 != 0, now);
    }
}

impl BusDevice for Pit {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let now = self.clock.now();
        let value = match offset {
            0..=2 => self.channels[offset as usize].read_data(now),
            // The mode register is write-only.
            _ => 0xff,
        };
        data.fill(value);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let value = data[0];
        let now = self.clock.now();
        match offset {
            0..=2 => {
                let channel = &mut self.channels[offset as usize];
                if channel.write_data(value, now) && offset == 0 {
                    self.program_irq0();
                }
            }
            3 => self.command(value),
            _ => {}
        }
    }
}

#[derive(Serialize, Deserialize)]
struct PitState {
    channels: [Channel; 3],
    port61: u8,
}

impl Snapshot for Pit {
    fn save(&self) -> Vec<u8> {
        snapshot::encode(&PitState {
            channels: self.channels.clone(),
            port61: self.port61,
        })
    }

    fn restore(&mut self, state: &[u8]) -> Result<(), StateError> {
        let state: PitState = snapshot::decode(state)?;
        self.channels = state.channels;
        self.port61 = state.port61;
        self.restore_irq0();
        Ok(())
    }
}

/// Port 0x61, as a bus device sharing the PIT.
pub struct Port61(pub Arc<Mutex<Pit>>);

impl BusDevice for Port61 {
    fn read(&mut self, _: u64, data: &mut [u8]) {
        data.fill(self.0.lock().unwrap().read_port61());
    }

    fn write(&mut self, _: u64, data: &[u8]) {
        self.0.lock().unwrap().write_port61(data[0]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;
    use crate::timer::testing::RecordingTicker;

    fn pit() -> (Pit, Arc<FakeClock>, RecordingTicker) {
        let clock = Arc::new(FakeClock::default());
        let ticker = RecordingTicker::default();
        (
            Pit::new(clock.clone(), Box::new(ticker.clone())),
            clock,
            ticker,
        )
    }

    /// Port 0x61 lives in a separate bus device; the helpers route it there.
    const P61: u64 = PORT_61 as u64;

    fn out(pit: &mut Pit, port: u64, value: u8) {
        if port == P61 {
            pit.write_port61(value);
        } else {
            pit.write(port, &[value]);
        }
    }

    fn inb(pit: &mut Pit, port: u64) -> u8 {
        if port == P61 {
            return pit.read_port61();
        }
        let mut data = [0];
        pit.read(port, &mut data);
        data[0]
    }

    /// The time `ticks` PIT input clocks take, rounded up so the tick has happened.
    fn ticks(ticks: u64) -> Duration {
        Duration::from_nanos((ticks * 1_000_000_000).div_ceil(FREQUENCY))
    }

    #[test]
    fn a_restored_pit_counts_on_and_runs_its_timer() {
        use crate::snapshot::Snapshot;
        let (mut saved, clock, _) = pit();
        // Channel 0, mode 2, count 1000.
        out(&mut saved, 3, 0x34);
        out(&mut saved, 0, 0xe8);
        out(&mut saved, 0, 0x03);
        clock.advance(ticks(300));
        let state = saved.save();

        // A new PIT, on a clock that has gone on to the same time.
        let (mut restored, restored_clock, ticker) = pit();
        restored_clock.advance(ticks(300));
        restored.restore(&state).unwrap();
        let period = Duration::from_nanos(1000 * 1_000_000_000 / FREQUENCY);
        assert_eq!(ticker.last(), Some(Some(TimerMode::Periodic(period))));
        out(&mut restored, 3, 0x00);
        let count = u16::from_le_bytes([inb(&mut restored, 0), inb(&mut restored, 0)]);
        assert_eq!(count, 700);
        assert_eq!(restored.save(), state);
    }

    #[test]
    fn mode_2_programs_periodic_irq0_and_counts_down() {
        let (mut pit, clock, ticker) = pit();
        // Channel 0, low-then-high, mode 2: 1193 ticks, about 1 ms, the classic HZ=1000.
        out(&mut pit, 3, 0x34);
        out(&mut pit, 0, 0xa9);
        out(&mut pit, 0, 0x04);
        assert_eq!(
            ticker.last(),
            Some(Some(TimerMode::Periodic(Duration::from_nanos(999_847))))
        );

        clock.advance(ticks(100));
        out(&mut pit, 3, 0x00); // latch channel 0
        let count = u16::from_le_bytes([inb(&mut pit, 0), inb(&mut pit, 0)]);
        assert_eq!(count, 1193 - 100);

        // It wraps back to the reload value each period.
        clock.advance(ticks(1193));
        out(&mut pit, 3, 0x00);
        let count = u16::from_le_bytes([inb(&mut pit, 0), inb(&mut pit, 0)]);
        assert_eq!(count, 1193 - 100);
    }

    #[test]
    fn mode_0_one_shot_and_mode_change_disarms_irq0() {
        let (mut pit, _, ticker) = pit();
        out(&mut pit, 3, 0x30); // channel 0, low-then-high, mode 0
        assert_eq!(ticker.last(), Some(None), "a mode write stops the timer");
        out(&mut pit, 0, 0x00);
        out(&mut pit, 0, 0x00); // count 0 = 65536
        assert_eq!(
            ticker.last(),
            Some(Some(TimerMode::OneShot(Duration::from_nanos(54_925_401))))
        );
    }

    #[test]
    fn linux_fast_calibration_sequence_on_channel_2() {
        let (mut pit, clock, _) = pit();
        // Gate high, speaker off; channel 2, low-then-high, mode 0; count 0xffff.
        out(&mut pit, P61, 1);
        out(&mut pit, 3, 0xb0);
        out(&mut pit, 2, 0xff);
        out(&mut pit, 2, 0xff);

        clock.advance(ticks(0x1000));
        // Linux reads the count unlatched: low byte, then high byte.
        let low = inb(&mut pit, 2);
        let high = inb(&mut pit, 2);
        assert_eq!(u16::from_le_bytes([low, high]), 0xffff - 0x1000);

        assert_eq!(inb(&mut pit, P61) & OUT2, 0, "OUT2 low while counting");
        clock.advance(ticks(0xffff));
        assert_ne!(inb(&mut pit, P61) & OUT2, 0, "OUT2 high at terminal count");
    }

    #[test]
    fn channel_2_does_not_count_with_gate_low() {
        let (mut pit, clock, _) = pit();
        out(&mut pit, 3, 0xb0);
        out(&mut pit, 2, 0x00);
        out(&mut pit, 2, 0x10); // 0x1000
        clock.advance(ticks(500));
        out(&mut pit, 3, 0x80); // latch channel 2
        let count = u16::from_le_bytes([inb(&mut pit, 2), inb(&mut pit, 2)]);
        assert_eq!(count, 0x1000, "gate low: frozen");

        out(&mut pit, P61, 1);
        clock.advance(ticks(0x100));
        out(&mut pit, 3, 0x80);
        let count = u16::from_le_bytes([inb(&mut pit, 2), inb(&mut pit, 2)]);
        assert_eq!(count, 0x1000 - 0x100);
    }

    #[test]
    fn read_back_latches_status_then_count() {
        let (mut pit, clock, _) = pit();
        out(&mut pit, 3, 0x34);
        out(&mut pit, 0, 0x00);
        out(&mut pit, 0, 0x10);
        clock.advance(ticks(0x10));
        // Read-back channel 0: latch count and status.
        out(&mut pit, 3, 0b1100_0010);
        let status = inb(&mut pit, 0);
        assert_eq!(status & 0x3f, 0x34, "access and mode as programmed");
        assert_eq!(status & 0x40, 0, "count is loaded");
        let count = u16::from_le_bytes([inb(&mut pit, 0), inb(&mut pit, 0)]);
        assert_eq!(count, 0x1000 - 0x10);
    }

    #[test]
    fn port_61_keeps_gate_and_speaker_bits_and_toggles_refresh() {
        let (mut pit, clock, _) = pit();
        out(&mut pit, P61, 0xff);
        assert_eq!(inb(&mut pit, P61) & 0x0f, GATE2 | SPEAKER);
        let first = inb(&mut pit, P61) & REFRESH;
        clock.advance(Duration::from_nanos(15_085));
        assert_ne!(inb(&mut pit, P61) & REFRESH, first);
    }
}
