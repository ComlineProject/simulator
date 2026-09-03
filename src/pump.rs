//! The single-threaded pump: one connection's worth of clock, RNG, [`Channel`],
//! and server dispatcher, driven as a discrete-event simulation. A call doesn't
//! block — [`Pump::call`] frames a request, puts it on the wire, and returns its
//! id; the reply lands later when a delivery event brings the response frame
//! back and [`Pump::result`] can read it.
//!
//! This is where the playground's `engine.ts` / `generic.ts` round-trip lives,
//! minus the `async` `Client` / `Server`: the runtime's `Framing` primitives
//! plus [`GenericDispatch`] are enough when the pump owns the schedule.
//!
//! Scope note: still one hard-wired `client ↔ server` connection. Many
//! connections, nodes and the incremental `Wires` diff come with the `model` /
//! `engine` port.

use std::collections::HashMap;

use comline_runtime::contract::{
    Call, DatagramFraming, Envelope, Framing, RuntimeError, WireFormat,
};
use serde_json::Value;

use crate::behavior::{Behavior, BehaviorStep, ReplyWith, SimOutcome};
use crate::clock::Clock;
use crate::faults::{Direction, FaultSpec};
use crate::format::Json;
use crate::frame::Tap;
use crate::generic::{BehaviorMap, GenericDispatch};
use crate::rng::Mulberry32;
use crate::shape::{
    find_protocol, zero_value, FnShape, Framing as ShapeFraming, ProjectShape, ProtocolShape,
    SchemaShape, TypeDef, TypeRef,
};
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
    /// A `delay` behaviour's timer elapsed — frame and send its reply now.
    CompleteReply {
        request_id: u64,
        outcome: SimOutcome,
    },
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
    dispatch: GenericDispatch,
    behaviors: BehaviorMap,
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
    /// A pump over the built-in chat protocol, `send` seeded to reply with a
    /// fixed message — the shape `smoke()` and the tests drive.
    pub fn new() -> Self {
        let mut pump = Self::from_shape(&chat_shape(), "chat", "Chat")
            .expect("the built-in chat shape has a Chat protocol");
        pump.set_behavior(
            "send",
            Box::new(ReplyWith {
                value: serde_json::json!({ "body": "HELLO", "seq": 1 }),
            }),
        );
        pump
    }

    /// A pump for the `ns::protocol` of a compiled project. Every function is
    /// seeded with "reply with the zero value of its return type"; the caller
    /// then swaps in the behaviours the session wants. `None` if the protocol
    /// isn't in the shape, or isn't datagram-framed (JSON-RPC comes later).
    pub fn from_shape(shape: &ProjectShape, ns: &str, protocol: &str) -> Option<Self> {
        let (schema, proto) = find_protocol(shape, ns, protocol)?;
        if proto.framing != ShapeFraming::Datagram {
            return None;
        }
        Some(Self {
            clock: Clock::new(),
            rng: Mulberry32::new(1),
            channel: Channel::new("client", "server"),
            behaviors: default_behaviors(proto, &schema.types),
            dispatch: GenericDispatch::new(proto.clone()),
            fmt: Json,
            framing: DatagramFraming,
            next_request_id: 0,
            results: HashMap::new(),
        })
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

    /// Swap the server behaviour for `fn_name`; takes effect on the next call.
    pub fn set_behavior(&mut self, fn_name: &str, behavior: Box<dyn Behavior>) {
        self.behaviors.insert(fn_name.to_string(), behavior);
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

    pub fn protocol(&self) -> &ProtocolShape {
        self.dispatch.protocol()
    }

    /// The outcome of the call with request id `id`, once it has settled.
    pub fn result(&self, id: u64) -> Option<&CallResult> {
        self.results.get(&id)
    }

    // ── driving ──────────────────────────────────────────────────────────

    /// Frame a call to the function named `fn_name` with `params` and put the
    /// request on the wire. Returns the request id, or [`RuntimeError::UnknownCall`]
    /// if the protocol has no such function.
    pub fn call(&mut self, fn_name: &str, params: &Value) -> Result<u64, RuntimeError> {
        let function = self
            .dispatch
            .protocol()
            .functions
            .iter()
            .find(|f| f.name == fn_name)
            .ok_or(RuntimeError::UnknownCall)?;
        let call_id = function.index as u16;
        let one_way = function.oneway;

        self.next_request_id += 1;
        let id = self.next_request_id;
        let mut frame = Vec::new();
        self.framing
            .encode_request(Call::new(call_id, ""), id, params, &self.fmt, &mut frame)?;
        self.emit(Direction::Request, frame);

        // a one-way call never gets a response — settle it now so callers that
        // poll `result` aren't left waiting.
        if one_way {
            self.results.insert(id, CallResult::Ok(Value::Null));
        }
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
            Event::CompleteReply {
                request_id,
                outcome,
            } => self.reply_now(request_id, outcome),
        }
    }

    /// Server side: decode the request, run its behaviour, and act on the step —
    /// reply now, schedule a delayed reply, hang, or (with no engine to relay
    /// on) answer a `forward` with the "only in the engine" error.
    fn serve(&mut self, request_frame: &[u8]) {
        let Some(req) = self.framing.decode_request(request_frame) else {
            return;
        };
        let request_id = req.request_id;

        let step =
            match self
                .dispatch
                .dispatch(req.call, req.params, &self.fmt, &mut self.behaviors)
            {
                Ok(step) => step,
                // unknown call / undecodable params — no response, the call times out
                Err(_) => return,
            };

        match step {
            BehaviorStep::Now(outcome) => self.reply_now(request_id, outcome),
            BehaviorStep::After { delay_ms, outcome } => {
                self.clock.schedule(
                    delay_ms,
                    Event::CompleteReply {
                        request_id,
                        outcome,
                    },
                );
            }
            BehaviorStep::Hang => {} // client's call stays pending, like a dead peer
            BehaviorStep::Forward { .. } => self.reply_now(
                request_id,
                SimOutcome::Err {
                    ordinal: 0,
                    data: serde_json::json!({ "error": "forwarding is only available in the engine" }),
                },
            ),
        }
    }

    /// Frame `outcome` as a response to `request_id` and put it on the wire.
    fn reply_now(&mut self, request_id: u64, outcome: SimOutcome) {
        let mut resp = Vec::new();
        match outcome {
            SimOutcome::Ok(value) => {
                let mut body = Vec::new();
                if self.fmt.encode(&value, &mut body).is_err() {
                    return;
                }
                self.framing
                    .encode_response_ok(request_id, &body, &mut resp);
            }
            SimOutcome::Err { ordinal, data } => {
                let mut body = Vec::new();
                if self.fmt.encode(&data, &mut body).is_err() {
                    return;
                }
                self.framing
                    .encode_response_err(request_id, ordinal, &body, &mut resp);
            }
            SimOutcome::None => return, // one-way — nothing to send back
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

/// "Reply with the zero value of the return type" for every function — the
/// starting behaviour a freshly-placed server instance runs.
fn default_behaviors(proto: &ProtocolShape, types: &[TypeDef]) -> BehaviorMap {
    proto
        .functions
        .iter()
        .map(|f| {
            let value = f
                .returns
                .as_ref()
                .map_or(Value::Null, |r| zero_value(r, types));
            (
                f.name.clone(),
                Box::new(ReplyWith { value }) as Box<dyn Behavior>,
            )
        })
        .collect()
}

/// The built-in chat shape `Pump::new` drives — the same one the `shape.rs`
/// fixture pins the deserialization of.
fn chat_shape() -> ProjectShape {
    let string = || TypeRef::Prim {
        name: "string".into(),
    };
    ProjectShape {
        schemas: vec![SchemaShape {
            namespace: "chat".into(),
            ir_hash: "0x9f2b1c7d4e6a8035".into(),
            protocols: vec![ProtocolShape {
                name: "Chat".into(),
                framing: ShapeFraming::Datagram,
                functions: vec![FnShape {
                    name: "send".into(),
                    index: 0,
                    oneway: false,
                    args: vec![crate::shape::ArgShape {
                        name: "text".into(),
                        ty: string(),
                    }],
                    returns: Some(TypeRef::Ref {
                        name: "Message".into(),
                    }),
                    throws: vec![],
                }],
            }],
            errors: vec![],
            types: vec![TypeDef::Struct {
                name: "Message".into(),
                fields: vec![
                    crate::shape::FieldShape {
                        name: "body".into(),
                        ty: string(),
                        optional: false,
                    },
                    crate::shape::FieldShape {
                        name: "seq".into(),
                        ty: TypeRef::Prim { name: "u64".into() },
                        optional: false,
                    },
                ],
            }],
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::behavior::{BehaviorKind, Echo};
    use serde_json::json;

    fn hello() -> Value {
        json!({ "body": "HELLO", "seq": 1 })
    }

    /// The `send` function + its schema, for building behaviours in tests.
    fn chat_send() -> (crate::shape::SchemaShape, crate::shape::FnShape) {
        let shape = chat_shape();
        let schema = shape.schemas.into_iter().next().unwrap();
        let function = schema.protocols[0].functions[0].clone();
        (schema, function)
    }

    #[test]
    fn a_call_round_trips_over_the_real_contract() {
        let mut pump = Pump::new();
        let id = pump.call("send", &json!(["hi"])).unwrap();
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
    fn calls_resolve_by_function_name() {
        let mut pump = Pump::new();
        assert_eq!(
            pump.call("nope", &json!([])).unwrap_err(),
            RuntimeError::UnknownCall
        );
    }

    #[test]
    fn a_generic_dispatch_behaviour_swap_takes_effect_next_call() {
        let mut pump = Pump::new();
        pump.set_behavior("send", Box::new(Echo));

        let id = pump.call("send", &json!(["echo me"])).unwrap();
        pump.run();
        assert_eq!(pump.result(id), Some(&CallResult::Ok(json!(["echo me"]))));
    }

    #[test]
    fn from_shape_seeds_the_zero_value_reply() {
        let shape: ProjectShape =
            serde_json::from_str(include_str!("../tests/fixtures/chat.describe.json")).unwrap();
        let mut pump = Pump::from_shape(&shape, "chat", "Chat").unwrap();

        let id = pump.call("send", &json!(["hi"])).unwrap();
        pump.run();
        // Message's zero value: body "", seq 0
        assert_eq!(
            pump.result(id),
            Some(&CallResult::Ok(json!({ "body": "", "seq": 0 })))
        );
    }

    #[test]
    fn from_shape_is_none_for_a_missing_protocol() {
        let shape = chat_shape();
        assert!(Pump::from_shape(&shape, "chat", "Missing").is_none());
        assert!(Pump::from_shape(&shape, "nope", "Chat").is_none());
    }

    #[test]
    fn latency_shows_up_as_virtual_time() {
        let mut pump = Pump::new();
        pump.set_latency(50.0);
        let id = pump.call("send", &json!(["hi"])).unwrap();
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

        let id = pump.call("send", &json!(["hi"])).unwrap();
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

        let id = pump.call("send", &json!(["hi"])).unwrap();
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
        let id = pump.call("send", &json!(["hi"])).unwrap();

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
        let id = pump.call("send", &json!(["hi"])).unwrap();

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
                let _ = pump.call("send", &json!(["hi"]));
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

    #[test]
    fn a_delay_behaviour_defers_the_reply_in_virtual_time() {
        let (schema, function) = chat_send();
        let mut pump = Pump::new();
        pump.set_latency(20.0);
        pump.set_behavior(
            "send",
            BehaviorKind::Delay.make(&json!({ "ms": 500, "value": hello() }), &function, &schema),
        );

        let id = pump.call("send", &json!(["hi"])).unwrap();

        pump.advance(100.0); // request lands at 20, delay timer set for 520
        assert_eq!(pump.result(id), None);

        pump.advance(1000.0);
        assert_eq!(pump.result(id), Some(&CallResult::Ok(hello())));
        // request delivered (20) + behaviour delay (500) + response delivered (20)
        assert_eq!(pump.tap().frames[1].at, 520.0);
        assert_eq!(pump.now(), 1100.0);
    }

    #[test]
    fn a_drop_behaviour_hangs_the_call() {
        let (schema, function) = chat_send();
        let mut pump = Pump::new();
        pump.set_behavior(
            "send",
            BehaviorKind::Drop.make(&json!({}), &function, &schema),
        );

        let id = pump.call("send", &json!(["hi"])).unwrap();
        pump.run();

        assert_eq!(pump.result(id), None, "no reply ever comes");
        assert_eq!(pump.tap().frames.len(), 1, "only the request was sent");
    }

    #[test]
    fn a_raise_behaviour_settles_the_call_as_an_error() {
        let (schema, function) = chat_send();
        let mut pump = Pump::new();
        pump.set_behavior(
            "send",
            BehaviorKind::Raise.make(
                &json!({ "ordinal": 3, "data": { "why": "nope" } }),
                &function,
                &schema,
            ),
        );

        let id = pump.call("send", &json!(["hi"])).unwrap();
        pump.run();

        assert_eq!(
            pump.result(id),
            Some(&CallResult::Err {
                ordinal: 3,
                body: json!({ "why": "nope" })
            })
        );
    }

    #[test]
    fn increment_bumps_the_reply_across_calls() {
        let (schema, function) = chat_send();
        let mut pump = Pump::new();
        pump.set_behavior(
            "send",
            BehaviorKind::Increment.make(
                &json!({ "base": { "body": "", "seq": 0 }, "path": "seq" }),
                &function,
                &schema,
            ),
        );

        let a = pump.call("send", &json!(["hi"])).unwrap();
        pump.run();
        let b = pump.call("send", &json!(["hi"])).unwrap();
        pump.run();

        assert_eq!(
            pump.result(a),
            Some(&CallResult::Ok(json!({ "body": "", "seq": 1 })))
        );
        assert_eq!(
            pump.result(b),
            Some(&CallResult::Ok(json!({ "body": "", "seq": 2 })))
        );
    }

    #[test]
    fn a_forward_behaviour_without_an_engine_errors() {
        let (schema, function) = chat_send();
        let mut pump = Pump::new();
        pump.set_behavior(
            "send",
            BehaviorKind::Forward.make(
                &json!({ "viaConnectionId": "c2", "targetFn": "send" }),
                &function,
                &schema,
            ),
        );

        let id = pump.call("send", &json!(["hi"])).unwrap();
        pump.run();

        match pump.result(id) {
            Some(CallResult::Err { ordinal: 0, body }) => {
                assert!(body["error"].as_str().unwrap().contains("engine"));
            }
            other => panic!("expected an engine error, got {other:?}"),
        }
    }
}
