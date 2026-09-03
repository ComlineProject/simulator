//! Route B: one dispatcher that reads a [`ProtocolShape`] and does what a
//! generated `<Proto>Dispatcher` does — resolve the call, decode the params, and
//! run the instance's [`Behavior`] for that function. So any compiled schema is
//! driven without code generation.
//!
//! Ported from `GenericDispatch` in the playground's `generic.ts`. The consumer
//! side (`GenericClient`) has no analogue here — the [`Pump`](crate::pump::Pump)
//! owns the client half of a call directly, since there is no blocking
//! `Client::call` to wrap. Framing the outcome into a response is the pump's job
//! too; this type stops at the [`BehaviorStep`].

use std::collections::HashMap;

use comline_runtime::contract::{RequestCall, RuntimeError, WireFormat};
use serde_json::Value;

use crate::behavior::{Behavior, BehaviorCtx, BehaviorStep};
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

    /// Resolve the call and run its behaviour. `behaviors` is passed in (not
    /// held) so the pump keeps ownership of the live, swappable map.
    pub fn dispatch<W: WireFormat>(
        &self,
        call: RequestCall<'_>,
        params: &[u8],
        fmt: &W,
        behaviors: &mut BehaviorMap,
    ) -> Result<BehaviorStep, RuntimeError> {
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

        Ok(behavior.run(BehaviorCtx {
            params: decoded,
            function,
            protocol: &self.proto,
        }))
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

    use crate::behavior::{BehaviorKind, Echo, SimOutcome};
    use crate::format::Json;
    use crate::shape::{FnShape, Framing, SchemaShape, TypeDef, TypeRef};

    fn chat_proto() -> ProtocolShape {
        ProtocolShape {
            name: "Chat".into(),
            framing: Framing::Datagram,
            functions: vec![FnShape {
                name: "send".into(),
                index: 0,
                oneway: false,
                args: vec![],
                returns: Some(TypeRef::Ref {
                    name: "Message".into(),
                }),
                throws: vec![],
            }],
        }
    }

    fn chat_schema() -> SchemaShape {
        SchemaShape {
            namespace: "chat".into(),
            ir_hash: "0x0".into(),
            protocols: vec![chat_proto()],
            errors: vec![],
            types: vec![TypeDef::Struct {
                name: "Message".into(),
                fields: vec![],
            }],
        }
    }

    fn step(
        d: &GenericDispatch,
        call: RequestCall<'_>,
        params: &Value,
        behaviors: &mut BehaviorMap,
    ) -> Result<BehaviorStep, RuntimeError> {
        let mut bytes = Vec::new();
        Json.encode(params, &mut bytes).unwrap();
        d.dispatch(call, &bytes, &Json, behaviors)
    }

    #[test]
    fn resolves_by_id_and_by_name() {
        let d = GenericDispatch::new(chat_proto());
        let mut behaviors: BehaviorMap = HashMap::new();
        behaviors.insert("send".into(), Box::new(Echo));

        assert!(matches!(
            step(&d, RequestCall::Id(0), &Value::Null, &mut behaviors).unwrap(),
            BehaviorStep::Now(_)
        ));
        assert!(matches!(
            step(&d, RequestCall::Name("send"), &Value::Null, &mut behaviors).unwrap(),
            BehaviorStep::Now(_)
        ));
    }

    #[test]
    fn an_unknown_call_or_unbound_function_is_rejected() {
        let d = GenericDispatch::new(chat_proto());
        let mut behaviors: BehaviorMap = HashMap::new();

        assert_eq!(
            step(&d, RequestCall::Id(0), &Value::Null, &mut behaviors).unwrap_err(),
            RuntimeError::UnknownCall,
            "no behaviour bound"
        );
        behaviors.insert("send".into(), Box::new(Echo));
        assert_eq!(
            step(&d, RequestCall::Id(9), &Value::Null, &mut behaviors).unwrap_err(),
            RuntimeError::UnknownCall
        );
        assert_eq!(
            step(&d, RequestCall::Name("nope"), &Value::Null, &mut behaviors).unwrap_err(),
            RuntimeError::UnknownCall
        );
    }

    #[test]
    fn params_reach_the_behaviour_decoded() {
        let d = GenericDispatch::new(chat_proto());
        let mut behaviors: BehaviorMap = HashMap::new();
        behaviors.insert("send".into(), Box::new(Echo));

        let params = serde_json::json!(["hello"]);
        assert_eq!(
            step(&d, RequestCall::Id(0), &params, &mut behaviors).unwrap(),
            BehaviorStep::Now(SimOutcome::Ok(params))
        );
    }

    #[test]
    fn a_delay_behaviour_surfaces_as_an_after_step() {
        let d = GenericDispatch::new(chat_proto());
        let schema = chat_schema();
        let mut behaviors: BehaviorMap = HashMap::new();
        behaviors.insert(
            "send".into(),
            BehaviorKind::Delay.make(
                &serde_json::json!({ "ms": 500, "value": null }),
                &chat_proto().functions[0],
                &schema,
            ),
        );

        assert_eq!(
            step(&d, RequestCall::Id(0), &Value::Null, &mut behaviors).unwrap(),
            BehaviorStep::After {
                delay_ms: 500.0,
                outcome: SimOutcome::Ok(Value::Null)
            }
        );
    }
}
