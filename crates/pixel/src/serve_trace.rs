// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The serve steps of this invocation, collected where each request is
//! routed and written into its action-log event by `main`.

use std::sync::Mutex;
use std::time::Instant;

use pixel_actionlog::ServeStep;

/// Steps kept per invocation. `ready` makes two requests; a command that
/// loops over requests must not grow its log line without bound.
const MAX_STEPS: usize = 8;

static STEPS: Mutex<Vec<ServeStep>> = Mutex::new(Vec::new());

/// Append `step` to `steps` unless the cap is reached; the first steps are
/// the ones kept, since the first request of an invocation is the one that
/// pays a cold start.
fn push_capped(steps: &mut Vec<ServeStep>, step: ServeStep) {
    if steps.len() < MAX_STEPS {
        steps.push(step);
    }
}

/// Record how one request was served.
pub fn record(step: ServeStep) {
    if let Ok(mut steps) = STEPS.lock() {
        push_capped(&mut steps, step);
    }
}

/// Every step recorded so far, leaving none behind.
pub fn take() -> Vec<ServeStep> {
    STEPS
        .lock()
        .map(|mut steps| std::mem::take(&mut *steps))
        .unwrap_or_default()
}

/// Whole milliseconds since `start`.
pub fn millis_since(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Run `f`, returning its value and the whole milliseconds it took.
pub fn timed<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let clock = Instant::now();
    let value = f();
    (value, millis_since(clock))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixel_actionlog::{InProcessReason, ServeRoute};
    use std::time::Duration;

    #[test]
    fn push_capped_keeps_the_first_steps_up_to_the_cap() {
        let mut steps = Vec::new();
        push_capped(&mut steps, ServeStep::new(ServeRoute::DaemonStarted));
        for _ in 1..MAX_STEPS + 3 {
            push_capped(&mut steps, ServeStep::new(ServeRoute::Daemon));
        }
        assert_eq!(steps.len(), MAX_STEPS);
        assert_eq!(steps[0].route, ServeRoute::DaemonStarted);
    }

    /// The only test touching the process-wide list: `record` then `take`
    /// hands the step over once.
    #[test]
    fn take_returns_recorded_steps_once() {
        let step = ServeStep::in_process(InProcessReason::NotRouted);
        take();
        record(step.clone());
        let taken = take();
        assert!(taken.contains(&step), "{taken:?}");
        assert!(!take().contains(&step));
    }

    #[test]
    fn millis_since_measures_elapsed_wall_clock() {
        let start = Instant::now();
        std::thread::sleep(Duration::from_millis(5));
        let ms = millis_since(start);
        assert!(ms >= 5, "{ms}");
        assert!(ms < 5_000, "{ms}");
    }

    #[test]
    fn timed_returns_the_value_and_the_time_it_took() {
        let (value, ms) = timed(|| {
            std::thread::sleep(Duration::from_millis(5));
            42
        });
        assert_eq!(value, 42);
        assert!((5..5_000).contains(&ms), "{ms}");
    }
}
