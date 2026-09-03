//! `Sim` — the `wasm-bindgen` surface the playground UI drives. It owns a
//! [`Session`] and the [`Engine`] that runs it; everything crosses the boundary
//! as JSON strings (no `serde-wasm-bindgen` dependency, and the UI is JS anyway).
//!
//! The fallible methods have a private `try_*` returning `Result<_, String>` (so
//! they can be unit-tested natively) behind a thin `wasm_bindgen` wrapper that
//! maps the message to a thrown `Error`.

use serde_json::{json, Value};
use wasm_bindgen::prelude::*;

use crate::behavior::BehaviorKind;
use crate::engine::{CallResult, Engine};
use crate::format::Codec;
use crate::framedecode::{describe_frame, DecodeCtx};
use crate::model::FramingChoice;
use crate::model::{InstanceSpec, Placement, Role, Session};
use crate::record::{Input, Recorder, Recording};
use crate::session_codec::{decode_session, encode_session};
use crate::shape::{find_protocol, ProjectShape};

fn js_err(message: String) -> JsValue {
    JsValue::from_str(&message)
}

fn call_result_json(result: &CallResult) -> Value {
    match result {
        CallResult::Ok(value) => json!({ "status": "ok", "value": value }),
        CallResult::Err { ordinal, body } => {
            json!({ "status": "err", "ordinal": ordinal, "body": body })
        }
        CallResult::Undecodable(message) => json!({ "status": "undecodable", "message": message }),
        CallResult::Timeout => json!({ "status": "timeout" }),
    }
}

#[wasm_bindgen]
pub struct Sim {
    session: Session,
    engine: Engine,
    recorder: Recorder,
}

#[wasm_bindgen]
impl Sim {
    /// `shape_json` is the playground editor's `describe_project` output.
    /// `session_link` optionally restores a `#s=…` payload.
    #[wasm_bindgen(constructor)]
    pub fn new(shape_json: &str, session_link: Option<String>) -> Result<Sim, JsValue> {
        Self::try_new(shape_json, session_link.as_deref()).map_err(js_err)
    }

    fn try_new(shape_json: &str, session_link: Option<&str>) -> Result<Sim, String> {
        let shape: ProjectShape =
            serde_json::from_str(shape_json).map_err(|e| format!("bad shape json: {e}"))?;
        let session = match session_link {
            Some(link) => decode_session(link, shape)
                .ok_or_else(|| "session link did not decode".to_string())?,
            None => Session::empty(shape),
        };
        let mut engine = Engine::new();
        engine.sync(&session);
        Ok(Sim {
            session,
            engine,
            recorder: Recorder::new(),
        })
    }

    /// Re-point at a freshly compiled shape (a schema edit). Reconciles the
    /// session and re-opens every wire.
    pub fn set_shape(&mut self, shape_json: &str) -> Result<(), JsValue> {
        let shape: ProjectShape =
            serde_json::from_str(shape_json).map_err(|e| js_err(format!("bad shape json: {e}")))?;
        self.session.rebuild(shape);
        self.engine.rebuild(&self.session);
        Ok(())
    }

    // ── topology ─────────────────────────────────────────────────────────

    /// `spec_json`: `{ "schemaNs", "protocol", "role", "nodeId"?, "x"?, "y"? }`.
    /// Returns the new instance id.
    pub fn add_instance(&mut self, spec_json: &str) -> Result<String, JsValue> {
        self.try_add_instance(spec_json).map_err(js_err)
    }

    fn try_add_instance(&mut self, spec_json: &str) -> Result<String, String> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct SpecJson {
            schema_ns: String,
            protocol: String,
            role: Role,
            #[serde(default)]
            node_id: Option<String>,
            #[serde(default)]
            x: f64,
            #[serde(default)]
            y: f64,
        }
        let s: SpecJson = serde_json::from_str(spec_json).map_err(|e| e.to_string())?;
        let id = self.session.add_instance(
            InstanceSpec {
                schema_ns: s.schema_ns,
                protocol: s.protocol,
                role: s.role,
            },
            Placement {
                node_id: s.node_id,
                x: s.x,
                y: s.y,
            },
        );
        self.engine.sync(&self.session);
        Ok(id)
    }

    pub fn remove_instance(&mut self, id: &str) {
        self.session.remove_instance(id);
        self.engine.sync(&self.session);
    }

    pub fn add_connection(&mut self, client_id: &str, server_id: &str) -> Result<String, JsValue> {
        let id = self
            .session
            .add_connection(client_id, server_id)
            .map_err(js_err)?;
        self.engine.sync(&self.session);
        Ok(id)
    }

    pub fn remove_connection(&mut self, conn_id: &str) {
        self.session.remove_connection(conn_id);
        self.engine.sync(&self.session);
    }

    pub fn rename_node(&mut self, node_id: &str, label: &str) {
        self.session.rename_node(node_id, label);
    }

    pub fn move_node(&mut self, node_id: &str, x: f64, y: f64) {
        self.session.move_node(node_id, x, y);
    }

    pub fn set_latency(&mut self, ms: f64) {
        self.session.latency_ms = ms.max(0.0);
        self.engine.rebuild(&self.session);
    }

    pub fn set_call_timeout(&mut self, ms: f64) {
        self.session.call_timeout_ms = ms.max(0.0);
        self.engine.rebuild(&self.session);
    }

    pub fn set_seed(&mut self, seed: u32) {
        self.session.seed = seed;
        self.engine.set_seed(seed);
    }

    pub fn set_clock_mode(&mut self, mode: &str) {
        self.session.clock_mode = if mode == "stepped" {
            crate::model::ClockMode::Stepped
        } else {
            crate::model::ClockMode::Real
        };
    }

    /// `kind` is a behaviour tag (`"reply"`, `"echo"`, …); `config_json` its
    /// config object.
    pub fn set_behavior(
        &mut self,
        instance_id: &str,
        fn_name: &str,
        kind: &str,
        config_json: &str,
    ) -> Result<(), JsValue> {
        self.try_set_behavior(instance_id, fn_name, kind, config_json)
            .map_err(js_err)
    }

    fn try_set_behavior(
        &mut self,
        instance_id: &str,
        fn_name: &str,
        kind: &str,
        config_json: &str,
    ) -> Result<(), String> {
        let kind = BehaviorKind::parse(kind).ok_or_else(|| format!("unknown behaviour {kind}"))?;
        let config: Value = serde_json::from_str(config_json).map_err(|e| e.to_string())?;
        self.session
            .set_behavior(instance_id, fn_name, kind, Some(config.clone()))?;

        let schema = self.session.instance(instance_id).and_then(|inst| {
            find_protocol(&self.session.shape, &inst.schema_ns, &inst.protocol)
                .map(|(schema, _)| schema.clone())
        });
        if let Some(schema) = schema {
            for conn_id in self.engine.connections_for_instance(instance_id) {
                let _ = self
                    .engine
                    .set_behavior(&conn_id, fn_name, kind, &config, &schema);
            }
        }

        if self.recorder.recording {
            if let Some(setting) = self
                .session
                .instance(instance_id)
                .and_then(|i| i.behaviors.get(fn_name))
                .cloned()
            {
                self.recorder.capture(
                    Input::Behavior {
                        instance_id: instance_id.to_string(),
                        function: fn_name.to_string(),
                        setting,
                    },
                    self.engine.now(),
                );
            }
        }
        Ok(())
    }

    /// `faults_json` is a `FaultSpec` object (camelCase, as in a session link).
    pub fn set_faults(&mut self, conn_id: &str, faults_json: &str) -> Result<(), JsValue> {
        let spec = serde_json::from_str(faults_json)
            .map_err(|e| js_err(format!("bad fault spec: {e}")))?;
        if let Some(conn) = self
            .session
            .connections
            .iter_mut()
            .find(|c| c.id == conn_id)
        {
            conn.faults = spec;
        }
        let spec = serde_json::from_str(faults_json).unwrap();
        self.engine.set_faults(conn_id, spec);
        if self.recorder.recording {
            if let Some(conn) = self.session.connections.iter().find(|c| c.id == conn_id) {
                self.recorder.capture(
                    Input::Fault {
                        conn_id: conn_id.to_string(),
                        faults: conn.faults.clone(),
                    },
                    self.engine.now(),
                );
            }
        }
        Ok(())
    }

    /// Set a connection's framing (`auto` / `datagram` / `jsonrpc`) and wire
    /// format (`json` / `msgpack`), then re-open the wire.
    pub fn set_transport(
        &mut self,
        conn_id: &str,
        framing: &str,
        wire_format: &str,
    ) -> Result<(), JsValue> {
        let framing = FramingChoice::parse(framing)
            .ok_or_else(|| js_err(format!("unknown framing {framing}")))?;
        let codec = Codec::parse(wire_format)
            .ok_or_else(|| js_err(format!("unknown wire format {wire_format}")))?;
        self.session.set_transport(conn_id, framing, codec);
        self.engine.rebuild(&self.session);
        Ok(())
    }

    /// Run the same call over each framing / codec combo (2g). Returns
    /// `{ "datagram/json": { frames, result }, … }` JSON; the decoded bodies
    /// should match across all of them.
    pub fn compare(
        &self,
        conn_id: &str,
        fn_name: &str,
        params_json: &str,
    ) -> Result<String, JsValue> {
        let params: Value = serde_json::from_str(params_json)
            .map_err(|e| js_err(format!("bad params json: {e}")))?;
        let runs = crate::engine::compare_transports(&self.session, conn_id, fn_name, &params);
        let mut out = serde_json::Map::new();
        for run in runs {
            out.insert(
                run.label,
                json!({
                    "frames": run.frames,
                    "result": run.result.as_ref().map(call_result_json),
                }),
            );
        }
        Ok(Value::Object(out).to_string())
    }

    // ── driving ──────────────────────────────────────────────────────────

    /// Frame a call; returns the request id (read the outcome back with
    /// [`Sim::result`]).
    pub fn call(
        &mut self,
        conn_id: &str,
        fn_name: &str,
        params_json: &str,
    ) -> Result<f64, JsValue> {
        let params: Value = serde_json::from_str(params_json)
            .map_err(|e| js_err(format!("bad params json: {e}")))?;
        let id = self
            .engine
            .call(conn_id, fn_name, &params)
            .map_err(|e| js_err(format!("{e:?}")))?;
        if self.recorder.recording {
            self.recorder.capture(
                Input::Call {
                    conn_id: conn_id.to_string(),
                    function: fn_name.to_string(),
                    params,
                },
                self.engine.now(),
            );
        }
        Ok(id as f64)
    }

    pub fn run(&mut self) {
        self.engine.run();
    }

    pub fn step(&mut self) -> bool {
        self.engine.step()
    }

    pub fn advance(&mut self, ms: f64) {
        self.engine.advance(ms);
    }

    pub fn now(&self) -> f64 {
        self.engine.now()
    }

    pub fn pending(&self) -> usize {
        self.engine.pending()
    }

    /// The settled outcome of a call, as `{ status, ... }` JSON, or `undefined`
    /// while it is still in flight.
    pub fn result(&self, request_id: f64) -> Option<String> {
        self.engine
            .result(request_id as u64)
            .map(|r| call_result_json(r).to_string())
    }

    // ── inspection ───────────────────────────────────────────────────────

    /// The session as a `#s=…` shareable payload.
    pub fn link(&self) -> String {
        encode_session(&self.session)
    }

    /// The session (minus `shape`) as JSON — nodes, instances, connections, …
    pub fn session_json(&self) -> String {
        serde_json::to_string(&self.session).unwrap_or_else(|_| "null".into())
    }

    /// A connection's frame log as a JSON array (empty if the connection is
    /// unknown).
    pub fn frames(&self, conn_id: &str) -> String {
        match self.engine.tap(conn_id) {
            Some(tap) => serde_json::to_string(&tap.frames).unwrap_or_else(|_| "[]".into()),
            None => "[]".into(),
        }
    }

    /// A decoded view of one frame (by its `seq`), as `FrameDetail` JSON.
    pub fn describe_frame(&self, conn_id: &str, seq: u32) -> Option<String> {
        let info = self.engine.wire_info(conn_id)?;
        let frame = self
            .engine
            .tap(conn_id)?
            .frames
            .iter()
            .find(|f| f.seq == seq)?;
        let detail = describe_frame(
            frame,
            &DecodeCtx {
                client_name: info.client_name,
                server_name: info.server_name,
                framing: info.framing,
                codec: info.codec,
                fn_names: &info.fn_names,
            },
        );
        serde_json::to_string(&detail).ok()
    }

    pub fn connection_error(&self, conn_id: &str) -> Option<String> {
        self.engine.connection_error(conn_id).map(str::to_string)
    }

    /// The behaviour picker's options for `schema_ns::protocol::fn_name`, as
    /// `[{ "kind", "label", "applies" }]` JSON — `applies` is whether the kind
    /// makes sense for that function. `[]` if the function is unknown.
    pub fn behavior_catalog(&self, schema_ns: &str, protocol: &str, fn_name: &str) -> String {
        let Some((_, proto)) = find_protocol(&self.session.shape, schema_ns, protocol) else {
            return "[]".into();
        };
        let Some(function) = proto.functions.iter().find(|f| f.name == fn_name) else {
            return "[]".into();
        };
        let catalog: Vec<Value> = BehaviorKind::ALL
            .iter()
            .map(|k| {
                json!({
                    "kind": k.as_str(),
                    "label": k.label(),
                    "applies": k.applies_to(function),
                })
            })
            .collect();
        Value::Array(catalog).to_string()
    }

    /// The behaviour a freshly-added server function starts on, for
    /// `schema_ns::protocol::fn_name` — `"reply"` or `"drop"`.
    pub fn default_behavior_kind(&self, schema_ns: &str, protocol: &str, fn_name: &str) -> String {
        find_protocol(&self.session.shape, schema_ns, protocol)
            .and_then(|(_, p)| p.functions.iter().find(|f| f.name == fn_name))
            .map(|f| crate::behavior::default_kind_for(f).as_str().to_string())
            .unwrap_or_else(|| "reply".into())
    }

    pub fn connection_dead(&self, conn_id: &str) -> bool {
        self.engine.connection_dead(conn_id)
    }

    // ── record / replay ──────────────────────────────────────────────────

    pub fn record_start(&mut self) {
        self.recorder.start(&self.session, self.engine.now());
    }

    pub fn recording(&self) -> bool {
        self.recorder.recording
    }

    pub fn recording_count(&self) -> usize {
        self.recorder.count()
    }

    /// Stop recording and return the `Recording` JSON.
    pub fn record_stop(&mut self) -> String {
        serde_json::to_string(&self.recorder.stop()).unwrap_or_else(|_| "null".into())
    }

    /// Replace this sim's state with a replay of `recording_json` against
    /// `shape_json` (run to completion on a fresh stepped engine).
    pub fn load_replay(&mut self, recording_json: &str, shape_json: &str) -> Result<(), JsValue> {
        self.try_load_replay(recording_json, shape_json)
            .map_err(js_err)
    }

    fn try_load_replay(&mut self, recording_json: &str, shape_json: &str) -> Result<(), String> {
        let rec: Recording =
            serde_json::from_str(recording_json).map_err(|e| format!("bad recording: {e}"))?;
        let shape: ProjectShape =
            serde_json::from_str(shape_json).map_err(|e| format!("bad shape json: {e}"))?;
        let replay = crate::record::replay_recording(&rec, shape)?;
        self.session = replay.session;
        self.engine = replay.engine;
        self.recorder = Recorder::new();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::chat;
    use serde_json::json;

    fn sim() -> Sim {
        let shape_json = serde_json::to_string(&chat::shape()).unwrap();
        Sim::try_new(&shape_json, None).unwrap()
    }

    #[test]
    fn build_a_topology_and_run_a_call() {
        let mut s = sim();
        let srv = s
            .try_add_instance(r#"{"schemaNs":"chat","protocol":"Chat","role":"server"}"#)
            .unwrap();
        let cli = s
            .try_add_instance(r#"{"schemaNs":"chat","protocol":"Chat","role":"client"}"#)
            .unwrap();
        let conn = s.session.add_connection(&cli, &srv).unwrap();
        s.engine.sync(&s.session);

        let id = s.engine.call(&conn, "send", &json!(["hi"])).unwrap();
        s.engine.run();

        let out: Value = serde_json::from_str(&s.result(id as f64).unwrap()).unwrap();
        assert_eq!(out["status"], "ok");
        // default seeded behaviour is reply-with-zero-value
        assert_eq!(out["value"], json!({ "body": "", "seq": 0 }));

        let frames: Vec<Value> = serde_json::from_str(&s.frames(&conn)).unwrap();
        assert_eq!(frames.len(), 4, "2 handshake + request + response");
    }

    #[test]
    fn set_behavior_swaps_the_live_wire() {
        let mut s = sim();
        let srv = s
            .try_add_instance(r#"{"schemaNs":"chat","protocol":"Chat","role":"server"}"#)
            .unwrap();
        let cli = s
            .try_add_instance(r#"{"schemaNs":"chat","protocol":"Chat","role":"client"}"#)
            .unwrap();
        let conn = s.session.add_connection(&cli, &srv).unwrap();
        s.engine.sync(&s.session);

        s.try_set_behavior(&srv, "send", "echo", "{}").unwrap();
        let id = s.engine.call(&conn, "send", &json!(["echo me"])).unwrap();
        s.engine.run();

        let out: Value = serde_json::from_str(&s.result(id as f64).unwrap()).unwrap();
        assert_eq!(out["value"], json!(["echo me"]));
    }

    #[cfg(feature = "script")]
    #[test]
    fn a_scripted_behaviour_runs_through_the_facade() {
        let mut s = sim();
        let srv = s
            .try_add_instance(r#"{"schemaNs":"chat","protocol":"Chat","role":"server"}"#)
            .unwrap();
        let cli = s
            .try_add_instance(r#"{"schemaNs":"chat","protocol":"Chat","role":"client"}"#)
            .unwrap();
        let conn = s.session.add_connection(&cli, &srv).unwrap();
        s.engine.sync(&s.session);

        let cfg = json!({ "source": "#{ echoed: params[0] }" }).to_string();
        s.try_set_behavior(&srv, "send", "script", &cfg).unwrap();
        let id = s.engine.call(&conn, "send", &json!(["scripted!"])).unwrap();
        s.engine.run();

        let out: Value = serde_json::from_str(&s.result(id as f64).unwrap()).unwrap();
        assert_eq!(out["value"], json!({ "echoed": "scripted!" }));
    }

    #[test]
    fn link_round_trips_through_the_constructor() {
        let mut s = sim();
        s.try_add_instance(r#"{"schemaNs":"chat","protocol":"Chat","role":"server"}"#)
            .unwrap();
        s.set_latency(30.0);
        let link = s.link();

        let shape_json = serde_json::to_string(&chat::shape()).unwrap();
        let restored = Sim::try_new(&shape_json, Some(&link)).unwrap();
        assert_eq!(restored.session.instances.len(), 1);
        assert_eq!(restored.session.latency_ms, 30.0);
    }

    #[test]
    fn describe_frame_reads_a_request_back() {
        let mut s = sim();
        let srv = s
            .try_add_instance(r#"{"schemaNs":"chat","protocol":"Chat","role":"server"}"#)
            .unwrap();
        let cli = s
            .try_add_instance(r#"{"schemaNs":"chat","protocol":"Chat","role":"client"}"#)
            .unwrap();
        let conn = s.session.add_connection(&cli, &srv).unwrap();
        s.engine.sync(&s.session);
        let id = s.engine.call(&conn, "send", &json!(["hi"])).unwrap();
        s.engine.run();

        let req: Value = serde_json::from_str(&s.describe_frame(&conn, 3).unwrap()).unwrap();
        assert_eq!(req["kind"], "request");
        assert_eq!(req["fn"], "send");
        assert_eq!(req["params"], json!(["hi"]));
        let _ = id;
    }

    #[test]
    fn record_then_load_replay_reproduces_the_run() {
        let shape_json = serde_json::to_string(&chat::session().shape).unwrap();
        // build a live sim from a peopled session link
        let base = {
            let mut b = Sim::try_new(&shape_json, None).unwrap();
            b.session = chat::session();
            b.engine.sync(&b.session);
            b
        };
        let link = base.link();

        let mut s = Sim::try_new(&shape_json, Some(&link)).unwrap();
        s.set_clock_mode("stepped");
        s.record_start();
        s.call(chat::CONN, "send", "[\"one\"]").unwrap();
        s.run();
        assert_eq!(s.recording_count(), 1);
        let recording = s.record_stop();
        assert!(!s.recording());

        let mut fresh = Sim::try_new(&shape_json, None).unwrap();
        fresh.try_load_replay(&recording, &shape_json).unwrap();
        let frames: Vec<Value> = serde_json::from_str(&fresh.frames(chat::CONN)).unwrap();
        assert!(
            frames.iter().any(|f| f["kind"] == "response"),
            "the replayed call produced a reply"
        );
    }

    #[test]
    fn compare_returns_one_entry_per_transport_combo() {
        let mut s = Sim::try_new(&serde_json::to_string(&chat::shape()).unwrap(), None).unwrap();
        s.session = chat::session();
        s.engine.sync(&s.session);

        let out: Value = serde_json::from_str(
            &s.compare(chat::CONN, "send", "[\"hi\"]")
                .map_err(|_| ())
                .unwrap(),
        )
        .unwrap();
        let obj = out.as_object().unwrap();
        assert_eq!(obj.len(), 3);
        assert!(obj.contains_key("datagram/json"));
        assert!(obj.contains_key("datagram/msgpack"));
        assert!(obj.contains_key("jsonrpc/json"));
        for (label, run) in obj {
            assert_eq!(run["result"]["status"], "ok", "{label}");
            assert_eq!(
                run["result"]["value"],
                json!({ "body": "HELLO", "seq": 1 }),
                "{label}"
            );
        }
    }

    #[test]
    fn set_transport_switches_a_live_connection() {
        let mut s = Sim::try_new(&serde_json::to_string(&chat::shape()).unwrap(), None).unwrap();
        s.session = chat::session();
        s.engine.sync(&s.session);

        s.set_transport(chat::CONN, "jsonrpc", "json")
            .map_err(|_| ())
            .unwrap();
        let id = s.engine.call(chat::CONN, "send", &json!(["hi"])).unwrap();
        s.engine.run();
        assert_eq!(
            serde_json::from_str::<Value>(&s.result(id as f64).unwrap()).unwrap()["value"],
            json!({ "body": "HELLO", "seq": 1 })
        );
        // the request frame is now a json-rpc text frame
        let req: Value = serde_json::from_str(&s.describe_frame(chat::CONN, 3).unwrap()).unwrap();
        assert_eq!(req["framing"], "jsonrpc-2.0");

        // the parse succeeds but the combo is invalid → the connection is refused
        s.set_transport(chat::CONN, "jsonrpc", "msgpack")
            .map_err(|_| ())
            .unwrap();
        assert_eq!(
            s.connection_error(chat::CONN).as_deref(),
            Some("json-rpc framing requires the json codec")
        );
    }

    #[test]
    fn behavior_catalog_lists_kinds_and_gates_them() {
        let s = Sim::try_new(&serde_json::to_string(&chat::shape()).unwrap(), None).unwrap();

        let catalog: Vec<Value> =
            serde_json::from_str(&s.behavior_catalog("chat", "Chat", "send")).unwrap();
        assert_eq!(catalog.len(), 8);

        let by_kind = |k: &str| {
            catalog.iter().find(|e| e["kind"] == k).unwrap()["applies"]
                .as_bool()
                .unwrap()
        };
        assert!(by_kind("reply"));
        assert!(by_kind("increment")); // send returns a struct ref
        assert!(!by_kind("raise")); // send has no throws
        assert!(by_kind("script"));

        assert_eq!(s.behavior_catalog("chat", "Chat", "nope"), "[]");
        assert_eq!(s.default_behavior_kind("chat", "Chat", "send"), "reply");
    }
}
