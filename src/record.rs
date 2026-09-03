//! Record & replay. A recording is the session at record-start (as an
//! [`encode_session`](crate::session_codec::encode_session) string) plus the
//! ordered list of user inputs — a call, a behaviour edit, a fault edit — each
//! stamped with the clock time it happened at, relative to record-start.
//!
//! Ported from `record.ts`. The discrete-event clock makes [`replay_recording`]
//! deterministic for free — where the TS interleaves `clock.advance` with
//! `await tick()` / `Promise.allSettled`, here it is a straight loop:
//! `engine.advance(to the event's time)`, apply the input, drain what it
//! scheduled, repeat.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::engine::Engine;
use crate::faults::FaultSpec;
use crate::model::{BehaviorSetting, ClockMode, Session};
use crate::session_codec::{decode_session, encode_session};
use crate::shape::{find_protocol, ProjectShape};

/// One user input, in the flat shape `record.ts` writes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Input {
    /// A client call on a live connection.
    Call {
        #[serde(rename = "connId")]
        conn_id: String,
        #[serde(rename = "fn")]
        function: String,
        params: Value,
    },
    /// A server instance's behaviour was swapped for one function.
    Behavior {
        #[serde(rename = "instanceId")]
        instance_id: String,
        #[serde(rename = "fn")]
        function: String,
        setting: BehaviorSetting,
    },
    /// A connection's fault spec was edited.
    Fault {
        #[serde(rename = "connId")]
        conn_id: String,
        faults: FaultSpec,
    },
}

/// An [`Input`] plus the clock time it happened at (ms since record-start).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputEvent {
    #[serde(flatten)]
    pub input: Input,
    pub at: f64,
}

/// A full recording: the start-state session link and the timestamped inputs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recording {
    pub v: u32,
    /// [`encode_session`] of the session when recording started.
    pub session: String,
    pub events: Vec<InputEvent>,
}

/// Captures inputs against a running session for later replay.
#[derive(Debug, Default)]
pub struct Recorder {
    events: Vec<InputEvent>,
    snapshot: String,
    t0: f64,
    pub recording: bool,
}

impl Recorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot `session` and start capturing from clock time `now`.
    pub fn start(&mut self, session: &Session, now: f64) {
        self.snapshot = encode_session(session);
        self.events.clear();
        self.t0 = now;
        self.recording = true;
    }

    /// Record an input at clock time `now` (ignored unless recording).
    pub fn capture(&mut self, input: Input, now: f64) {
        if !self.recording {
            return;
        }
        self.events.push(InputEvent {
            input,
            at: (now - self.t0).max(0.0),
        });
    }

    /// Stop and hand back the recording.
    pub fn stop(&mut self) -> Recording {
        self.recording = false;
        Recording {
            v: 1,
            session: self.snapshot.clone(),
            events: self.events.clone(),
        }
    }

    pub fn count(&self) -> usize {
        self.events.len()
    }
}

/// The result of a replay: the reconstructed session and the engine that ran it.
/// Read the frame log off `engine.tap(conn_id)`.
pub struct Replay {
    pub session: Session,
    pub engine: Engine,
}

/// Drive `rec` on a fresh stepped engine against `shape`. Deterministic: same
/// recording + shape → same frame sequence, every run. `Err` if the recording's
/// session snapshot doesn't decode or its version is unknown.
pub fn replay_recording(rec: &Recording, shape: ProjectShape) -> Result<Replay, String> {
    if rec.v != 1 {
        return Err(format!("replay: unknown recording version {}", rec.v));
    }
    let mut session = decode_session(&rec.session, shape)
        .ok_or("replay: the recording's session did not decode")?;
    session.clock_mode = ClockMode::Stepped;

    let mut engine = Engine::new();
    engine.rebuild(&session);

    for event in &rec.events {
        engine.advance((event.at - engine.now()).max(0.0));
        apply_input(&mut engine, &mut session, &event.input);
        engine.advance(0.0); // fire anything the input scheduled for right now
    }
    // let everything still in flight land
    engine.advance(5000.0);

    Ok(Replay { session, engine })
}

fn apply_input(engine: &mut Engine, session: &mut Session, input: &Input) {
    match input {
        Input::Call {
            conn_id,
            function,
            params,
        } => {
            if engine.connection_error(conn_id).is_none() {
                let _ = engine.call(conn_id, function, params);
            }
        }
        Input::Behavior {
            instance_id,
            function,
            setting,
        } => {
            let _ = session.set_behavior(
                instance_id,
                function,
                setting.kind,
                Some(setting.config.clone()),
            );
            let schema = session.instance(instance_id).and_then(|inst| {
                find_protocol(&session.shape, &inst.schema_ns, &inst.protocol)
                    .map(|(schema, _)| schema.clone())
            });
            if let Some(schema) = schema {
                for conn_id in engine.connections_for_instance(instance_id) {
                    let _ = engine.set_behavior(
                        &conn_id,
                        function,
                        setting.kind,
                        &setting.config,
                        &schema,
                    );
                }
            }
        }
        Input::Fault { conn_id, faults } => {
            if let Some(conn) = session.connections.iter_mut().find(|c| c.id == *conn_id) {
                conn.faults = faults.clone();
            }
            engine.set_faults(conn_id, faults.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::behavior::BehaviorKind;
    use crate::faults::no_faults;
    use crate::model::{ClockMode, InstanceSpec, Placement, Role};
    use crate::session_codec::decode_session;
    use crate::shape::{FnShape, Framing, ProjectShape, ProtocolShape, SchemaShape, TypeRef};
    use serde_json::json;

    fn shape() -> ProjectShape {
        ProjectShape {
            schemas: vec![SchemaShape {
                namespace: "chat".into(),
                ir_hash: "0x1".into(),
                protocols: vec![ProtocolShape {
                    name: "Chat".into(),
                    framing: Framing::Datagram,
                    functions: vec![FnShape {
                        name: "send".into(),
                        index: 0,
                        oneway: false,
                        args: vec![],
                        returns: Some(TypeRef::Unit),
                        throws: vec![],
                    }],
                }],
                errors: vec![],
                types: vec![],
            }],
        }
    }

    fn session() -> Session {
        let mut s = Session::empty(shape());
        s.add_instance(
            InstanceSpec {
                schema_ns: "chat".into(),
                protocol: "Chat".into(),
                role: Role::Server,
            },
            Placement::default(),
        );
        s
    }

    #[test]
    fn input_serializes_flat_with_a_kind_tag_and_camel_case_keys() {
        let call = InputEvent {
            input: Input::Call {
                conn_id: "c1".into(),
                function: "send".into(),
                params: json!(["hi"]),
            },
            at: 12.5,
        };
        let v = serde_json::to_value(&call).unwrap();
        assert_eq!(v["kind"], "call");
        assert_eq!(v["connId"], "c1");
        assert_eq!(v["fn"], "send");
        assert_eq!(v["params"], json!(["hi"]));
        assert_eq!(v["at"], 12.5);

        let back: InputEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back, call);
    }

    #[test]
    fn every_input_variant_round_trips() {
        let events = vec![
            InputEvent {
                input: Input::Call {
                    conn_id: "c1".into(),
                    function: "send".into(),
                    params: Value::Null,
                },
                at: 0.0,
            },
            InputEvent {
                input: Input::Behavior {
                    instance_id: "i1".into(),
                    function: "send".into(),
                    setting: BehaviorSetting {
                        kind: BehaviorKind::Echo,
                        config: json!({}),
                    },
                },
                at: 5.0,
            },
            InputEvent {
                input: Input::Fault {
                    conn_id: "c1".into(),
                    faults: no_faults(),
                },
                at: 9.0,
            },
        ];
        let rec = Recording {
            v: 1,
            session: "AAA".into(),
            events: events.clone(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert_eq!(serde_json::from_str::<Recording>(&json).unwrap(), rec);
    }

    #[test]
    fn recorder_snapshots_at_start_and_stamps_relative_times() {
        let mut r = Recorder::new();
        assert!(!r.recording);

        r.capture(
            Input::Fault {
                conn_id: "c1".into(),
                faults: no_faults(),
            },
            5.0,
        );
        assert_eq!(r.count(), 0, "ignored before start");

        let mut s = session();
        s.clock_mode = ClockMode::Stepped;
        r.start(&s, 100.0);
        assert!(r.recording);

        r.capture(
            Input::Call {
                conn_id: "c1".into(),
                function: "send".into(),
                params: Value::Null,
            },
            150.0,
        );
        r.capture(
            Input::Call {
                conn_id: "c1".into(),
                function: "send".into(),
                params: Value::Null,
            },
            90.0,
        );

        let rec = r.stop();
        assert!(!r.recording);
        assert_eq!(rec.v, 1);
        assert_eq!(rec.events.len(), 2);
        assert_eq!(rec.events[0].at, 50.0, "150 − 100");
        assert_eq!(rec.events[1].at, 0.0, "clamped, never negative");

        // the snapshot is a real session link
        let restored = decode_session(&rec.session, shape()).expect("snapshot decodes");
        assert_eq!(restored.clock_mode, ClockMode::Stepped);
        assert_eq!(restored.instances.len(), 1);
    }

    #[test]
    fn a_second_start_clears_the_first_run() {
        let mut r = Recorder::new();
        r.start(&session(), 0.0);
        r.capture(
            Input::Call {
                conn_id: "c1".into(),
                function: "send".into(),
                params: Value::Null,
            },
            1.0,
        );
        r.start(&session(), 0.0);
        assert_eq!(r.count(), 0);
    }

    // ── replay ──────────────────────────────────────────────────────────

    use crate::fixtures as chat;

    fn call_at(at: f64, params: Value) -> InputEvent {
        InputEvent {
            input: Input::Call {
                conn_id: chat::CONN.into(),
                function: "send".into(),
                params,
            },
            at,
        }
    }

    /// A recording of two `send` calls on the chat session.
    fn two_call_recording() -> Recording {
        let mut r = Recorder::new();
        r.start(&chat::session(), 0.0);
        Recording {
            v: 1,
            session: r.stop().session,
            events: vec![call_at(0.0, json!(["one"])), call_at(200.0, json!(["two"]))],
        }
    }

    #[test]
    fn replay_is_deterministic() {
        let rec = two_call_recording();
        let frames = |r: &Recording| {
            replay_recording(r, chat::shape())
                .unwrap()
                .engine
                .tap(chat::CONN)
                .unwrap()
                .frames
                .iter()
                .map(|f| (f.bytes.clone(), f.at, f.fault.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(frames(&rec), frames(&rec));
    }

    #[test]
    fn replay_runs_the_recorded_calls() {
        let replay = replay_recording(&two_call_recording(), chat::shape()).unwrap();
        let payload: Vec<_> = replay
            .engine
            .tap(chat::CONN)
            .unwrap()
            .frames
            .iter()
            .filter(|f| f.kind == crate::frame::FrameKind::Response)
            .collect();
        assert_eq!(payload.len(), 2, "both calls got a reply");
        assert!(payload.iter().all(|f| f.from == "chat-1"));
    }

    #[test]
    fn replay_applies_a_recorded_behaviour_edit() {
        let mut rec = two_call_recording();
        let server_id = {
            let s = chat::session();
            s.instances
                .iter()
                .find(|i| i.role == Role::Server)
                .unwrap()
                .id
                .clone()
        };
        // between the two calls, switch `send` to echo
        rec.events.insert(
            1,
            InputEvent {
                input: Input::Behavior {
                    instance_id: server_id,
                    function: "send".into(),
                    setting: BehaviorSetting {
                        kind: BehaviorKind::Echo,
                        config: json!({}),
                    },
                },
                at: 100.0,
            },
        );

        let replay = replay_recording(&rec, chat::shape()).unwrap();
        let bodies: Vec<Value> = replay
            .engine
            .tap(chat::CONN)
            .unwrap()
            .frames
            .iter()
            .filter(|f| f.kind == crate::frame::FrameKind::Response)
            .map(|f| {
                // datagram response: [request_id u64][tag u8][json body]
                serde_json::from_slice(&f.bytes[9..]).unwrap()
            })
            .collect();
        assert_eq!(
            bodies[0],
            json!({ "body": "HELLO", "seq": 1 }),
            "first call: reply"
        );
        assert_eq!(
            bodies[1],
            json!(["two"]),
            "second call: echoed after the edit"
        );
    }

    #[test]
    fn replay_applies_a_recorded_fault_edit() {
        let mut rec = two_call_recording();
        let mut cut = no_faults();
        cut.drop_prob = 1.0;
        cut.apply_to = crate::faults::FaultDir::Responses;
        rec.events.insert(
            1,
            InputEvent {
                input: Input::Fault {
                    conn_id: chat::CONN.into(),
                    faults: cut,
                },
                at: 100.0,
            },
        );

        let replay = replay_recording(&rec, chat::shape()).unwrap();
        let responses = replay
            .engine
            .tap(chat::CONN)
            .unwrap()
            .frames
            .iter()
            .filter(|f| f.kind == crate::frame::FrameKind::Response && f.fault.is_none())
            .count();
        assert_eq!(responses, 1, "only the pre-fault reply got through clean");
    }

    #[test]
    fn replay_rejects_a_bad_session_snapshot() {
        let rec = Recording {
            v: 1,
            session: "not a session".into(),
            events: vec![],
        };
        assert!(replay_recording(&rec, chat::shape()).is_err());

        let wrong_version = Recording {
            v: 2,
            session: two_call_recording().session,
            events: vec![],
        };
        assert!(replay_recording(&wrong_version, chat::shape()).is_err());
    }
}
