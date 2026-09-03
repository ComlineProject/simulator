//! The simulation clock: a virtual time value plus a queue of events ordered by
//! due time. Nothing in the sim runs on wall-clock time — the [`Pump`] pops the
//! earliest event, moves `now` to its due instant, applies it (which may
//! schedule more), and repeats.
//!
//! This replaces the playground's async `Clock` (`RealClock` / `SteppedClock`
//! and their `Promise`-based `sleep`). There is only stepped time here; the host
//! decides how fast to advance it — drain everything for a test or a settled
//! call, advance by a delta from a `requestAnimationFrame` callback for
//! real-time playback, one event at a time for frame-by-frame stepping.
//! Deterministic: the same schedule drains in the same order every run, ties
//! broken by insertion order.
//!
//! [`Pump`]: crate::pump::Pump

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// Time is kept as fixed-point microseconds so the event queue can order on an
/// integer key; the API is `f64` milliseconds, matching the playground.
type Micros = i64;

fn to_micros(ms: f64) -> Micros {
    (ms * 1000.0).round() as Micros
}

fn to_millis(us: Micros) -> f64 {
    us as f64 / 1000.0
}

/// One queued event: its payload plus the `(due, seq)` key it orders on.
struct Scheduled<E> {
    due: Micros,
    /// Insertion order — breaks ties so equal-time events fire FIFO.
    seq: u64,
    event: E,
}

impl<E> PartialEq for Scheduled<E> {
    fn eq(&self, other: &Self) -> bool {
        (self.due, self.seq) == (other.due, other.seq)
    }
}
impl<E> Eq for Scheduled<E> {}
impl<E> PartialOrd for Scheduled<E> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<E> Ord for Scheduled<E> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reversed: `BinaryHeap` is a max-heap, we want the *earliest* on top.
        (other.due, other.seq).cmp(&(self.due, self.seq))
    }
}

/// A virtual clock with a due-time-ordered event queue, generic over the event
/// payload so it needn't know anything about frames or the pump.
pub struct Clock<E> {
    now: Micros,
    seq: u64,
    queue: BinaryHeap<Scheduled<E>>,
}

impl<E> Default for Clock<E> {
    fn default() -> Self {
        Self {
            now: 0,
            seq: 0,
            queue: BinaryHeap::new(),
        }
    }
}

impl<E> Clock<E> {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current virtual time, in milliseconds.
    pub fn now(&self) -> f64 {
        to_millis(self.now)
    }

    /// Queue `event` to fire `delay_ms` from now (clamped to ≥ 0). Returns a
    /// handle to [`cancel`](Self::cancel) it with.
    pub fn schedule(&mut self, delay_ms: f64, event: E) -> u64 {
        let due = self.now + to_micros(delay_ms.max(0.0));
        self.seq += 1;
        let handle = self.seq;
        self.queue.push(Scheduled {
            due,
            seq: handle,
            event,
        });
        handle
    }

    /// Drop the queued event with this handle, if it hasn't fired. A no-op
    /// otherwise.
    pub fn cancel(&mut self, handle: u64) {
        let kept: BinaryHeap<Scheduled<E>> = std::mem::take(&mut self.queue)
            .into_iter()
            .filter(|s| s.seq != handle)
            .collect();
        self.queue = kept;
    }

    /// Timers queued and not yet fired.
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// The due time of the next event, in milliseconds, if any.
    pub fn peek_due(&self) -> Option<f64> {
        self.queue.peek().map(|s| to_millis(s.due))
    }

    /// Fire the single earliest event, moving `now` to its due instant.
    /// `None` when the queue is empty.
    pub fn pop_next(&mut self) -> Option<E> {
        let next = self.queue.pop()?;
        self.now = self.now.max(next.due);
        Some(next.event)
    }

    /// Move `now` forward to at least `abs_ms` (never backwards). The pump calls
    /// this after an `advance(delta)` so time parks at the window edge even when
    /// nothing was queued that far out.
    pub fn park_at_least(&mut self, abs_ms: f64) {
        self.now = self.now.max(to_micros(abs_ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain<E>(c: &mut Clock<E>) -> Vec<E> {
        let mut out = Vec::new();
        while let Some(e) = c.pop_next() {
            out.push(e);
        }
        out
    }

    #[test]
    fn fires_in_due_time_order_then_insertion_order() {
        let mut c: Clock<&str> = Clock::new();
        c.schedule(30.0, "c");
        c.schedule(10.0, "a");
        c.schedule(10.0, "a2"); // same time as "a" — must come after it
        c.schedule(20.0, "b");

        assert_eq!(drain(&mut c), ["a", "a2", "b", "c"]);
    }

    #[test]
    fn pop_next_advances_now_to_the_event() {
        let mut c: Clock<()> = Clock::new();
        c.schedule(42.0, ());
        assert_eq!(c.now(), 0.0);
        c.pop_next();
        assert_eq!(c.now(), 42.0);
    }

    #[test]
    fn scheduling_is_relative_to_the_current_time() {
        let mut c: Clock<&str> = Clock::new();
        c.schedule(10.0, "first");
        c.pop_next(); // now = 10
        c.schedule(5.0, "second"); // due at 15, not 5
        assert_eq!(c.peek_due(), Some(15.0));
        c.pop_next();
        assert_eq!(c.now(), 15.0);
    }

    #[test]
    fn cancel_removes_a_pending_event_by_handle() {
        let mut c: Clock<&str> = Clock::new();
        c.schedule(10.0, "a");
        let h = c.schedule(20.0, "b");
        c.schedule(30.0, "c");
        c.cancel(h);
        c.cancel(9999); // unknown handle → no-op

        assert_eq!(drain(&mut c), ["a", "c"]);
    }

    #[test]
    fn park_at_least_only_moves_time_forward() {
        let mut c: Clock<()> = Clock::new();
        c.park_at_least(100.0);
        assert_eq!(c.now(), 100.0);
        c.park_at_least(50.0);
        assert_eq!(c.now(), 100.0, "never goes backwards");
    }

    #[test]
    fn peek_due_is_none_when_empty() {
        let mut c: Clock<()> = Clock::new();
        assert_eq!(c.peek_due(), None);
        c.schedule(5.0, ());
        assert_eq!(c.peek_due(), Some(5.0));
        c.pop_next();
        assert_eq!(c.peek_due(), None);
    }
}
