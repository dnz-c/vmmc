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
