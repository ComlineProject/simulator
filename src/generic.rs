//! Route B: one dispatcher that reads a [`ProtocolShape`] and does what a
//! generated `<Proto>Dispatcher` does — resolve the call, decode the params, run
//! the instance's [`Behavior`] for that function, and write the outcome to the
//! [`Reply`] in the three shapes a generated dispatcher writes. So any compiled
//! schema is driven without code generation.
//!
//! Ported from `GenericDispatch` in the playground's `generic.ts`. The consumer
//! side (`GenericClient`) has no analogue here — the [`Pump`](crate::pump::Pump)
//! owns the client half of a call directly, since there is no blocking
//! `Client::call` to wrap.

use std::collections::HashMap;

use comline_runtime::contract::{Reply, RequestCall, RuntimeError, WireFormat};
use serde_json::Value;

use crate::behavior::{Behavior, BehaviorCtx, SimOutcome};
use crate::shape::ProtocolShape;

/// One behaviour per function name. The engine swaps entries live so a
/// behaviour change takes effect on the next call with no reconnect.
pub type BehaviorMap = HashMap<String, Box<dyn Behavior>>;

/// The provider side for one protocol.
pub struct GenericDispatch {
    proto: ProtocolShape,
}

impl GenericDispatch {
    pub fn new(proto: ProtocolShape) -> Self {
        Self { proto }
    }

    pub fn protocol(&self) -> &ProtocolShape {
        &self.proto
    }

    /// The protocol's function names, in declaration order.
    pub fn call_names(&self) -> Vec<&str> {
        self.proto
            .functions
            .iter()
            .map(|f| f.name.as_str())
            .collect()
    }

    /// Resolve, decode, run, encode. `behaviors` is passed in (not held) so the
    /// pump keeps ownership of the live, swappable map.
    pub fn dispatch<W: WireFormat>(
        &self,
        call: RequestCall<'_>,
        params: &[u8],
        fmt: &W,
        behaviors: &mut BehaviorMap,
        reply: &mut Reply<'_>,
    ) -> Result<(), RuntimeError> {
        let idx = self.resolve(call).ok_or(RuntimeError::UnknownCall)?;
        let function = &self.proto.functions[idx];

        let behavior = behaviors
            .get_mut(&function.name)
            .ok_or(RuntimeError::UnknownCall)?;

        let decoded = if params.is_empty() {
            Value::Null
        } else {
            fmt.decode::<Value>(params)?
        };

        let outcome = behavior.run(BehaviorCtx {
            params: decoded,
            function,
            protocol: &self.proto,
        });

        match outcome {
            SimOutcome::Ok(value) => {
                let mut body = Vec::new();
                fmt.encode(&value, &mut body)?;
                reply.ok(&body);
            }
            SimOutcome::Err { ordinal, data } => {
                let mut body = Vec::new();
                fmt.encode(&data, &mut body)?;
                reply.err(ordinal, &body);
            }
            SimOutcome::None => {} // one-way — nothing to send
        }
        Ok(())
    }

    fn resolve(&self, call: RequestCall<'_>) -> Option<usize> {
        match call {
            RequestCall::Id(id) => {
                let idx = id as usize;
                (idx < self.proto.functions.len()).then_some(idx)
            }
            RequestCall::Name(name) => self.proto.functions.iter().position(|f| f.name == name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use comline_runtime::contract::Outcome;

    use crate::behavior::{Echo, ReplyWith};
    use crate::format::Json;
    use crate::shape::{FnShape, Framing};

    fn chat_proto() -> ProtocolShape {
        ProtocolShape {
            name: "Chat".into(),
            framing: Framing::Datagram,
            functions: vec![FnShape {
                name: "send".into(),
                index: 0,
                oneway: false,
                args: vec![],
                returns: None,
                throws: vec![],
            }],
        }
    }

    fn run(
        d: &GenericDispatch,
        call: RequestCall<'_>,
        params: &Value,
        behaviors: &mut BehaviorMap,
    ) -> Result<(Outcome, Value), RuntimeError> {
        let mut param_bytes = Vec::new();
        Json.encode(params, &mut param_bytes).unwrap();
        let mut body = Vec::new();
        let outcome = {
            let mut reply = Reply::new(&mut body);
            d.dispatch(call, &param_bytes, &Json, behaviors, &mut reply)?;
            reply.outcome()
        };
        let decoded = if body.is_empty() {
            Value::Null
        } else {
            Json.decode::<Value>(&body).unwrap()
        };
        Ok((outcome, decoded))
    }

    #[test]
    fn resolves_by_id_and_by_name() {
        let d = GenericDispatch::new(chat_proto());
        let mut behaviors: BehaviorMap = HashMap::new();
        behaviors.insert(
            "send".into(),
            Box::new(ReplyWith {
                value: Value::from(1),
            }),
        );

        let (o1, _) = run(&d, RequestCall::Id(0), &Value::Null, &mut behaviors).unwrap();
        let (o2, _) = run(&d, RequestCall::Name("send"), &Value::Null, &mut behaviors).unwrap();
        assert_eq!(o1, Outcome::Ok);
        assert_eq!(o2, Outcome::Ok);
    }

    #[test]
    fn an_unknown_call_is_rejected() {
        let d = GenericDispatch::new(chat_proto());
        let mut behaviors: BehaviorMap = HashMap::new();
        behaviors.insert("send".into(), Box::new(Echo));

        assert_eq!(
            run(&d, RequestCall::Id(9), &Value::Null, &mut behaviors).unwrap_err(),
            RuntimeError::UnknownCall
        );
        assert_eq!(
            run(&d, RequestCall::Name("nope"), &Value::Null, &mut behaviors).unwrap_err(),
            RuntimeError::UnknownCall
        );
    }

    #[test]
    fn a_function_with_no_behaviour_set_is_rejected() {
        let d = GenericDispatch::new(chat_proto());
        let mut behaviors: BehaviorMap = HashMap::new();
        assert_eq!(
            run(&d, RequestCall::Id(0), &Value::Null, &mut behaviors).unwrap_err(),
            RuntimeError::UnknownCall
        );
    }

    #[test]
    fn echo_round_trips_the_params_through_the_wire_format() {
        let d = GenericDispatch::new(chat_proto());
        let mut behaviors: BehaviorMap = HashMap::new();
        behaviors.insert("send".into(), Box::new(Echo));

        let params = serde_json::json!(["hello"]);
        let (outcome, body) = run(&d, RequestCall::Id(0), &params, &mut behaviors).unwrap();
        assert_eq!(outcome, Outcome::Ok);
        assert_eq!(body, params);
    }

    #[test]
    fn a_raised_error_becomes_an_err_outcome() {
        struct Raise;
        impl Behavior for Raise {
            fn run(&mut self, _ctx: BehaviorCtx<'_>) -> SimOutcome {
                SimOutcome::Err {
                    ordinal: 7,
                    data: serde_json::json!({ "why": "no" }),
                }
            }
        }
        let d = GenericDispatch::new(chat_proto());
        let mut behaviors: BehaviorMap = HashMap::new();
        behaviors.insert("send".into(), Box::new(Raise));

        let (outcome, body) = run(&d, RequestCall::Id(0), &Value::Null, &mut behaviors).unwrap();
        assert_eq!(outcome, Outcome::Err(7));
        assert_eq!(body, serde_json::json!({ "why": "no" }));
    }
}
