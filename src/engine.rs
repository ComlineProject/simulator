//! The engine: many connections over one shared discrete-event [`Clock`], each a
//! tapped [`Channel`] + a [`GenericDispatch`] + its behaviour map. Ported from
//! `engine.ts`'s `Wires` / `connect`, minus the `async` `Client` / `Server` — a
//! call schedules a request-delivery event and settles later via [`Engine::result`].
//!
//! Cross-connection `forward` relays through a continuation map keyed by the
//! inner call's request id, with a `forwarding` set as the cycle guard — the
//! same shape as `Wires.forwardVia`, without parking a stack frame.
//!
//! A call with no reply inside its connection's timeout window settles as
//! [`CallResult::Timeout`] and the wire goes [`dead`](Engine::connection_dead)
//! (2c) — reopen it with [`Engine::rebuild`].
//!
//! [`Clock`]: crate::clock::Clock

use std::collections::{BTreeMap, HashMap, HashSet};

use comline_runtime::contract::{Envelope, Handshake, RuntimeError, WireFormat};
use serde_json::{json, Value};

use crate::behavior::{BehaviorKind, BehaviorStep, SimOutcome};
use crate::clock::Clock;
use crate::faults::{Direction, FaultSpec};
use crate::format::Codec;
use crate::frame::Tap;
use crate::framing::WireFraming;
use crate::generic::{BehaviorMap, GenericDispatch};
use crate::model::{Connection, FramingChoice, Instance, Session};
use crate::rng::Mulberry32;
use crate::shape::{find_protocol, Framing as ShapeFraming, ProtocolShape, SchemaShape};
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
    /// No reply within the connection's call-timeout window. The wire is left
    /// `dead` — see [`Engine::connection_dead`].
    Timeout,
}

/// A scheduled step of the simulation, tagged with the connection it belongs to.
enum Event {
    DeliverToServer {
        conn: String,
        frame: Vec<u8>,
    },
    DeliverToClient {
        conn: String,
        frame: Vec<u8>,
    },
    FlushReorder {
        conn: String,
        dir: Direction,
    },
    CompleteReply {
        conn: String,
        request_id: u64,
        outcome: SimOutcome,
    },
    /// A call's timeout window elapsed. If it hasn't settled, it times out and
    /// its wire goes `dead`.
    Timeout {
        conn: String,
        request_id: u64,
    },
}

fn deliver_event(conn: String, dir: Direction, frame: Vec<u8>) -> Event {
    match dir {
        Direction::Request => Event::DeliverToServer { conn, frame },
        Direction::Response => Event::DeliverToClient { conn, frame },
    }
}

/// One live connection.
struct Wire {
    channel: Channel,
    dispatch: GenericDispatch,
    behaviors: BehaviorMap,
    client_id: String,
    server_id: String,
    client_name: String,
    server_name: String,
    framing: WireFraming,
    codec: Codec,
    /// `None` when connected; otherwise why it was refused (`"handshake"` for a
    /// version / framing / wire-format mismatch).
    error: Option<String>,
    /// How long a call on this wire waits for its reply before timing out.
    /// `0` = wait forever.
    timeout_ms: f64,
    /// Set once a call has timed out — the client half is desynced, every later
    /// call fails fast until the connection is reopened.
    dead: bool,
}

/// A forwarded call in flight: when the inner call settles, answer the outer one.
struct ForwardCont {
    outer_conn: String,
    outer_request_id: u64,
    via: String,
}

/// A read-only view of one live connection.
pub struct WireInfo<'a> {
    pub client_id: &'a str,
    pub server_id: &'a str,
    pub client_name: &'a str,
    pub server_name: &'a str,
    pub fn_names: Vec<&'a str>,
    pub framing: WireFraming,
    pub codec: Codec,
    pub error: Option<&'a str>,
    pub dead: bool,
}

pub struct Engine {
    clock: Clock<Event>,
    rng: Mulberry32,
    wires: BTreeMap<String, Wire>,
    next_request_id: u64,
    results: HashMap<u64, CallResult>,
    forwards: HashMap<u64, ForwardCont>,
    forwarding: HashSet<String>,
    /// Requests issued and not yet settled — the timeout only fires for these.
    in_flight: HashSet<u64>,
    /// request id → the clock handle of its pending timeout, so settling a call
    /// early cancels it (otherwise a far-future timeout keeps the sim "busy").
    timeouts: HashMap<u64, u64>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Self {
            clock: Clock::new(),
            rng: Mulberry32::new(1),
            wires: BTreeMap::new(),
            next_request_id: 0,
            results: HashMap::new(),
            forwards: HashMap::new(),
            forwarding: HashSet::new(),
            in_flight: HashSet::new(),
            timeouts: HashMap::new(),
        }
    }

    // ── session wiring ───────────────────────────────────────────────────

    /// Match the live wire set to `session.connections`: open the ones that
    /// appeared, close the ones that vanished, leave the rest running.
    pub fn sync(&mut self, session: &Session) {
        let want: HashSet<&str> = session.connections.iter().map(|c| c.id.as_str()).collect();
        self.wires.retain(|id, _| want.contains(id.as_str()));
        for conn in &session.connections {
            if self.wires.contains_key(&conn.id) {
                continue;
            }
            let wire = self.connect(session, conn);
            self.wires.insert(conn.id.clone(), wire);
        }
    }

    /// Close every wire and re-open from scratch, re-seeding the fault RNG so a
    /// stepped run from `session.seed` is reproducible. The clock keeps running
    /// (time doesn't jump back on a schema / latency edit).
    pub fn rebuild(&mut self, session: &Session) {
        self.wires.clear();
        self.forwards.clear();
        self.forwarding.clear();
        self.in_flight.clear();
        self.timeouts.clear();
        self.rng = Mulberry32::new(session.seed);
        self.sync(session);
    }

    fn connect(&mut self, session: &Session, conn: &Connection) -> Wire {
        let client = session
            .instance(&conn.client_id)
            .expect("connect: client instance exists");
        let server = session
            .instance(&conn.server_id)
            .expect("connect: server instance exists");
        let (schema, proto) = find_protocol(&session.shape, &server.schema_ns, &server.protocol)
            .expect("connect: the server's protocol is compiled");

        let mut channel = Channel::new(client.name.clone(), server.name.clone());
        channel.set_latency(session.latency_ms);
        *channel.faults_mut() = conn.faults.clone();

        let behaviors = build_behaviors(server, proto, schema);

        // Framing: the connection's choice, else the protocol's. Codec: the
        // connection's. JSON-RPC is a JSON text envelope — it can't carry a
        // MessagePack body, so that pair is refused.
        let framing = match conn.framing {
            FramingChoice::Auto => match proto.framing {
                ShapeFraming::Jsonrpc => WireFraming::Jsonrpc,
                ShapeFraming::Datagram => WireFraming::Datagram,
            },
            FramingChoice::Datagram => WireFraming::Datagram,
            FramingChoice::Jsonrpc => WireFraming::Jsonrpc,
        };
        let codec = conn.wire_format;
        let bad_combo = framing == WireFraming::Jsonrpc && codec == Codec::Msgpack;

        // Handshake: record a frame each way (the inspector shows them) and
        // refuse on a mismatch. The two ends always agree on codec + framing, so
        // only the IR hash can disagree — the version-skew demo. A partition
        // cuts the handshake too → also a refusal.
        let error = {
            let c_hs = handshake_frame(&client.ir_hash, codec.name(), framing.name());
            let s_hs = handshake_frame(&server.ir_hash, codec.name(), framing.name());
            let c = channel.send(Direction::Request, &c_hs, 0.0, &mut self.rng);
            let s = channel.send(Direction::Response, &s_hs, 0.0, &mut self.rng);
            let cut = matches!(c, SendOutcome::Dropped) || matches!(s, SendOutcome::Dropped);
            if bad_combo {
                Some("json-rpc framing requires the json codec".to_string())
            } else if cut || client.ir_hash != server.ir_hash {
                Some("handshake".to_string())
            } else {
                None
            }
        };

        Wire {
            channel,
            dispatch: GenericDispatch::new(proto.clone()),
            behaviors,
            client_id: client.id.clone(),
            server_id: server.id.clone(),
            client_name: client.name.clone(),
            server_name: server.name.clone(),
            framing,
            codec,
            error,
            timeout_ms: session.call_timeout_ms.max(0.0),
            dead: false,
        }
    }

    // ── knobs ────────────────────────────────────────────────────────────

    /// Swap a live server behaviour; takes effect on the next call.
    pub fn set_behavior(
        &mut self,
        conn_id: &str,
        fn_name: &str,
        kind: BehaviorKind,
        config: &Value,
        schema: &SchemaShape,
    ) -> Result<(), String> {
        let wire = self
            .wires
            .get_mut(conn_id)
            .ok_or_else(|| format!("no connection {conn_id}"))?;
        let function = wire
            .dispatch
            .protocol()
            .functions
            .iter()
            .find(|f| f.name == fn_name)
            .ok_or_else(|| format!("no function {fn_name}"))?
            .clone();
        wire.behaviors
            .insert(fn_name.to_string(), kind.make(config, &function, schema));
        Ok(())
    }

    /// Replace a live connection's fault spec (edited in the inspector, no
    /// reconnect).
    pub fn set_faults(&mut self, conn_id: &str, spec: FaultSpec) {
        if let Some(wire) = self.wires.get_mut(conn_id) {
            *wire.channel.faults_mut() = spec;
        }
    }

    /// Re-seed the fault RNG.
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

    pub fn result(&self, request_id: u64) -> Option<&CallResult> {
        self.results.get(&request_id)
    }

    pub fn tap(&self, conn_id: &str) -> Option<&Tap> {
        self.wires.get(conn_id).map(|w| &w.channel.tap)
    }

    /// Why a connection was refused, if it was.
    pub fn connection_error(&self, conn_id: &str) -> Option<&str> {
        self.wires.get(conn_id).and_then(|w| w.error.as_deref())
    }

    /// True once a call on this connection has timed out — it is desynced and
    /// must be reopened ([`Engine::rebuild`]).
    pub fn connection_dead(&self, conn_id: &str) -> bool {
        self.wires.get(conn_id).is_some_and(|w| w.dead)
    }

    /// The endpoint names, function names and status of a live connection — what
    /// the frame inspector needs to decode its frames.
    pub fn wire_info(&self, conn_id: &str) -> Option<WireInfo<'_>> {
        let w = self.wires.get(conn_id)?;
        Some(WireInfo {
            client_id: &w.client_id,
            server_id: &w.server_id,
            client_name: &w.client_name,
            server_name: &w.server_name,
            fn_names: w
                .dispatch
                .protocol()
                .functions
                .iter()
                .map(|f| f.name.as_str())
                .collect(),
            framing: w.framing,
            codec: w.codec,
            error: w.error.as_deref(),
            dead: w.dead,
        })
    }

    pub fn connection_ids(&self) -> impl Iterator<Item = &str> {
        self.wires.keys().map(String::as_str)
    }

    /// The connections `instance_id` is an end of.
    pub fn connections_for_instance(&self, instance_id: &str) -> Vec<String> {
        self.wires
            .iter()
            .filter(|(_, w)| w.client_id == instance_id || w.server_id == instance_id)
            .map(|(id, _)| id.clone())
            .collect()
    }

    // ── driving ──────────────────────────────────────────────────────────

    /// Frame a call on `conn_id` to `fn_name` and put the request on the wire.
    /// Returns the request id. `Err` if the connection is unknown or refused, or
    /// the protocol has no such function.
    pub fn call(
        &mut self,
        conn_id: &str,
        fn_name: &str,
        params: &Value,
    ) -> Result<u64, RuntimeError> {
        let wire = self.wires.get(conn_id).ok_or(RuntimeError::Transport)?;
        if wire.error.is_some() {
            return Err(RuntimeError::Handshake);
        }
        if wire.dead {
            return Err(RuntimeError::Timeout);
        }
        let function = wire
            .dispatch
            .protocol()
            .functions
            .iter()
            .find(|f| f.name == fn_name)
            .ok_or(RuntimeError::UnknownCall)?;
        let call_id = function.index as u16;
        let one_way = function.oneway;
        let timeout_ms = wire.timeout_ms;

        self.next_request_id += 1;
        let id = self.next_request_id;
        let mut frame = Vec::new();
        wire.framing
            .encode_request(call_id, fn_name, id, params, &wire.codec, &mut frame)?;

        self.emit(conn_id, Direction::Request, frame);
        if one_way {
            self.results.insert(id, CallResult::Ok(Value::Null));
        } else {
            self.in_flight.insert(id);
            if timeout_ms > 0.0 {
                let handle = self.clock.schedule(
                    timeout_ms,
                    Event::Timeout {
                        conn: conn_id.to_string(),
                        request_id: id,
                    },
                );
                self.timeouts.insert(id, handle);
            }
        }
        Ok(id)
    }

    /// Run until the event queue is empty.
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

    /// Advance virtual time by `ms`, firing every event that comes due within the
    /// window (including ones scheduled while firing), then park at the edge.
    pub fn advance(&mut self, ms: f64) {
        let until = self.clock.now() + ms.max(0.0);
        while self.clock.peek_due().is_some_and(|t| t <= until) {
            let ev = self.clock.pop_next().unwrap();
            self.apply(ev);
        }
        self.clock.park_at_least(until);
    }

    // ── internals ────────────────────────────────────────────────────────

    fn apply(&mut self, ev: Event) {
        match ev {
            Event::DeliverToServer { conn, frame } => self.serve(&conn, &frame),
            Event::DeliverToClient { conn, frame } => self.receive(&conn, &frame),
            Event::FlushReorder { conn, dir } => {
                if let Some(wire) = self.wires.get_mut(&conn) {
                    for f in wire.channel.flush_reorder(&mut self.rng) {
                        self.clock
                            .schedule(0.0, deliver_event(conn.clone(), dir, f));
                    }
                }
            }
            Event::CompleteReply {
                conn,
                request_id,
                outcome,
            } => self.reply_now(&conn, request_id, outcome),
            Event::Timeout { conn, request_id } => self.time_out(&conn, request_id),
        }
    }

    /// Hand a frame to a connection's channel and schedule whatever it decided.
    fn emit(&mut self, conn_id: &str, dir: Direction, frame: Vec<u8>) {
        let now = self.clock.now();
        let Some(wire) = self.wires.get_mut(conn_id) else {
            return;
        };
        let outcome = wire.channel.send(dir, &frame, now, &mut self.rng);
        match outcome {
            SendOutcome::Dropped => {}
            SendOutcome::Deliver { frames, delay_ms } => {
                for f in frames {
                    self.clock
                        .schedule(delay_ms, deliver_event(conn_id.to_string(), dir, f));
                }
            }
            SendOutcome::Buffered { schedule_flush } => {
                if schedule_flush {
                    self.clock.schedule(
                        REORDER_FLUSH_MS,
                        Event::FlushReorder {
                            conn: conn_id.to_string(),
                            dir,
                        },
                    );
                }
            }
        }
    }

    /// Server side: decode the request, run its behaviour, act on the step.
    fn serve(&mut self, conn_id: &str, request_frame: &[u8]) {
        let Some(wire) = self.wires.get_mut(conn_id) else {
            return;
        };
        let Some(req) = wire.framing.decode_request(request_frame) else {
            return;
        };
        let request_id = req.request_id;
        let codec = wire.codec;
        let step = match wire
            .dispatch
            .dispatch(req.call, req.params, &codec, &mut wire.behaviors)
        {
            Ok(step) => step,
            Err(_) => return,
        };

        match step {
            BehaviorStep::Now(outcome) => self.reply_now(conn_id, request_id, outcome),
            BehaviorStep::After { delay_ms, outcome } => {
                self.clock.schedule(
                    delay_ms,
                    Event::CompleteReply {
                        conn: conn_id.to_string(),
                        request_id,
                        outcome,
                    },
                );
            }
            BehaviorStep::Hang => {}
            BehaviorStep::Forward {
                via,
                target_fn,
                params,
            } => self.begin_forward(conn_id, request_id, via, target_fn, params),
        }
    }

    /// Client side: decode the response frame and settle the matching call.
    fn receive(&mut self, conn_id: &str, response_frame: &[u8]) {
        let Some(wire) = self.wires.get(conn_id) else {
            return;
        };
        let codec = wire.codec;
        let Some((request_id, envelope)) = wire.framing.decode_response(response_frame) else {
            return;
        };
        let result = match envelope {
            Envelope::Ok(payload) => match codec.decode::<Value>(payload) {
                Ok(value) => CallResult::Ok(value),
                Err(_) => CallResult::Undecodable("ok body did not decode".into()),
            },
            Envelope::Err { id, body } => {
                let body = if body.is_empty() {
                    Value::Null
                } else {
                    codec.decode::<Value>(body).unwrap_or(Value::Null)
                };
                CallResult::Err { ordinal: id, body }
            }
        };
        self.settle(request_id, result);
    }

    /// Frame `outcome` as a response to `request_id` on `conn_id` and send it.
    fn reply_now(&mut self, conn_id: &str, request_id: u64, outcome: SimOutcome) {
        let Some(wire) = self.wires.get(conn_id) else {
            return;
        };
        let (framing, codec) = (wire.framing, wire.codec);
        let mut resp = Vec::new();
        match outcome {
            SimOutcome::Ok(value) => {
                let mut body = Vec::new();
                if codec.encode(&value, &mut body).is_err() {
                    return;
                }
                framing.encode_response_ok(request_id, &body, &mut resp);
            }
            SimOutcome::Err { ordinal, data } => {
                let mut body = Vec::new();
                if codec.encode(&data, &mut body).is_err() {
                    return;
                }
                framing.encode_response_err(request_id, ordinal, &body, &mut resp);
            }
            SimOutcome::None => return,
        }
        self.emit(conn_id, Direction::Response, resp);
    }

    /// Settle a call — either resume the forward waiting on it, or record it.
    /// First settlement wins: a late frame after a timeout is ignored.
    fn settle(&mut self, request_id: u64, result: CallResult) {
        if !self.in_flight.remove(&request_id) {
            return;
        }
        if let Some(handle) = self.timeouts.remove(&request_id) {
            self.clock.cancel(handle);
        }
        if let Some(cont) = self.forwards.remove(&request_id) {
            self.forwarding.remove(&cont.via);
            let outcome = match result {
                CallResult::Ok(value) => SimOutcome::Ok(value),
                CallResult::Err { ordinal, body } => SimOutcome::Err {
                    ordinal,
                    data: body,
                },
                CallResult::Undecodable(msg) => SimOutcome::Err {
                    ordinal: 0,
                    data: json!({ "error": msg }),
                },
                CallResult::Timeout => SimOutcome::Err {
                    ordinal: 0,
                    data: json!({ "error": "forward: upstream timed out" }),
                },
            };
            self.reply_now(&cont.outer_conn, cont.outer_request_id, outcome);
        } else {
            self.results.insert(request_id, result);
        }
    }

    /// A call's timeout window elapsed: if it is still in flight, time it out and
    /// leave its wire `dead`.
    fn time_out(&mut self, conn_id: &str, request_id: u64) {
        if !self.in_flight.contains(&request_id) {
            return;
        }
        self.timeouts.remove(&request_id); // this event *is* the timeout — nothing to cancel
        if let Some(wire) = self.wires.get_mut(conn_id) {
            wire.dead = true;
        }
        self.settle(request_id, CallResult::Timeout);
    }

    /// Relay the outer call on `via`, then answer it with the inner outcome.
    fn begin_forward(
        &mut self,
        outer_conn: &str,
        outer_request_id: u64,
        via: String,
        target_fn: String,
        params: Value,
    ) {
        let refusal = match self.wires.get(&via) {
            None => Some(format!("forward: no connection {via}")),
            Some(w) if w.error.is_some() => Some(format!(
                "forward: {via} refused ({})",
                w.error.as_deref().unwrap()
            )),
            _ if self.forwarding.contains(&via) => Some("forwarding cycle".to_string()),
            _ => None,
        };
        if let Some(message) = refusal {
            self.reply_now(
                outer_conn,
                outer_request_id,
                SimOutcome::Err {
                    ordinal: 0,
                    data: json!({ "error": message }),
                },
            );
            return;
        }

        self.forwarding.insert(via.clone());
        match self.call(&via, &target_fn, &params) {
            Ok(inner_id) => {
                self.forwards.insert(
                    inner_id,
                    ForwardCont {
                        outer_conn: outer_conn.to_string(),
                        outer_request_id,
                        via,
                    },
                );
            }
            Err(_) => {
                self.forwarding.remove(&via);
                self.reply_now(
                    outer_conn,
                    outer_request_id,
                    SimOutcome::Err {
                        ordinal: 0,
                        data: json!({ "error": format!("forward: {via} has no {target_fn}") }),
                    },
                );
            }
        }
    }
}

/// The server's per-function behaviour map for a fresh wire. Falls back to
/// `reply` with an empty config for a function the instance never seeded
/// (shouldn't happen — `Session` seeds every one).
fn build_behaviors(server: &Instance, proto: &ProtocolShape, schema: &SchemaShape) -> BehaviorMap {
    proto
        .functions
        .iter()
        .map(|f| {
            let (kind, config) = match server.behaviors.get(&f.name) {
                Some(setting) => (setting.kind, setting.config.clone()),
                None => (BehaviorKind::Reply, json!({})),
            };
            (f.name.clone(), kind.make(&config, f, schema))
        })
        .collect()
}

/// The framing / codec combinations the compare panel runs (2g). JSON-RPC pairs
/// only with JSON.
pub const TRANSPORT_MATRIX: [(FramingChoice, Codec); 3] = [
    (FramingChoice::Datagram, Codec::Json),
    (FramingChoice::Datagram, Codec::Msgpack),
    (FramingChoice::Jsonrpc, Codec::Json),
];

/// One combo's run: its label, the frame log, and the settled call outcome.
pub struct MatrixRun {
    pub label: String,
    pub frames: Vec<crate::frame::Frame>,
    pub result: Option<CallResult>,
}

/// Run the same call on connection `conn_id` over each transport combo, on a
/// throwaway engine per combo (so the session isn't disturbed). The decoded
/// bodies should match across all of them — divergence is a framing / codec bug.
pub fn compare_transports(
    session: &Session,
    conn_id: &str,
    fn_name: &str,
    params: &Value,
) -> Vec<MatrixRun> {
    TRANSPORT_MATRIX
        .iter()
        .map(|&(framing, codec)| {
            let mut s = session.clone();
            s.set_transport(conn_id, framing, codec);
            let mut engine = Engine::new();
            engine.rebuild(&s);
            let id = engine.call(conn_id, fn_name, params).ok();
            engine.run();
            MatrixRun {
                label: format!("{}/{}", framing_label(framing), codec.name()),
                frames: engine
                    .tap(conn_id)
                    .map(|t| t.frames.clone())
                    .unwrap_or_default(),
                result: id.and_then(|id| engine.result(id).cloned()),
            }
        })
        .collect()
}

fn framing_label(f: FramingChoice) -> &'static str {
    match f {
        FramingChoice::Jsonrpc => "jsonrpc",
        _ => "datagram",
    }
}

/// A 31-byte handshake frame carrying the instance's IR hash and the connection's
/// wire-format / framing names (hashed in).
fn handshake_frame(ir_hash: &str, wire_format: &str, framing: &str) -> Vec<u8> {
    let hash = ir_hash
        .strip_prefix("0x")
        .and_then(|h| u64::from_str_radix(h, 16).ok())
        .unwrap_or(0);
    let mut buf = Vec::new();
    Handshake::new(hash, wire_format, framing, 0).encode(&mut buf);
    buf
}

/// A one-connection chat engine, shared by `smoke()` and the tests.
pub(crate) mod chat {
    use super::*;
    use crate::model::{InstanceSpec, Placement, Role};
    use crate::shape::{
        ArgShape, FieldShape, FnShape, Framing, ProjectShape, SchemaShape, TypeDef, TypeRef,
    };

    /// The connection id `add_connection` assigns the single wire.
    pub const CONN: &str = "c1";

    pub fn shape() -> ProjectShape {
        let string = || TypeRef::Prim {
            name: "string".into(),
        };
        ProjectShape {
            schemas: vec![SchemaShape {
                namespace: "chat".into(),
                ir_hash: "0x9f2b1c7d4e6a8035".into(),
                protocols: vec![ProtocolShape {
                    name: "Chat".into(),
                    framing: Framing::Datagram,
                    functions: vec![FnShape {
                        name: "send".into(),
                        index: 0,
                        oneway: false,
                        args: vec![ArgShape {
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
                        FieldShape {
                            name: "body".into(),
                            ty: string(),
                            optional: false,
                        },
                        FieldShape {
                            name: "seq".into(),
                            ty: TypeRef::Prim { name: "u64".into() },
                            optional: false,
                        },
                    ],
                }],
            }],
        }
    }

    /// `chat-1` (server, `send` → reply `{body:"HELLO",seq:1}`) with `chat-2`
    /// (client) connected as `c1`.
    pub fn session() -> Session {
        let mut s = Session::empty(shape());
        let srv = s.add_instance(
            InstanceSpec {
                schema_ns: "chat".into(),
                protocol: "Chat".into(),
                role: Role::Server,
            },
            Placement::default(),
        );
        let cli = s.add_instance(
            InstanceSpec {
                schema_ns: "chat".into(),
                protocol: "Chat".into(),
                role: Role::Client,
            },
            Placement::default(),
        );
        s.set_behavior(
            &srv,
            "send",
            BehaviorKind::Reply,
            Some(json!({ "value": { "body": "HELLO", "seq": 1 } })),
        )
        .unwrap();
        s.add_connection(&cli, &srv).unwrap();
        s
    }

    pub fn engine() -> Engine {
        let mut e = Engine::new();
        e.sync(&session());
        e
    }

    #[allow(dead_code)]
    pub fn schema() -> SchemaShape {
        shape().schemas.into_iter().next().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::chat::{self, CONN};
    use super::*;
    use crate::faults::FaultDir;
    use crate::frame::FrameKind;
    use crate::model::{InstanceSpec, Placement, Role};
    use serde_json::json;

    fn hello() -> Value {
        json!({ "body": "HELLO", "seq": 1 })
    }

    fn client_instance(spec_role: Role) -> InstanceSpec {
        InstanceSpec {
            schema_ns: "chat".into(),
            protocol: "Chat".into(),
            role: spec_role,
        }
    }

    /// The tap frames past the two handshake frames a clean connect records.
    fn payload_frames(engine: &Engine, conn: &str) -> Vec<crate::frame::Frame> {
        engine
            .tap(conn)
            .unwrap()
            .frames
            .iter()
            .filter(|f| f.kind != FrameKind::Handshake)
            .cloned()
            .collect()
    }

    #[test]
    fn a_connect_records_a_handshake_each_way() {
        let e = chat::engine();
        let frames = &e.tap(CONN).unwrap().frames;
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| f.kind == FrameKind::Handshake));
        assert_eq!(
            (frames[0].from.as_str(), frames[0].to.as_str()),
            ("chat-2", "chat-1")
        );
        assert_eq!(
            (frames[1].from.as_str(), frames[1].to.as_str()),
            ("chat-1", "chat-2")
        );
        assert!(e.connection_error(CONN).is_none());
    }

    #[test]
    fn a_call_round_trips_over_the_real_contract() {
        let mut e = chat::engine();
        let id = e.call(CONN, "send", &json!(["hi"])).unwrap();
        assert_eq!(e.result(id), None);

        e.run();

        assert_eq!(e.result(id), Some(&CallResult::Ok(hello())));
        let p = payload_frames(&e, CONN);
        assert_eq!(p.len(), 2, "one request, one response");
        assert_eq!((p[0].from.as_str(), p[0].to.as_str()), ("chat-2", "chat-1"));
        assert_eq!((p[1].from.as_str(), p[1].to.as_str()), ("chat-1", "chat-2"));
    }

    #[test]
    fn an_unknown_or_missing_function_rejects_a_call() {
        let mut e = chat::engine();
        assert_eq!(
            e.call("nope", "send", &json!([])).unwrap_err(),
            RuntimeError::Transport
        );
        assert_eq!(
            e.call(CONN, "missing", &json!([])).unwrap_err(),
            RuntimeError::UnknownCall
        );
    }

    #[test]
    fn a_version_skew_refuses_the_handshake() {
        let mut session = chat::session();
        let server_id = session
            .instances
            .iter()
            .find(|i| i.role == Role::Server)
            .unwrap()
            .id
            .clone();
        session
            .instances
            .iter_mut()
            .find(|i| i.id == server_id)
            .unwrap()
            .ir_hash = "0xdifferent".into();

        let mut e = Engine::new();
        e.sync(&session);
        assert_eq!(e.connection_error(CONN), Some("handshake"));
        assert_eq!(
            e.call(CONN, "send", &json!(["hi"])).unwrap_err(),
            RuntimeError::Handshake
        );
    }

    #[test]
    fn latency_shows_up_as_virtual_time() {
        let mut session = chat::session();
        session.latency_ms = 50.0;
        let mut e = Engine::new();
        e.sync(&session);

        let id = e.call(CONN, "send", &json!(["hi"])).unwrap();
        e.run();
        assert_eq!(e.result(id), Some(&CallResult::Ok(hello())));
        assert_eq!(e.now(), 100.0, "50 ms each way");
    }

    #[test]
    fn a_dropped_request_never_settles_without_a_timeout() {
        let mut session = chat::session();
        session.call_timeout_ms = 0.0; // wait forever — isolate the drop path
        session.connections[0].faults.drop_prob = 1.0;
        session.connections[0].faults.apply_to = FaultDir::Requests;
        let mut e = Engine::new();
        e.sync(&session);

        let id = e.call(CONN, "send", &json!(["hi"])).unwrap();
        e.run();
        assert_eq!(e.result(id), None);
        assert!(payload_frames(&e, CONN)
            .iter()
            .any(|f| f.fault.as_deref() == Some("dropped")));
    }

    #[test]
    fn sync_is_incremental() {
        let mut session = chat::session();
        let mut e = Engine::new();
        e.sync(&session);
        let c1_tap = e.tap(CONN).unwrap() as *const Tap;

        let srv = session
            .instances
            .iter()
            .find(|i| i.role == Role::Server)
            .unwrap()
            .id
            .clone();
        let cli2 = session.add_instance(client_instance(Role::Client), Placement::default());
        let c2 = session.add_connection(&cli2, &srv).unwrap();
        e.sync(&session);

        assert_eq!(
            e.tap(CONN).unwrap() as *const Tap,
            c1_tap,
            "c1 left running"
        );
        assert!(e.tap(&c2).is_some(), "c2 opened");

        session.remove_connection(&c2);
        e.sync(&session);
        assert!(e.tap(&c2).is_none(), "c2 closed");
        assert!(e.tap(CONN).is_some(), "c1 still there");
    }

    #[test]
    fn behaviour_swap_takes_effect_on_the_next_call() {
        let session = chat::session();
        let mut e = Engine::new();
        e.sync(&session);

        e.set_behavior(
            CONN,
            "send",
            BehaviorKind::Echo,
            &json!({}),
            &chat::schema(),
        )
        .unwrap();
        let id = e.call(CONN, "send", &json!(["echo me"])).unwrap();
        e.run();
        assert_eq!(e.result(id), Some(&CallResult::Ok(json!(["echo me"]))));
    }

    #[test]
    fn forward_relays_a_call_to_a_second_connection() {
        let mut session = chat::session(); // chat-1 (srv, replies HELLO), chat-2 (cli), c1
        let backend = session
            .instances
            .iter()
            .find(|i| i.role == Role::Server)
            .unwrap()
            .id
            .clone();

        let gw_client = session.add_instance(client_instance(Role::Client), Placement::default());
        let c_gw_backend = session.add_connection(&gw_client, &backend).unwrap();

        let gw_server = session.add_instance(client_instance(Role::Server), Placement::default());
        let outer_client =
            session.add_instance(client_instance(Role::Client), Placement::default());
        let c_outer = session.add_connection(&outer_client, &gw_server).unwrap();
        session
            .set_behavior(
                &gw_server,
                "send",
                BehaviorKind::Forward,
                Some(json!({ "viaConnectionId": c_gw_backend, "targetFn": "send" })),
            )
            .unwrap();

        let mut e = Engine::new();
        e.sync(&session);
        let id = e.call(&c_outer, "send", &json!(["relay me"])).unwrap();
        e.run();

        assert_eq!(e.result(id), Some(&CallResult::Ok(hello())));
    }

    #[test]
    fn a_forwarding_cycle_is_refused() {
        let mut session = Session::empty(chat::shape());
        let g1s = session.add_instance(client_instance(Role::Server), Placement::default());
        let g1c = session.add_instance(client_instance(Role::Client), Placement::default());
        let g2s = session.add_instance(client_instance(Role::Server), Placement::default());
        let g2c = session.add_instance(client_instance(Role::Client), Placement::default());
        let client = session.add_instance(client_instance(Role::Client), Placement::default());

        let c1_to_g2 = session.add_connection(&g1c, &g2s).unwrap();
        let c2_to_g1 = session.add_connection(&g2c, &g1s).unwrap();
        let c_in = session.add_connection(&client, &g1s).unwrap();

        session
            .set_behavior(
                &g1s,
                "send",
                BehaviorKind::Forward,
                Some(json!({ "viaConnectionId": c1_to_g2, "targetFn": "send" })),
            )
            .unwrap();
        session
            .set_behavior(
                &g2s,
                "send",
                BehaviorKind::Forward,
                Some(json!({ "viaConnectionId": c2_to_g1, "targetFn": "send" })),
            )
            .unwrap();

        let mut e = Engine::new();
        e.sync(&session);
        let id = e.call(&c_in, "send", &json!(["loop"])).unwrap();
        e.run();

        match e.result(id) {
            Some(CallResult::Err { ordinal: 0, body }) => {
                assert!(body["error"].as_str().unwrap().contains("cycle"), "{body}");
            }
            other => panic!("expected a cycle error, got {other:?}"),
        }
    }

    #[test]
    fn reseeding_keeps_a_run_reproducible() {
        let frames_for = |seed| {
            let mut session = chat::session();
            session.seed = seed;
            session.connections[0].faults.corrupt_prob = 0.5;
            let mut e = Engine::new();
            e.rebuild(&session);
            for _ in 0..8 {
                let _ = e.call(CONN, "send", &json!(["hi"]));
                e.run();
            }
            e.tap(CONN)
                .unwrap()
                .frames
                .iter()
                .map(|f| (f.bytes.clone(), f.fault.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(frames_for(7), frames_for(7));
        assert_ne!(frames_for(7), frames_for(8));
    }

    #[test]
    fn a_dropped_reply_times_the_call_out_and_kills_the_wire() {
        let mut session = chat::session();
        session.call_timeout_ms = 120.0;
        session.connections[0].faults.drop_prob = 1.0;
        session.connections[0].faults.apply_to = FaultDir::Responses;
        let mut e = Engine::new();
        e.sync(&session);

        let id = e.call(CONN, "send", &json!(["x"])).unwrap();
        e.run();

        assert_eq!(e.result(id), Some(&CallResult::Timeout));
        assert!(e.connection_dead(CONN));
        assert_eq!(e.now(), 120.0, "settled at the timeout instant");
        // a dead wire fails every later call fast
        assert_eq!(
            e.call(CONN, "send", &json!(["y"])).unwrap_err(),
            RuntimeError::Timeout
        );
    }

    #[test]
    fn rebuild_recovers_a_dead_wire() {
        let mut session = chat::session();
        session.call_timeout_ms = 120.0;
        session.connections[0].faults.drop_prob = 1.0;
        session.connections[0].faults.apply_to = FaultDir::Responses;
        let mut e = Engine::new();
        e.sync(&session);
        e.call(CONN, "send", &json!(["x"])).unwrap();
        e.run();
        assert!(e.connection_dead(CONN));

        session.connections[0].faults.drop_prob = 0.0;
        e.rebuild(&session);
        assert!(!e.connection_dead(CONN));

        let id = e.call(CONN, "send", &json!(["x"])).unwrap();
        e.run();
        assert_eq!(e.result(id), Some(&CallResult::Ok(hello())));
    }

    #[test]
    fn a_zero_timeout_waits_forever() {
        let mut session = chat::session();
        session.call_timeout_ms = 0.0;
        session.connections[0].faults.drop_prob = 1.0;
        session.connections[0].faults.apply_to = FaultDir::Responses;
        let mut e = Engine::new();
        e.sync(&session);

        let id = e.call(CONN, "send", &json!(["x"])).unwrap();
        e.advance(1_000_000.0);
        assert_eq!(e.result(id), None, "never settles, never dies");
        assert!(!e.connection_dead(CONN));
    }

    #[test]
    fn a_forward_whose_upstream_hangs_times_out_and_kills_both_wires() {
        let mut session = chat::session(); // chat-1 (backend), chat-2, c1
        session.call_timeout_ms = 150.0;
        let backend = session
            .instances
            .iter()
            .find(|i| i.role == Role::Server)
            .unwrap()
            .id
            .clone();
        // make the backend never reply
        session
            .set_behavior(&backend, "send", BehaviorKind::Drop, Some(json!({})))
            .unwrap();

        let gw_client = session.add_instance(client_instance(Role::Client), Placement::default());
        let c_gw_backend = session.add_connection(&gw_client, &backend).unwrap();
        let gw_server = session.add_instance(client_instance(Role::Server), Placement::default());
        let outer_client =
            session.add_instance(client_instance(Role::Client), Placement::default());
        let c_outer = session.add_connection(&outer_client, &gw_server).unwrap();
        session
            .set_behavior(
                &gw_server,
                "send",
                BehaviorKind::Forward,
                Some(json!({ "viaConnectionId": c_gw_backend, "targetFn": "send" })),
            )
            .unwrap();

        let mut e = Engine::new();
        e.sync(&session);
        let id = e.call(&c_outer, "send", &json!(["relay"])).unwrap();
        e.run();

        // the outer client's own timeout fires first (one session-wide window,
        // and it was scheduled before the nested inner call) — same as the TS
        assert_eq!(e.result(id), Some(&CallResult::Timeout));
        assert!(e.connection_dead(&c_outer), "the outer wire is dead");
        assert!(e.connection_dead(&c_gw_backend), "so is the hung upstream");
    }

    // ── framing / codec matrix (2g) ─────────────────────────────────────

    #[test]
    fn a_call_round_trips_over_every_transport_combo() {
        for (framing, codec) in TRANSPORT_MATRIX {
            let mut session = chat::session();
            session.set_transport(chat::CONN, framing, codec);
            let mut e = Engine::new();
            e.sync(&session);
            assert!(
                e.connection_error(chat::CONN).is_none(),
                "{framing:?}/{codec:?} connected"
            );

            let id = e.call(chat::CONN, "send", &json!(["hi"])).unwrap();
            e.run();
            assert_eq!(
                e.result(id),
                Some(&CallResult::Ok(hello())),
                "{framing:?}/{codec:?} decoded body"
            );
        }
    }

    #[test]
    fn json_rpc_with_msgpack_is_refused() {
        let mut session = chat::session();
        session.set_transport(chat::CONN, FramingChoice::Jsonrpc, Codec::Msgpack);
        let mut e = Engine::new();
        e.sync(&session);
        assert_eq!(
            e.connection_error(chat::CONN),
            Some("json-rpc framing requires the json codec")
        );
    }

    #[test]
    fn compare_transports_agrees_on_the_body_and_differs_on_the_frames() {
        let session = chat::session();
        let runs = compare_transports(&session, chat::CONN, "send", &json!(["hi"]));
        assert_eq!(runs.len(), 3);

        // every combo decodes to the same reply
        for run in &runs {
            assert_eq!(
                run.result,
                Some(CallResult::Ok(hello())),
                "{} body",
                run.label
            );
        }

        // the datagram + msgpack request frame is shorter than the datagram +
        // json one, and the json-rpc one is a text frame
        let req_bytes = |label: &str| {
            runs.iter()
                .find(|r| r.label == label)
                .unwrap()
                .frames
                .iter()
                .find(|f| f.kind == FrameKind::Request)
                .unwrap()
                .bytes
                .clone()
        };
        let dg_json = req_bytes("datagram/json");
        let dg_mp = req_bytes("datagram/msgpack");
        let rpc = req_bytes("jsonrpc/json");
        assert!(dg_mp.len() < dg_json.len(), "msgpack is tighter");
        assert!(std::str::from_utf8(&rpc)
            .unwrap()
            .contains("\"method\":\"send\""));
    }
}
