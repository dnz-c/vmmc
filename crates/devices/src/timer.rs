//! Host timers that fire device interrupts (for example PIT channel 0).

use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// When a [`Ticker`] fires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerMode {
    /// Every `period`, starting one period from now.
    Periodic(Duration),
    /// Once, after the delay.
    OneShot(Duration),
}

impl TimerMode {
    fn first_delay(self) -> Duration {
        match self {
            Self::Periodic(delay) | Self::OneShot(delay) => delay,
        }
    }
}

/// Something that calls back on a schedule. Devices hold one per timer they emulate.
pub trait Ticker: Send {
    /// Replaces the schedule. `None` disarms the timer.
    fn program(&self, mode: Option<TimerMode>);
}

/// The deadline after one that just passed at `now`. A periodic timer that fell behind
/// skips the periods it missed and fires once, rather than bursting to catch up.
fn next_deadline(mode: TimerMode, deadline: Instant, now: Instant) -> Option<Instant> {
    match mode {
        TimerMode::OneShot(_) => None,
        TimerMode::Periodic(period) => {
            let period = period.max(Duration::from_micros(1));
            let missed = now.saturating_duration_since(deadline).as_nanos() / period.as_nanos();
            let skip = u32::try_from(missed + 1).unwrap_or(u32::MAX);
            Some(deadline + period * skip)
        }
    }
}

#[derive(Default)]
struct State {
    mode: Option<TimerMode>,
    next: Option<Instant>,
    /// Bumped by every `program`, so the thread can tell its deadline went stale.
    generation: u64,
    shutdown: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

/// A [`Ticker`] backed by a host thread.
///
/// While armed, the thread sleeps with `thread::sleep`, which is precise to well under a
/// millisecond on Windows (unlike condition variable timeouts, which follow the ~15 ms
/// system tick). A reschedule that lands mid-sleep takes effect when the sleep ends.
pub struct ThreadTicker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl ThreadTicker {
    /// Starts the thread. `tick` runs on it each time the timer fires.
    pub fn new(name: &str, tick: impl Fn() + Send + 'static) -> io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let thread = thread::Builder::new().name(name.into()).spawn({
            let shared = shared.clone();
            move || run(&shared, tick)
        })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }
}

fn run(shared: &Shared, tick: impl Fn()) {
    let mut state = shared.state.lock().unwrap();
    loop {
        if state.shutdown {
            return;
        }
        let (Some(deadline), Some(mode)) = (state.next, state.mode) else {
            state = shared.wake.wait(state).unwrap();
            continue;
        };
        let now = Instant::now();
        if now < deadline {
            let generation = state.generation;
            drop(state);
            thread::sleep(deadline - now);
            state = shared.state.lock().unwrap();
            if state.generation != generation {
                continue;
            }
        }
        state.next = next_deadline(mode, deadline, Instant::now());
        if state.next.is_none() {
            state.mode = None;
        }
        drop(state);
        tick();
        state = shared.state.lock().unwrap();
    }
}

impl Ticker for ThreadTicker {
    fn program(&self, mode: Option<TimerMode>) {
        let mut state = self.shared.state.lock().unwrap();
        state.mode = mode;
        state.next = mode.map(|mode| Instant::now() + mode.first_delay());
        state.generation += 1;
        self.shared.wake.notify_one();
    }
}

impl Drop for ThreadTicker {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().shutdown = true;
        self.shared.wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Records every schedule it is given.
    #[derive(Clone, Default)]
    pub struct RecordingTicker(pub Arc<Mutex<Vec<Option<TimerMode>>>>);

    impl RecordingTicker {
        pub fn last(&self) -> Option<Option<TimerMode>> {
            self.0.lock().unwrap().last().copied()
        }
    }

    impl Ticker for RecordingTicker {
        fn program(&self, mode: Option<TimerMode>) {
            self.0.lock().unwrap().push(mode);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[test]
    fn periodic_deadlines_skip_missed_periods() {
        let start = Instant::now();
        let period = Duration::from_millis(10);
        let mode = TimerMode::Periodic(period);
        // On time: the next deadline is one period on.
        assert_eq!(next_deadline(mode, start, start), Some(start + period));
        // 25 ms late: two periods were missed; fire once and resume on the grid.
        assert_eq!(
            next_deadline(mode, start, start + Duration::from_millis(25)),
            Some(start + period * 3)
        );
        assert_eq!(
            next_deadline(TimerMode::OneShot(period), start, start),
            None
        );
    }

    #[test]
    fn thread_ticker_fires_periodically_and_disarms() {
        let ticks = Arc::new(AtomicU32::new(0));
        let ticker = ThreadTicker::new("test-ticker", {
            let ticks = ticks.clone();
            move || {
                ticks.fetch_add(1, Ordering::SeqCst);
            }
        })
        .unwrap();

        ticker.program(Some(TimerMode::Periodic(Duration::from_millis(2))));
        thread::sleep(Duration::from_millis(100));
        ticker.program(None);
        let fired = ticks.load(Ordering::SeqCst);
        // Loose bounds: CI machines are noisy.
        assert!(fired >= 5, "only {fired} ticks in 100 ms at a 2 ms period");

        thread::sleep(Duration::from_millis(20));
        let after_disarm = ticks.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(
            ticks.load(Ordering::SeqCst),
            after_disarm,
            "kept firing after disarm"
        );
    }

    #[test]
    fn thread_ticker_one_shot_fires_once() {
        let ticks = Arc::new(AtomicU32::new(0));
        let ticker = ThreadTicker::new("test-oneshot", {
            let ticks = ticks.clone();
            move || {
                ticks.fetch_add(1, Ordering::SeqCst);
            }
        })
        .unwrap();
        ticker.program(Some(TimerMode::OneShot(Duration::from_millis(5))));
        thread::sleep(Duration::from_millis(60));
        assert_eq!(ticks.load(Ordering::SeqCst), 1);
    }
}
