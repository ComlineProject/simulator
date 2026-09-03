//! Record & replay. A recording is the session at record-start (as an
//! [`encode_session`](crate::session_codec::encode_session) string) plus the
//! ordered list of user inputs — a call, a behaviour edit, a fault edit — each
//! stamped with the clock time it happened at, relative to record-start.
//!
//! Ported from `record.ts`. This slice carries the data types and the
//! [`Recorder`]; `replay_recording` lands with the `engine` port, since it needs
//! a running `Wires`. The discrete-event clock makes that replay deterministic
//! for free — no `Promise.allSettled` / `tick()` dance.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::faults::FaultSpec;
use crate::model::{BehaviorSetting, Session};
use crate::session_codec::encode_session;

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
}
