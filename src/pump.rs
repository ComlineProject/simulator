//! The single-threaded pump: one connection's worth of clock, RNG, [`Channel`],
//! and server dispatcher, driven as a discrete-event simulation. A call doesn't
//! block — [`Pump::call`] frames a request, puts it on the wire, and returns its
//! id; the reply lands later when a delivery event brings the response frame
//! back and [`Pump::result`] can read it.
//!
//! This is where the playground's `engine.ts` / `generic.ts` round-trip lives,
//! minus the `async` `Client` / `Server`: the runtime's `Framing` + `Dispatch`
//! primitives are enough when the pump owns the schedule.
//!
//! Scope note: one hard-wired `client ↔ server` connection and a
//! reply-with-a-constant [`ConstDispatch`]. The next slice swaps in a
//! `GenericDispatch` built from the compiled IR and a client keyed by function.

use std::collections::HashMap;

use comline_runtime::contract::{
    Call, DatagramFraming, Dispatch, Envelope, Framing, Kind, Outcome, Reply, RequestCall,
    RuntimeError, WireFormat,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::clock::Clock;
use crate::faults::{Direction, FaultSpec};
use crate::format::Json;
use crate::frame::Tap;
use crate::rng::Mulberry32;
use crate::wire::{Channel, SendOutcome, REORDER_FLUSH_MS};

/// What a settled call produced.
#[derive(Debug, Clone, PartialEq)]
pub enum CallResult {
    /// A decoded success value.
    Ok(Value),
    /// A raised schema error: its schema-global `ordinal` and decoded `body`.
    Err { ordinal: u16, body: Value },
    /// The response frame arrived but its body would not decode — e.g. a
    /// corrupted wire. The call never gets a clean value.
    Undecodable(String),
}

/// A scheduled step of the simulation.
enum Event {
    /// A request frame has arrived at the server.
    DeliverToServer(Vec<u8>),
    /// A response frame has arrived at the client.
    DeliverToClient(Vec<u8>),
    /// A connection's reorder buffer should be released now.
    FlushReorder(Direction),
}

/// The delivery event a frame travelling in `dir` turns into.
fn deliver_event(dir: Direction, frame: Vec<u8>) -> Event {
    match dir {
        Direction::Request => Event::DeliverToServer(frame),
        Direction::Response => Event::DeliverToClient(frame),
    }
}

pub struct Pump {
    clock: Clock<Event>,
    rng: Mulberry32,
    channel: Channel,
    dispatch: ConstDispatch,
    fmt: Json,
    framing: DatagramFraming,
    next_request_id: u64,
    results: HashMap<u64, CallResult>,
}

impl Default for Pump {
    fn default() -> Self {
        Self::new()
    }
}

impl Pump {
    pub fn new() -> Self {
        Self {
            clock: Clock::new(),
            rng: Mulberry32::new(1),
            channel: Channel::new("client", "server"),
            dispatch: ConstDispatch::new(),
            fmt: Json,
            framing: DatagramFraming,
            next_request_id: 0,
            results: HashMap::new(),
        }
    }

    // ── knobs ─────────────────────────────────────────────────────────────

    pub fn faults_mut(&mut self) -> &mut FaultSpec {
        self.channel.faults_mut()
    }

    pub fn set_latency(&mut self, ms: f64) {
        self.channel.set_latency(ms);
    }

    /// Re-seed the fault RNG. A stepped run from a fixed seed is reproducible.
    pub fn set_seed(&mut self, seed: u32) {
        self.rng = Mulberry32::new(seed);
    }

    // ── observation ──────────────────────────────────────────────────────

    pub fn now(&self) -> f64 {
        self.clock.now()
    }

    pub fn pending(&self) -> usize {
        self.clock.pending()
    }

    pub fn tap(&self) -> &Tap {
        &self.channel.tap
    }

    /// The outcome of the call with request id `id`, once it has settled.
    pub fn result(&self, id: u64) -> Option<&CallResult> {
        self.results.get(&id)
    }

    // ── driving ──────────────────────────────────────────────────────────

    /// Frame a call to function `call_id` with `params` and put the request on
    /// the wire. Returns the request id to read the result back with.
    pub fn call(&mut self, call_id: u16, params: &Value) -> Result<u64, RuntimeError> {
        self.next_request_id += 1;
        let id = self.next_request_id;
        let mut frame = Vec::new();
        self.framing
            .encode_request(Call::from(call_id), id, params, &self.fmt, &mut frame)?;
        self.emit(Direction::Request, frame);
        Ok(id)
    }

    /// Run until the event queue is empty (every in-flight call has settled or
    /// been dropped). The idiom for a test or a `real`-mode step.
    pub fn run(&mut self) {
        while let Some(ev) = self.clock.pop_next() {
            self.apply(ev);
        }
    }

    /// Fire the single earliest event. `false` if the queue was empty.
    pub fn step(&mut self) -> bool {
        match self.clock.pop_next() {
            Some(ev) => {
                self.apply(ev);
                true
            }
            None => false,
        }
    }

    /// Advance virtual time by `ms`, firing every event that comes due within
    /// the window (including ones scheduled by events fired in the same call),
    /// then park time at the window edge. This is the `requestAnimationFrame`
    /// / playback path.
    pub fn advance(&mut self, ms: f64) {
        let until = self.clock.now() + ms.max(0.0);
        while self.clock.peek_due().is_some_and(|t| t <= until) {
            let ev = self.clock.pop_next().unwrap();
            self.apply(ev);
        }
        self.clock.park_at_least(until);
    }

    // ── internals ────────────────────────────────────────────────────────

    /// Hand a frame to the channel and schedule whatever it decided.
    fn emit(&mut self, dir: Direction, frame: Vec<u8>) {
        match self
            .channel
            .send(dir, &frame, self.clock.now(), &mut self.rng)
        {
            SendOutcome::Dropped => {}
            SendOutcome::Deliver { frames, delay_ms } => {
                for f in frames {
                    self.clock.schedule(delay_ms, deliver_event(dir, f));
                }
            }
            SendOutcome::Buffered { schedule_flush } => {
                if schedule_flush {
                    self.clock
                        .schedule(REORDER_FLUSH_MS, Event::FlushReorder(dir));
                }
            }
        }
    }

    fn apply(&mut self, ev: Event) {
        match ev {
            Event::DeliverToServer(frame) => self.serve(&frame),
            Event::DeliverToClient(frame) => self.receive(&frame),
            Event::FlushReorder(dir) => {
                for f in self.channel.flush_reorder(&mut self.rng) {
                    self.clock.schedule(0.0, deliver_event(dir, f));
                }
            }
        }
    }

    /// Server side: decode the request, dispatch it, frame the response, and put
    /// it back on the wire.
    fn serve(&mut self, request_frame: &[u8]) {
        let Some(req) = self.framing.decode_request(request_frame) else {
            return;
        };
        let call = match req.call {
            RequestCall::Id(id) => Kind::Id(id),
            // datagram framing never puts a name on the wire
            RequestCall::Name(_) => return,
        };
        let request_id = req.request_id;

        let mut body = Vec::new();
        let outcome = {
            let mut reply = Reply::new(&mut body);
            match self
                .dispatch
                .dispatch(call, req.params, &self.fmt, &mut reply)
            {
                Ok(()) => reply.outcome(),
                // the next slice frames this as an error response
                Err(_) => return,
            }
        };

        let mut resp = Vec::new();
        match outcome {
            Outcome::Ok => self
                .framing
                .encode_response_ok(request_id, &body, &mut resp),
            Outcome::Err(id) => self
                .framing
                .encode_response_err(request_id, id, &body, &mut resp),
            Outcome::None => return, // one-way — nothing to send back
        }
        self.emit(Direction::Response, resp);
    }

    /// Client side: decode the response frame and settle the matching call.
    fn receive(&mut self, response_frame: &[u8]) {
        let Some((request_id, envelope)) = self.framing.decode_response(response_frame) else {
            return;
        };
        let result = match envelope {
            Envelope::Ok(payload) => match self.fmt.decode::<Value>(payload) {
                Ok(value) => CallResult::Ok(value),
                Err(_) => CallResult::Undecodable("ok body did not decode".into()),
            },
            Envelope::Err { id, body } => {
                let body = if body.is_empty() {
                    Value::Null
                } else {
                    self.fmt.decode::<Value>(body).unwrap_or(Value::Null)
                };
                CallResult::Err { ordinal: id, body }
            }
        };
        self.results.insert(request_id, result);
    }
}

// ── a stand-in dispatcher: replies to `send` with a constant ──────────────
//
// Replaced in the next slice by a `GenericDispatch` reading the compiled IR.

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Message {
    body: String,
    seq: u64,
}

const CONST_CALLS: &[&str] = &["send"];

struct ConstDispatch {
    reply: Vec<u8>,
}

impl ConstDispatch {
    fn new() -> Self {
        let mut reply = Vec::new();
        Json.encode(
            &Message {
                body: "HELLO".into(),
                seq: 1,
            },
            &mut reply,
        )
        .expect("encode the canned reply");
        Self { reply }
    }
}

impl Dispatch for ConstDispatch {
    fn calls(&self) -> &'static [&'static str] {
        CONST_CALLS
    }

    fn dispatch<W: WireFormat>(
        &self,
        call: Kind,
        _params: &[u8],
        _format: &W,
        reply: &mut Reply,
    ) -> Result<(), RuntimeError> {
        match call.resolve(CONST_CALLS) {
            Some(0) => {
                reply.ok(&self.reply);
                Ok(())
            }
            _ => Err(RuntimeError::UnknownCall),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hello() -> Value {
        json!({ "body": "HELLO", "seq": 1 })
    }

    #[test]
    fn a_call_round_trips_over_the_real_contract() {
        let mut pump = Pump::new();
        let id = pump.call(0, &json!(["hi"])).unwrap();
        assert_eq!(pump.result(id), None, "not settled until the pump runs");

        pump.run();

        assert_eq!(pump.result(id), Some(&CallResult::Ok(hello())));
        let frames = &pump.tap().frames;
        assert_eq!(frames.len(), 2, "one request, one response");
        assert_eq!(
            (frames[0].from.as_str(), frames[0].to.as_str()),
            ("client", "server")
        );
        assert_eq!(
            (frames[1].from.as_str(), frames[1].to.as_str()),
            ("server", "client")
        );
    }

    #[test]
    fn latency_shows_up_as_virtual_time() {
        let mut pump = Pump::new();
        pump.set_latency(50.0);
        let id = pump.call(0, &json!(["hi"])).unwrap();
        pump.run();

        assert_eq!(pump.result(id), Some(&CallResult::Ok(hello())));
        assert_eq!(pump.now(), 100.0, "50 ms each way");
        assert_eq!(pump.tap().frames[0].at, 0.0);
        assert_eq!(pump.tap().frames[1].at, 50.0);
    }

    #[test]
    fn a_dropped_request_never_settles() {
        let mut pump = Pump::new();
        pump.faults_mut().drop_prob = 1.0;
        pump.faults_mut().apply_to = crate::faults::FaultDir::Requests;

        let id = pump.call(0, &json!(["hi"])).unwrap();
        pump.run();

        assert_eq!(pump.result(id), None);
        assert_eq!(pump.tap().frames.len(), 1);
        assert_eq!(pump.tap().frames[0].fault.as_deref(), Some("dropped"));
    }

    #[test]
    fn a_corrupted_response_settles_as_undecodable() {
        let mut pump = Pump::new();
        pump.faults_mut().corrupt_prob = 1.0;
        pump.faults_mut().apply_to = crate::faults::FaultDir::Responses;

        let id = pump.call(0, &json!(["hi"])).unwrap();
        pump.run();

        match pump.result(id) {
            Some(CallResult::Undecodable(_)) => {}
            other => panic!("expected Undecodable, got {other:?}"),
        }
        assert!(pump
            .tap()
            .frames
            .iter()
            .any(|f| f.fault.as_deref() == Some("corrupted")));
    }

    #[test]
    fn step_fires_one_event_at_a_time() {
        let mut pump = Pump::new();
        pump.set_latency(10.0);
        let id = pump.call(0, &json!(["hi"])).unwrap();

        assert_eq!(pump.pending(), 1, "request delivery queued");
        assert!(pump.step()); // deliver request → server replies → response queued
        assert_eq!(pump.pending(), 1);
        assert_eq!(pump.result(id), None);

        assert!(pump.step()); // deliver response → call settles
        assert_eq!(pump.result(id), Some(&CallResult::Ok(hello())));
        assert!(!pump.step(), "queue drained");
    }

    #[test]
    fn advance_fires_the_whole_chain_within_the_window() {
        let mut pump = Pump::new();
        pump.set_latency(30.0);
        let id = pump.call(0, &json!(["hi"])).unwrap();

        pump.advance(10.0); // nothing due yet (first delivery at t=30)
        assert_eq!(pump.result(id), None);
        assert_eq!(pump.now(), 10.0);

        pump.advance(1000.0); // request lands at 30, response at 60 — both inside
        assert_eq!(pump.result(id), Some(&CallResult::Ok(hello())));
        assert_eq!(pump.now(), 1010.0);
    }

    #[test]
    fn reseeding_keeps_a_run_reproducible() {
        let frames_for = |seed| {
            let mut pump = Pump::new();
            pump.set_seed(seed);
            pump.faults_mut().corrupt_prob = 0.5;
            for _ in 0..8 {
                let _ = pump.call(0, &json!(["hi"]));
                pump.run();
            }
            pump.tap()
                .frames
                .iter()
                .map(|f| (f.bytes.clone(), f.fault.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(frames_for(7), frames_for(7));
        assert_ne!(frames_for(7), frames_for(8));
    }
}
