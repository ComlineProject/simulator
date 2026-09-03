//! What a simulated server instance does for one function when a call arrives.
//! Ported from the playground's `behavior.ts` — for now just the trait, the
//! outcome type, and the two trivial behaviours needed to exercise
//! [`GenericDispatch`](crate::generic::GenericDispatch). The full canned
//! catalogue (`increment`, `delay`, `raise`, `drop`, `forward`) and the
//! Rhai-scripted behaviour land in later slices.

use serde_json::Value;

use crate::shape::{FnShape, ProtocolShape};

/// What a behaviour produced for one call. The dispatcher turns this into the
/// response frame — `Ok` / `Err` envelope, or nothing for a one-way call.
#[derive(Clone, Debug, PartialEq)]
pub enum SimOutcome {
    /// A success value (`Value::Null` for a unit return).
    Ok(Value),
    /// A raised schema error: its schema-global `ordinal` and `data` body.
    Err { ordinal: u16, data: Value },
    /// One-way — nothing to send back.
    None,
}

/// The context a behaviour runs against.
pub struct BehaviorCtx<'a> {
    /// The decoded request params (`Value::Null` when the call takes none).
    pub params: Value,
    pub function: &'a FnShape,
    pub protocol: &'a ProtocolShape,
    // later slices: `forward` (relay to another wire) and `clock` (for `delay`)
}

/// A server instance's handler for one function.
pub trait Behavior {
    fn run(&mut self, ctx: BehaviorCtx<'_>) -> SimOutcome;
}

/// `Ok(value)` for a normal function, `None` for a one-way one.
fn ok_or_none(function: &FnShape, value: Value) -> SimOutcome {
    if function.oneway {
        SimOutcome::None
    } else {
        SimOutcome::Ok(value)
    }
}

/// "Reply with value" — always answers with a fixed value.
pub struct ReplyWith {
    pub value: Value,
}

impl Behavior for ReplyWith {
    fn run(&mut self, ctx: BehaviorCtx<'_>) -> SimOutcome {
        ok_or_none(ctx.function, self.value.clone())
    }
}

/// "Echo params" — answers with the request params verbatim.
pub struct Echo;

impl Behavior for Echo {
    fn run(&mut self, ctx: BehaviorCtx<'_>) -> SimOutcome {
        let params = ctx.params.clone();
        ok_or_none(ctx.function, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shape::Framing;

    fn proto(oneway: bool) -> ProtocolShape {
        ProtocolShape {
            name: "P".into(),
            framing: Framing::Datagram,
            functions: vec![FnShape {
                name: "f".into(),
                index: 0,
                oneway,
                args: vec![],
                returns: None,
                throws: vec![],
            }],
        }
    }

    fn ctx<'a>(p: &'a ProtocolShape, params: Value) -> BehaviorCtx<'a> {
        BehaviorCtx {
            params,
            function: &p.functions[0],
            protocol: p,
        }
    }

    #[test]
    fn reply_with_returns_the_fixed_value() {
        let p = proto(false);
        let mut b = ReplyWith {
            value: serde_json::json!({ "ok": true }),
        };
        assert_eq!(
            b.run(ctx(&p, Value::Null)),
            SimOutcome::Ok(serde_json::json!({ "ok": true }))
        );
    }

    #[test]
    fn echo_returns_the_params() {
        let p = proto(false);
        let mut b = Echo;
        assert_eq!(
            b.run(ctx(&p, serde_json::json!([1, 2, 3]))),
            SimOutcome::Ok(serde_json::json!([1, 2, 3]))
        );
    }

    #[test]
    fn a_one_way_function_sends_nothing_back() {
        let p = proto(true);
        let mut b = Echo;
        assert_eq!(b.run(ctx(&p, serde_json::json!(["x"]))), SimOutcome::None);
    }
}
