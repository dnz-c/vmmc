//! Time sources for devices that count time (PIT, PM timer, RTC).

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

pub trait Clock: Send + Sync {
    /// Monotonic time since an arbitrary, fixed start.
    fn now(&self) -> Duration;
    /// Wall-clock time since the Unix epoch, UTC.
    fn wall_time(&self) -> Duration;
}

/// The host's clocks.
#[derive(Debug)]
pub struct HostClock {
    start: Instant,
}

impl HostClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for HostClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for HostClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn wall_time(&self) -> Duration {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
    }
}

/// The guest's clock: the host's, except that it stands still while the VM is paused,
/// and it can be set, so a restored VM's devices go on from the time they were saved
/// at.
#[derive(Debug)]
pub struct VmClock {
    state: Mutex<VmClockState>,
}

#[derive(Debug)]
struct VmClockState {
    /// The time when the clock last stopped or was set.
    base: Duration,
    /// When it started running from `base`, while it runs.
    running_since: Option<Instant>,
}

impl VmClock {
    /// A running clock, starting from zero.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(VmClockState {
                base: Duration::ZERO,
                running_since: Some(Instant::now()),
            }),
        }
    }

    /// Stops the clock.
    pub fn pause(&self) {
        let mut state = self.lock();
        if let Some(since) = state.running_since.take() {
            state.base += since.elapsed();
        }
    }

    /// Starts it again from where it stopped.
    pub fn resume(&self) {
        let mut state = self.lock();
        if state.running_since.is_none() {
            state.running_since = Some(Instant::now());
        }
    }

    /// Sets it to `now`, running or not.
    pub fn set(&self, now: Duration) {
        let mut state = self.lock();
        state.base = now;
        if state.running_since.is_some() {
            state.running_since = Some(Instant::now());
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VmClockState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for VmClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for VmClock {
    fn now(&self) -> Duration {
        let state = self.lock();
        state.base
            + state
                .running_since
                .map_or(Duration::ZERO, |since| since.elapsed())
    }

    fn wall_time(&self) -> Duration {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
    }
}

/// A clock that only moves when told to. Wall time is a fixed base plus `now`.
#[derive(Debug, Default)]
pub struct FakeClock {
    now: Mutex<Duration>,
    wall_base: Duration,
}

impl FakeClock {
    /// A clock whose wall time starts `wall_base` after the Unix epoch.
    pub fn new(wall_base: Duration) -> Self {
        Self {
            now: Mutex::new(Duration::ZERO),
            wall_base,
        }
    }

    pub fn advance(&self, by: Duration) {
        *self.now.lock().unwrap() += by;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        *self.now.lock().unwrap()
    }

    fn wall_time(&self) -> Duration {
        self.wall_base + self.now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vm_clock_stands_still_while_paused_and_can_be_set() {
        let clock = VmClock::new();
        clock.pause();
        let paused = clock.now();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(clock.now(), paused);
        clock.set(Duration::from_secs(100));
        assert_eq!(clock.now(), Duration::from_secs(100));
        clock.resume();
        std::thread::sleep(Duration::from_millis(20));
        let running = clock.now();
        assert!(running > Duration::from_secs(100));
        assert!(running < Duration::from_secs(101));
    }
}
