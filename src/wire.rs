//! One connection's wire: the frame [`Tap`], the mutable [`FaultSpec`], the
//! fixed per-frame latency, and a reorder buffer. [`Channel::send`] records the
//! frame and decides what the pump should do with it — drop it, deliver it after
//! a delay, or hold it for reordering. Ported from `TappedTransport` in the
//! playground's `transport.ts`; the `duplex()` inner transport and the `async`
//! `clock.sleep` are gone — the pump schedules delivery on the [`Clock`].
//!
//! [`Clock`]: crate::clock::Clock

use crate::faults::{corrupt_bytes, fault_applies_to, Direction, FaultSpec};
use crate::frame::{classify, FrameKind, Tap};
use crate::rng::Mulberry32;

/// A held reorder batch is released after this long if it hasn't filled up
/// (`transport.ts`: `clock.after(40, …)`).
pub const REORDER_FLUSH_MS: f64 = 40.0;

/// What [`Channel::send`] decided; the pump acts on it.
#[derive(Debug, PartialEq)]
pub enum SendOutcome {
    /// Recorded on the tap, not delivered.
    Dropped,
    /// Deliver these frames to the peer after `delay_ms` of clock time. One
    /// frame for the normal path; a whole shuffled batch when a reorder buffer
    /// fills up (then `delay_ms` is 0 — a flushed frame goes straight out).
    Deliver { frames: Vec<Vec<u8>>, delay_ms: f64 },
    /// Held in the reorder buffer. `schedule_flush` is true when no flush timer
    /// was pending and the pump should queue one `REORDER_FLUSH_MS` out.
    Buffered { schedule_flush: bool },
}

impl SendOutcome {
    fn deliver_one(bytes: Vec<u8>, delay_ms: f64) -> Self {
        Self::Deliver {
            frames: vec![bytes],
            delay_ms,
        }
    }
}

/// One connection between a named client and a named server.
pub struct Channel {
    pub tap: Tap,
    client_name: String,
    server_name: String,
    faults: FaultSpec,
    latency_ms: f64,
    reorder_buf: Vec<Vec<u8>>,
    /// A flush timer is already queued — don't queue another (mirrors
    /// `transport.ts`'s `cancelReorder ??= …`).
    flush_pending: bool,
}

impl Channel {
    pub fn new(client_name: impl Into<String>, server_name: impl Into<String>) -> Self {
        Self {
            tap: Tap::new(),
            client_name: client_name.into(),
            server_name: server_name.into(),
            faults: FaultSpec::default(),
            latency_ms: 0.0,
            reorder_buf: Vec::new(),
            flush_pending: false,
        }
    }

    /// The fault spec, mutable in place — a tweak takes effect on the next frame
    /// with no reconnect, exactly as the inspector expects.
    pub fn faults_mut(&mut self) -> &mut FaultSpec {
        &mut self.faults
    }

    pub fn faults(&self) -> &FaultSpec {
        &self.faults
    }

    pub fn set_latency(&mut self, ms: f64) {
        self.latency_ms = ms.max(0.0);
    }

    /// Record a frame travelling in `dir` at clock time `now` and decide its
    /// fate against the fault spec. `rng` supplies the drop / corrupt / jitter
    /// rolls, in that order (matching `transport.ts`).
    pub fn send(
        &mut self,
        dir: Direction,
        bytes: &[u8],
        now: f64,
        rng: &mut Mulberry32,
    ) -> SendOutcome {
        let (from, to) = match dir {
            Direction::Request => (self.client_name.clone(), self.server_name.clone()),
            Direction::Response => (self.server_name.clone(), self.client_name.clone()),
        };
        let hint = match dir {
            Direction::Request => FrameKind::Request,
            Direction::Response => FrameKind::Response,
        };
        let is_handshake = classify(bytes) == FrameKind::Handshake;

        // Partition cuts everything, handshakes included.
        if self.faults.partition {
            self.tap.record(
                from,
                to,
                bytes,
                now,
                hint,
                Some("dropped · partition".into()),
            );
            return SendOutcome::Dropped;
        }

        // Handshakes and out-of-scope directions only see the latency knob.
        if is_handshake || !fault_applies_to(&self.faults, dir) {
            self.tap.record(from, to, bytes, now, hint, None);
            return SendOutcome::deliver_one(bytes.to_vec(), self.latency_ms);
        }

        if rng.chance(self.faults.drop_prob) {
            self.tap
                .record(from, to, bytes, now, hint, Some("dropped".into()));
            return SendOutcome::Dropped;
        }

        let mut out = bytes.to_vec();
        let mut notes: Vec<String> = Vec::new();

        if self.faults.corrupt_prob > 0.0 && rng.chance(self.faults.corrupt_prob) {
            out = corrupt_bytes(bytes, rng);
            notes.push("corrupted".into());
        }

        let range = (self.faults.delay_max - self.faults.delay_min).max(0.0);
        let jitter = if self.faults.delay_max > 0.0 {
            self.faults.delay_min + rng.next_f64() * range
        } else {
            0.0
        };
        let delay = self.latency_ms + jitter;
        if jitter > 0.0 {
            notes.push(format!("+{} ms", jitter.round() as i64));
        }

        let note = (!notes.is_empty()).then(|| notes.join(" · "));
        self.tap.record(from, to, &out, now, hint, note);

        if self.faults.reorder_window > 0 {
            self.reorder_buf.push(out);
            if self.reorder_buf.len() as u32 >= self.faults.reorder_window {
                return SendOutcome::Deliver {
                    frames: self.flush_reorder(rng),
                    delay_ms: 0.0,
                };
            }
            let schedule_flush = !self.flush_pending;
            self.flush_pending = true;
            return SendOutcome::Buffered { schedule_flush };
        }

        SendOutcome::deliver_one(out, delay)
    }

    /// Drain the reorder buffer, shuffled (Fisher-Yates with `rng`, matching
    /// `transport.ts`). Also fired by a flush timer — then the buffer may be
    /// empty and this returns nothing.
    pub fn flush_reorder(&mut self, rng: &mut Mulberry32) -> Vec<Vec<u8>> {
        let mut batch = std::mem::take(&mut self.reorder_buf);
        for i in (1..batch.len()).rev() {
            let j = (rng.next_f64() * (i as f64 + 1.0)).floor() as usize;
            batch.swap(i, j);
        }
        self.flush_pending = false;
        batch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::faults::FaultDir;

    fn rng() -> Mulberry32 {
        Mulberry32::new(1)
    }

    #[test]
    fn a_clean_channel_delivers_immediately() {
        let mut ch = Channel::new("client", "server");
        let out = ch.send(Direction::Request, b"hello", 0.0, &mut rng());
        assert_eq!(
            out,
            SendOutcome::Deliver {
                frames: vec![b"hello".to_vec()],
                delay_ms: 0.0
            }
        );
        assert_eq!(ch.tap.frames.len(), 1);
        assert_eq!(ch.tap.frames[0].from, "client");
        assert_eq!(ch.tap.frames[0].to, "server");
        assert!(ch.tap.frames[0].fault.is_none());
    }

    #[test]
    fn latency_becomes_the_delivery_delay() {
        let mut ch = Channel::new("client", "server");
        ch.set_latency(50.0);
        let out = ch.send(Direction::Response, b"pong", 10.0, &mut rng());
        assert_eq!(
            out,
            SendOutcome::Deliver {
                frames: vec![b"pong".to_vec()],
                delay_ms: 50.0
            }
        );
    }

    #[test]
    fn a_total_drop_is_recorded_but_not_delivered() {
        let mut ch = Channel::new("client", "server");
        ch.faults_mut().drop_prob = 1.0;
        let out = ch.send(Direction::Request, b"hello", 0.0, &mut rng());
        assert_eq!(out, SendOutcome::Dropped);
        assert_eq!(ch.tap.frames[0].fault.as_deref(), Some("dropped"));
    }

    #[test]
    fn a_partition_drops_even_a_handshake() {
        let mut ch = Channel::new("client", "server");
        ch.faults_mut().partition = true;
        let mut hs = vec![0u8; 31];
        hs[0] = b'C';
        hs[1] = b'O';
        let out = ch.send(Direction::Request, &hs, 0.0, &mut rng());
        assert_eq!(out, SendOutcome::Dropped);
        assert_eq!(
            ch.tap.frames[0].fault.as_deref(),
            Some("dropped · partition")
        );
    }

    #[test]
    fn apply_to_keeps_the_other_direction_clean() {
        let mut ch = Channel::new("client", "server");
        ch.faults_mut().drop_prob = 1.0;
        ch.faults_mut().apply_to = FaultDir::Responses;

        // a request is out of scope — delivered clean despite drop_prob = 1
        let out = ch.send(Direction::Request, b"req", 0.0, &mut rng());
        assert!(matches!(out, SendOutcome::Deliver { .. }));
        assert!(ch.tap.frames[0].fault.is_none());

        // a response is in scope — dropped
        let out = ch.send(Direction::Response, b"res", 0.0, &mut rng());
        assert_eq!(out, SendOutcome::Dropped);
    }

    #[test]
    fn corruption_flips_a_byte_and_annotates_the_frame() {
        let mut ch = Channel::new("client", "server");
        ch.faults_mut().corrupt_prob = 1.0;
        let original = b"aaaaaaaaaaaaaaaa";
        let out = ch.send(Direction::Request, original, 0.0, &mut rng());
        match out {
            SendOutcome::Deliver { frames, delay_ms } => {
                assert_eq!(delay_ms, 0.0);
                assert_ne!(frames[0], original, "a byte was flipped");
            }
            other => panic!("expected delivery, got {other:?}"),
        }
        assert_eq!(ch.tap.frames[0].fault.as_deref(), Some("corrupted"));
    }

    #[test]
    fn jitter_delays_delivery_and_notes_the_millis() {
        let mut ch = Channel::new("client", "server");
        ch.faults_mut().delay_min = 100.0;
        ch.faults_mut().delay_max = 100.0; // fixed 100 ms, no randomness
        let out = ch.send(Direction::Request, b"x", 0.0, &mut rng());
        match out {
            SendOutcome::Deliver { delay_ms, .. } => assert_eq!(delay_ms, 100.0),
            other => panic!("expected delivery, got {other:?}"),
        }
        assert_eq!(ch.tap.frames[0].fault.as_deref(), Some("+100 ms"));
    }

    #[test]
    fn reorder_buffers_until_the_window_fills_then_flushes_the_batch() {
        let mut ch = Channel::new("client", "server");
        ch.faults_mut().reorder_window = 3;
        let mut r = rng();

        let a = ch.send(Direction::Request, b"a", 0.0, &mut r);
        assert_eq!(
            a,
            SendOutcome::Buffered {
                schedule_flush: true
            }
        );
        let b = ch.send(Direction::Request, b"b", 0.0, &mut r);
        assert_eq!(
            b,
            SendOutcome::Buffered {
                schedule_flush: false
            }
        );

        let c = ch.send(Direction::Request, b"c", 0.0, &mut r);
        match c {
            SendOutcome::Deliver { frames, delay_ms } => {
                assert_eq!(delay_ms, 0.0);
                let mut sorted = frames.clone();
                sorted.sort();
                assert_eq!(sorted, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
            }
            other => panic!("expected a flushed batch, got {other:?}"),
        }
        assert_eq!(ch.tap.frames.len(), 3, "every buffered frame was recorded");

        // buffer drained — the next frame schedules a fresh flush timer
        let d = ch.send(Direction::Request, b"d", 0.0, &mut r);
        assert_eq!(
            d,
            SendOutcome::Buffered {
                schedule_flush: true
            }
        );
    }

    #[test]
    fn a_flush_on_an_empty_buffer_yields_nothing() {
        let mut ch = Channel::new("client", "server");
        assert!(ch.flush_reorder(&mut rng()).is_empty());
    }
}
