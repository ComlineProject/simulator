//! What a simulated server instance does for one function when a call arrives.
//! Ported from the playground's `behavior.ts`: the [`Behavior`] trait, the
//! canned catalogue keyed by [`BehaviorKind`] (label / applies-to / default
//! config / factory), and `default_kind_for`.
//!
//! The async parts of the TS version fold into the [`BehaviorStep`] a `run`
//! returns: `delay` yields [`BehaviorStep::After`] and the pump schedules the
//! reply on the clock; `drop` yields [`BehaviorStep::Hang`]; `forward` yields
//! [`BehaviorStep::Forward`] and the engine relays it (until the `engine` port
//! the pump answers it with the same "only in the engine" error the TS does).

use serde_json::{Map, Value};

use crate::shape::{is_numeric_prim, FnShape, SchemaShape, TypeDef, TypeRef};

/// What a behaviour produced for one call. The success / error *value* is fixed
/// when `run` returns; only *when* it is sent varies.
#[derive(Clone, Debug, PartialEq)]
pub enum SimOutcome {
    /// A success value (`Value::Null` for a unit return).
    Ok(Value),
    /// A raised schema error: its schema-global `ordinal` and `data` body.
    Err { ordinal: u16, data: Value },
    /// One-way — nothing to send back.
    None,
}

/// What the pump should do with a dispatched call.
#[derive(Clone, Debug, PartialEq)]
pub enum BehaviorStep {
    /// Reply now.
    Now(SimOutcome),
    /// Reply after `delay_ms` of clock time.
    After { delay_ms: f64, outcome: SimOutcome },
    /// Never reply — the client's call hangs, like a dead peer.
    Hang,
    /// Relay this call over another connection, then reply with its outcome.
    /// Handled by the engine; the bare pump can't (no other wire to relay on).
    Forward {
        via: String,
        target_fn: String,
        params: Value,
    },
}

/// The context a behaviour runs against.
pub struct BehaviorCtx<'a> {
    /// The decoded request params (`Value::Null` when the call takes none).
    pub params: Value,
    pub function: &'a FnShape,
    pub protocol: &'a crate::shape::ProtocolShape,
}

/// A server instance's handler for one function.
pub trait Behavior {
    fn run(&mut self, ctx: BehaviorCtx<'_>) -> BehaviorStep;
}

/// `Ok(value)` for a normal function, `None` for a one-way one.
fn ok_or_none(function: &FnShape, value: Value) -> SimOutcome {
    if function.oneway {
        SimOutcome::None
    } else {
        SimOutcome::Ok(value)
    }
}

// ── the canned behaviours ────────────────────────────────────────────────

/// "Reply with value" — always answers with a fixed value.
pub struct ReplyWith {
    pub value: Value,
}
impl Behavior for ReplyWith {
    fn run(&mut self, ctx: BehaviorCtx<'_>) -> BehaviorStep {
        BehaviorStep::Now(ok_or_none(ctx.function, self.value.clone()))
    }
}

/// "Echo params" — answers with the request params verbatim.
pub struct Echo;
impl Behavior for Echo {
    fn run(&mut self, ctx: BehaviorCtx<'_>) -> BehaviorStep {
        let params = ctx.params.clone();
        BehaviorStep::Now(ok_or_none(ctx.function, params))
    }
}

/// "Increment field" — a running value; one numeric field goes up by 1 each call.
pub struct Increment {
    base: Value,
    path: String,
    current: Option<Value>,
}
impl Behavior for Increment {
    fn run(&mut self, ctx: BehaviorCtx<'_>) -> BehaviorStep {
        let cur = self.current.get_or_insert_with(|| self.base.clone());
        if !self.path.is_empty() {
            let n = get_at(cur, &self.path)
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            set_at(cur, &self.path, number(n + 1.0));
        }
        BehaviorStep::Now(ok_or_none(ctx.function, cur.clone()))
    }
}

/// "Delay then reply" — a fixed value, `ms` of clock time later.
pub struct Delay {
    ms: f64,
    value: Value,
}
impl Behavior for Delay {
    fn run(&mut self, ctx: BehaviorCtx<'_>) -> BehaviorStep {
        BehaviorStep::After {
            delay_ms: self.ms.max(0.0),
            outcome: ok_or_none(ctx.function, self.value.clone()),
        }
    }
}

/// "Raise error" — answers with a schema error.
pub struct Raise {
    ordinal: u16,
    data: Value,
}
impl Behavior for Raise {
    fn run(&mut self, _ctx: BehaviorCtx<'_>) -> BehaviorStep {
        BehaviorStep::Now(SimOutcome::Err {
            ordinal: self.ordinal,
            data: self.data.clone(),
        })
    }
}

/// "Drop (never reply)" — the call hangs.
pub struct Drop;
impl Behavior for Drop {
    fn run(&mut self, _ctx: BehaviorCtx<'_>) -> BehaviorStep {
        BehaviorStep::Hang
    }
}

/// "Forward to another server" — relay over another connection.
pub struct Forward {
    via: String,
    target_fn: String,
}
impl Behavior for Forward {
    fn run(&mut self, ctx: BehaviorCtx<'_>) -> BehaviorStep {
        if self.via.is_empty() {
            return BehaviorStep::Now(SimOutcome::Err {
                ordinal: 0,
                data: serde_json::json!({ "error": "forward: pick a connection" }),
            });
        }
        BehaviorStep::Forward {
            via: self.via.clone(),
            target_fn: if self.target_fn.is_empty() {
                ctx.function.name.clone()
            } else {
                self.target_fn.clone()
            },
            params: ctx.params.clone(),
        }
    }
}

/// "Run a script (Rhai)" — a user-written handler. `params` (the decoded
/// request) and a persistent `state` map are in scope; the last expression is
/// the reply, `throw` raises an error.
#[cfg(feature = "script")]
mod script {
    use super::{ok_or_none, Behavior, BehaviorCtx, BehaviorStep, SimOutcome};
    use serde_json::Value;

    thread_local! {
        static ENGINE: rhai::Engine = build_engine();
    }

    /// A sandboxed engine: Rhai has no I/O to begin with, so this is about
    /// bounding work and memory. `print` / `debug` are silenced.
    fn build_engine() -> rhai::Engine {
        let mut engine = rhai::Engine::new();
        engine.set_max_operations(200_000);
        engine.set_max_call_levels(48);
        engine.set_max_expr_depths(64, 32);
        engine.set_max_string_size(16 * 1024);
        engine.set_max_array_size(8 * 1024);
        engine.set_max_map_size(8 * 1024);
        engine.on_print(|_| {});
        engine.on_debug(|_, _, _| {});
        engine
    }

    pub struct Script {
        ast: Option<rhai::AST>,
        error: Option<String>,
        state: rhai::Dynamic,
    }

    pub fn make(source: &str) -> Box<dyn Behavior> {
        let (ast, error) = ENGINE.with(|engine| match engine.compile(source) {
            Ok(ast) => (Some(ast), None),
            Err(err) => (None, Some(format!("script did not compile: {err}"))),
        });
        Box::new(Script {
            ast,
            error,
            state: rhai::Map::new().into(),
        })
    }

    impl Behavior for Script {
        fn run(&mut self, ctx: BehaviorCtx<'_>) -> BehaviorStep {
            let Some(ast) = &self.ast else {
                return err(self.error.clone().unwrap_or_default());
            };
            ENGINE.with(|engine| {
                let mut scope = rhai::Scope::new();
                let params = rhai::serde::to_dynamic(&ctx.params).unwrap_or(rhai::Dynamic::UNIT);
                scope.push_dynamic("params", params);
                scope.push_dynamic("state", self.state.clone());

                let outcome = engine.eval_ast_with_scope::<rhai::Dynamic>(&mut scope, ast);
                if let Some(next) = scope.get_value::<rhai::Dynamic>("state") {
                    self.state = next;
                }
                match outcome {
                    Ok(value) => {
                        let value =
                            rhai::serde::from_dynamic::<Value>(&value).unwrap_or(Value::Null);
                        BehaviorStep::Now(ok_or_none(ctx.function, value))
                    }
                    Err(e) => err(e.to_string()),
                }
            })
        }
    }

    fn err(message: String) -> BehaviorStep {
        BehaviorStep::Now(SimOutcome::Err {
            ordinal: 0,
            data: serde_json::json!({ "error": message }),
        })
    }
}

/// Without the `script` feature, the scripted behaviour is a stub that reports
/// it isn't available.
#[cfg(not(feature = "script"))]
mod script {
    use super::{Behavior, BehaviorCtx, BehaviorStep, SimOutcome};

    struct Disabled;

    pub fn make(_source: &str) -> Box<dyn Behavior> {
        Box::new(Disabled)
    }

    impl Behavior for Disabled {
        fn run(&mut self, _ctx: BehaviorCtx<'_>) -> BehaviorStep {
            BehaviorStep::Now(SimOutcome::Err {
                ordinal: 0,
                data: serde_json::json!({ "error": "scripting is not enabled in this build" }),
            })
        }
    }
}

// ── the catalogue ───────────────────────────────────────────────────────

/// The canned server behaviours a simulated instance can run for one function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BehaviorKind {
    Reply,
    Echo,
    Increment,
    Delay,
    Raise,
    Drop,
    Forward,
    /// A user-written Rhai script (milestone 2f). Sandboxed: no I/O, capped
    /// operations / string / collection sizes.
    Script,
}

impl BehaviorKind {
    pub const ALL: [BehaviorKind; 8] = [
        BehaviorKind::Reply,
        BehaviorKind::Echo,
        BehaviorKind::Increment,
        BehaviorKind::Delay,
        BehaviorKind::Raise,
        BehaviorKind::Drop,
        BehaviorKind::Forward,
        BehaviorKind::Script,
    ];

    pub fn label(self) -> &'static str {
        match self {
            BehaviorKind::Reply => "Reply with value",
            BehaviorKind::Echo => "Echo params",
            BehaviorKind::Increment => "Increment field",
            BehaviorKind::Delay => "Delay then reply",
            BehaviorKind::Raise => "Raise error",
            BehaviorKind::Drop => "Drop (never reply)",
            BehaviorKind::Forward => "Forward to another server",
            BehaviorKind::Script => "Run a script (Rhai)",
        }
    }

    /// The lowercase tag it serializes as.
    pub fn as_str(self) -> &'static str {
        match self {
            BehaviorKind::Reply => "reply",
            BehaviorKind::Echo => "echo",
            BehaviorKind::Increment => "increment",
            BehaviorKind::Delay => "delay",
            BehaviorKind::Raise => "raise",
            BehaviorKind::Drop => "drop",
            BehaviorKind::Forward => "forward",
            BehaviorKind::Script => "script",
        }
    }

    /// Parse the lowercase tag.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Whether this behaviour makes sense for `function`.
    pub fn applies_to(self, function: &FnShape) -> bool {
        match self {
            BehaviorKind::Reply
            | BehaviorKind::Echo
            | BehaviorKind::Delay
            | BehaviorKind::Drop
            | BehaviorKind::Script => true,
            BehaviorKind::Increment => {
                !function.oneway && matches!(function.returns, Some(TypeRef::Ref { .. }))
            }
            BehaviorKind::Raise => !function.throws.is_empty(),
            BehaviorKind::Forward => !function.oneway,
        }
    }

    /// A starting config for `function`, seeded from its return / error types.
    pub fn default_config(self, function: &FnShape, schema: &SchemaShape) -> Value {
        let zero_return = || {
            function
                .returns
                .as_ref()
                .map_or(Value::Null, |r| crate::shape::zero_value(r, &schema.types))
        };
        match self {
            BehaviorKind::Reply => serde_json::json!({ "value": zero_return() }),
            BehaviorKind::Echo | BehaviorKind::Drop => serde_json::json!({}),
            BehaviorKind::Increment => serde_json::json!({
                "base": zero_return(),
                "path": first_numeric_path(function, schema).unwrap_or_default(),
            }),
            BehaviorKind::Delay => serde_json::json!({ "ms": 400, "value": zero_return() }),
            BehaviorKind::Raise => {
                let first = function.throws.first();
                let ordinal = first.map_or(0, |t| t.ordinal);
                let mut data = Map::new();
                if let Some(err) = schema
                    .errors
                    .iter()
                    .find(|e| Some(e.ordinal) == first.map(|t| t.ordinal))
                {
                    for f in &err.fields {
                        data.insert(
                            f.name.clone(),
                            crate::shape::zero_value(&f.ty, &schema.types),
                        );
                    }
                }
                serde_json::json!({ "ordinal": ordinal, "data": Value::Object(data) })
            }
            BehaviorKind::Forward => {
                serde_json::json!({ "viaConnectionId": "", "targetFn": function.name })
            }
            BehaviorKind::Script => serde_json::json!({ "source": DEFAULT_SCRIPT }),
        }
    }

    /// Build the runnable behaviour from a (possibly partial) config.
    pub fn make(
        self,
        config: &Value,
        function: &FnShape,
        _schema: &SchemaShape,
    ) -> Box<dyn Behavior> {
        let get = |key: &str| config.get(key).cloned().unwrap_or(Value::Null);
        match self {
            BehaviorKind::Reply => Box::new(ReplyWith {
                value: get("value"),
            }),
            BehaviorKind::Echo => Box::new(Echo),
            BehaviorKind::Increment => Box::new(Increment {
                base: match get("base") {
                    Value::Null => Value::Object(Map::new()),
                    other => other,
                },
                path: get("path").as_str().unwrap_or_default().to_string(),
                current: None,
            }),
            BehaviorKind::Delay => Box::new(Delay {
                ms: get("ms").as_f64().unwrap_or(0.0),
                value: get("value"),
            }),
            BehaviorKind::Raise => Box::new(Raise {
                ordinal: get("ordinal").as_u64().unwrap_or(0) as u16,
                data: get("data"),
            }),
            BehaviorKind::Drop => Box::new(Drop),
            BehaviorKind::Forward => Box::new(Forward {
                via: get("viaConnectionId")
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                target_fn: get("targetFn")
                    .as_str()
                    .unwrap_or(&function.name)
                    .to_string(),
            }),
            BehaviorKind::Script => script::make(get("source").as_str().unwrap_or(DEFAULT_SCRIPT)),
        }
    }
}

/// The script a fresh `Script` behaviour starts with — an echo.
pub const DEFAULT_SCRIPT: &str = "\
// `params` is the decoded request. `state` is a map that persists between
// calls. The last expression is the reply; `throw` raises an error.
params";

/// The behaviour a freshly-added server function starts on.
pub fn default_kind_for(function: &FnShape) -> BehaviorKind {
    if function.oneway {
        BehaviorKind::Drop
    } else {
        BehaviorKind::Reply
    }
}

// ── helpers ─────────────────────────────────────────────────────────────

/// The first numeric-primitive field path of a struct return, for `increment`.
fn first_numeric_path(function: &FnShape, schema: &SchemaShape) -> Option<String> {
    let TypeRef::Ref { name } = function.returns.as_ref()? else {
        return None;
    };
    let def = schema.types.iter().find(|t| t.name() == name)?;
    let TypeDef::Struct { fields, .. } = def else {
        return None;
    };
    fields
        .iter()
        .find(|f| matches!(&f.ty, TypeRef::Prim { name } if is_numeric_prim(name)))
        .map(|f| f.name.clone())
}

/// `serde_json` keeps integers exact; keep whole increments looking like ints.
fn number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9.007e15 {
        Value::from(n as i64)
    } else {
        Value::from(n)
    }
}

fn get_at<'a>(obj: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(obj, |o, k| o.get(k))
}

fn set_at(obj: &mut Value, path: &str, value: Value) {
    match path.split_once('.') {
        None => {
            if let Value::Object(m) = obj {
                m.insert(path.to_string(), value);
            }
        }
        Some((head, rest)) => {
            if !obj.is_object() {
                *obj = Value::Object(Map::new());
            }
            let child = obj
                .as_object_mut()
                .unwrap()
                .entry(head.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            set_at(child, rest, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shape::{ArgShape, FieldShape, Framing, ProtocolShape};
    use serde_json::json;

    fn schema_with(
        functions: Vec<FnShape>,
        types: Vec<TypeDef>,
        errors: Vec<crate::shape::ErrorShape>,
    ) -> (SchemaShape, ProtocolShape) {
        let proto = ProtocolShape {
            name: "P".into(),
            framing: Framing::Datagram,
            functions,
        };
        let schema = SchemaShape {
            namespace: "p".into(),
            ir_hash: "0x0".into(),
            protocols: vec![proto.clone()],
            errors,
            types,
        };
        (schema, proto)
    }

    fn fn_send() -> FnShape {
        FnShape {
            name: "send".into(),
            index: 0,
            oneway: false,
            args: vec![ArgShape {
                name: "text".into(),
                ty: TypeRef::Prim {
                    name: "string".into(),
                },
            }],
            returns: Some(TypeRef::Ref {
                name: "Counter".into(),
            }),
            throws: vec![],
        }
    }

    fn counter_type() -> TypeDef {
        TypeDef::Struct {
            name: "Counter".into(),
            fields: vec![
                FieldShape {
                    name: "label".into(),
                    ty: TypeRef::Prim {
                        name: "string".into(),
                    },
                    optional: false,
                },
                FieldShape {
                    name: "n".into(),
                    ty: TypeRef::Prim { name: "u64".into() },
                    optional: false,
                },
            ],
        }
    }

    fn ctx<'a>(proto: &'a ProtocolShape, params: Value) -> BehaviorCtx<'a> {
        BehaviorCtx {
            params,
            function: &proto.functions[0],
            protocol: proto,
        }
    }

    #[test]
    fn default_kind_is_reply_or_drop_by_direction() {
        let mut f = fn_send();
        assert_eq!(default_kind_for(&f), BehaviorKind::Reply);
        f.oneway = true;
        assert_eq!(default_kind_for(&f), BehaviorKind::Drop);
    }

    #[test]
    fn applies_to_gates_the_kinds() {
        let normal = fn_send();
        let mut oneway = fn_send();
        oneway.oneway = true;
        oneway.returns = None;
        let mut prim_return = fn_send();
        prim_return.returns = Some(TypeRef::Prim { name: "u64".into() });
        let mut throwing = fn_send();
        throwing.throws = vec![crate::shape::ThrowShape {
            ordinal: 3,
            name: "Bad".into(),
        }];

        assert!(BehaviorKind::Increment.applies_to(&normal));
        assert!(!BehaviorKind::Increment.applies_to(&prim_return)); // needs a ref return
        assert!(!BehaviorKind::Increment.applies_to(&oneway));
        assert!(!BehaviorKind::Raise.applies_to(&normal));
        assert!(BehaviorKind::Raise.applies_to(&throwing));
        assert!(!BehaviorKind::Forward.applies_to(&oneway));
        assert!(BehaviorKind::Reply.applies_to(&oneway));
    }

    #[test]
    fn increment_default_config_finds_the_first_numeric_field() {
        let (schema, _) = schema_with(vec![fn_send()], vec![counter_type()], vec![]);
        let cfg = BehaviorKind::Increment.default_config(&fn_send(), &schema);
        assert_eq!(cfg["path"], json!("n"));
        assert_eq!(cfg["base"], json!({ "label": "", "n": 0 }));
    }

    #[test]
    fn raise_default_config_seeds_the_first_throw() {
        let err = crate::shape::ErrorShape {
            ordinal: 5,
            name: "TooLong".into(),
            message: "too long".into(),
            fields: vec![FieldShape {
                name: "max".into(),
                ty: TypeRef::Prim { name: "u32".into() },
                optional: false,
            }],
        };
        let mut f = fn_send();
        f.throws = vec![crate::shape::ThrowShape {
            ordinal: 5,
            name: "TooLong".into(),
        }];
        let (schema, _) = schema_with(vec![f.clone()], vec![], vec![err]);

        let cfg = BehaviorKind::Raise.default_config(&f, &schema);
        assert_eq!(cfg["ordinal"], json!(5));
        assert_eq!(cfg["data"], json!({ "max": 0 }));
    }

    #[test]
    fn reply_and_echo_answer_now() {
        let (schema, proto) = schema_with(vec![fn_send()], vec![counter_type()], vec![]);

        let mut reply = BehaviorKind::Reply.make(
            &json!({ "value": { "label": "x", "n": 3 } }),
            &proto.functions[0],
            &schema,
        );
        assert_eq!(
            reply.run(ctx(&proto, Value::Null)),
            BehaviorStep::Now(SimOutcome::Ok(json!({ "label": "x", "n": 3 })))
        );

        let mut echo = BehaviorKind::Echo.make(&json!({}), &proto.functions[0], &schema);
        assert_eq!(
            echo.run(ctx(&proto, json!(["hi"]))),
            BehaviorStep::Now(SimOutcome::Ok(json!(["hi"])))
        );
    }

    #[test]
    fn increment_bumps_the_field_each_call() {
        let (schema, proto) = schema_with(vec![fn_send()], vec![counter_type()], vec![]);
        let cfg = BehaviorKind::Increment.default_config(&proto.functions[0], &schema);
        let mut b = BehaviorKind::Increment.make(&cfg, &proto.functions[0], &schema);

        assert_eq!(
            b.run(ctx(&proto, Value::Null)),
            BehaviorStep::Now(SimOutcome::Ok(json!({ "label": "", "n": 1 })))
        );
        assert_eq!(
            b.run(ctx(&proto, Value::Null)),
            BehaviorStep::Now(SimOutcome::Ok(json!({ "label": "", "n": 2 })))
        );
    }

    #[test]
    fn increment_walks_a_nested_path() {
        let (schema, proto) = schema_with(vec![fn_send()], vec![counter_type()], vec![]);
        let mut b = BehaviorKind::Increment.make(
            &json!({ "base": { "stats": { "hits": 10 } }, "path": "stats.hits" }),
            &proto.functions[0],
            &schema,
        );
        assert_eq!(
            b.run(ctx(&proto, Value::Null)),
            BehaviorStep::Now(SimOutcome::Ok(json!({ "stats": { "hits": 11 } })))
        );
    }

    #[test]
    fn delay_defers_the_reply() {
        let (schema, proto) = schema_with(vec![fn_send()], vec![counter_type()], vec![]);
        let mut b = BehaviorKind::Delay.make(
            &json!({ "ms": 250, "value": { "n": 1 } }),
            &proto.functions[0],
            &schema,
        );
        assert_eq!(
            b.run(ctx(&proto, Value::Null)),
            BehaviorStep::After {
                delay_ms: 250.0,
                outcome: SimOutcome::Ok(json!({ "n": 1 }))
            }
        );
    }

    #[test]
    fn raise_errors_and_drop_hangs() {
        let (schema, proto) = schema_with(vec![fn_send()], vec![], vec![]);
        let mut raise = BehaviorKind::Raise.make(
            &json!({ "ordinal": 4, "data": { "why": "no" } }),
            &proto.functions[0],
            &schema,
        );
        assert_eq!(
            raise.run(ctx(&proto, Value::Null)),
            BehaviorStep::Now(SimOutcome::Err {
                ordinal: 4,
                data: json!({ "why": "no" })
            })
        );

        let mut drop = BehaviorKind::Drop.make(&json!({}), &proto.functions[0], &schema);
        assert_eq!(drop.run(ctx(&proto, Value::Null)), BehaviorStep::Hang);
    }

    #[test]
    fn forward_carries_the_connection_target_and_params() {
        let (schema, proto) = schema_with(vec![fn_send()], vec![], vec![]);

        let mut no_conn = BehaviorKind::Forward.make(
            &json!({ "viaConnectionId": "", "targetFn": "send" }),
            &proto.functions[0],
            &schema,
        );
        assert!(matches!(
            no_conn.run(ctx(&proto, Value::Null)),
            BehaviorStep::Now(SimOutcome::Err { ordinal: 0, .. })
        ));

        let mut fwd = BehaviorKind::Forward.make(
            &json!({ "viaConnectionId": "c2", "targetFn": "" }),
            &proto.functions[0],
            &schema,
        );
        assert_eq!(
            fwd.run(ctx(&proto, json!(["x"]))),
            BehaviorStep::Forward {
                via: "c2".into(),
                target_fn: "send".into(), // fell back to the current function name
                params: json!(["x"])
            }
        );
    }

    #[test]
    fn kind_round_trips_through_lowercase_json() {
        for k in BehaviorKind::ALL {
            let s = serde_json::to_string(&k).unwrap();
            assert_eq!(serde_json::from_str::<BehaviorKind>(&s).unwrap(), k);
            assert_eq!(BehaviorKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(
            serde_json::to_string(&BehaviorKind::Script).unwrap(),
            "\"script\""
        );
        assert_eq!(BehaviorKind::ALL.len(), 8);
    }

    #[test]
    fn script_default_config_is_a_source_string() {
        let (schema, proto) = schema_with(vec![fn_send()], vec![counter_type()], vec![]);
        let cfg = BehaviorKind::Script.default_config(&proto.functions[0], &schema);
        assert!(cfg["source"].as_str().unwrap().contains("params"));
        assert!(BehaviorKind::Script.applies_to(&fn_send()));
    }

    #[cfg(feature = "script")]
    mod scripted {
        use super::super::*;
        use super::{ctx, fn_send, schema_with};
        use serde_json::json;

        fn run_script(source: &str, params: Value) -> BehaviorStep {
            let (schema, proto) = schema_with(vec![fn_send()], vec![], vec![]);
            let mut b = BehaviorKind::Script.make(
                &json!({ "source": source }),
                &proto.functions[0],
                &schema,
            );
            b.run(ctx(&proto, params))
        }

        #[test]
        fn a_script_returning_params_echoes() {
            assert_eq!(
                run_script("params", json!([1, 2, 3])),
                BehaviorStep::Now(SimOutcome::Ok(json!([1, 2, 3])))
            );
        }

        #[test]
        fn a_script_can_compute_a_reply() {
            assert_eq!(
                run_script("#{ doubled: params[0] * 2 }", json!([21])),
                BehaviorStep::Now(SimOutcome::Ok(json!({ "doubled": 42 })))
            );
        }

        #[test]
        fn state_persists_between_calls() {
            let (schema, proto) = schema_with(vec![fn_send()], vec![], vec![]);
            let mut b = BehaviorKind::Script.make(
                &json!({ "source": "state.n = (state.n ?? 0) + 1; state.n" }),
                &proto.functions[0],
                &schema,
            );
            assert_eq!(
                b.run(ctx(&proto, Value::Null)),
                BehaviorStep::Now(SimOutcome::Ok(json!(1)))
            );
            assert_eq!(
                b.run(ctx(&proto, Value::Null)),
                BehaviorStep::Now(SimOutcome::Ok(json!(2)))
            );
            assert_eq!(
                b.run(ctx(&proto, Value::Null)),
                BehaviorStep::Now(SimOutcome::Ok(json!(3)))
            );
        }

        #[test]
        fn a_thrown_value_becomes_an_error() {
            match run_script(r#"throw "boom""#, Value::Null) {
                BehaviorStep::Now(SimOutcome::Err { ordinal: 0, data }) => {
                    assert!(data["error"].as_str().unwrap().contains("boom"), "{data}");
                }
                other => panic!("expected an error, got {other:?}"),
            }
        }

        #[test]
        fn a_script_that_does_not_compile_errors_at_run() {
            match run_script("this is ( not rhai", Value::Null) {
                BehaviorStep::Now(SimOutcome::Err { data, .. }) => {
                    assert!(
                        data["error"].as_str().unwrap().contains("compile"),
                        "{data}"
                    );
                }
                other => panic!("expected a compile error, got {other:?}"),
            }
        }

        #[test]
        fn an_infinite_loop_is_stopped_not_hung() {
            match run_script("let i = 0; while true { i += 1; } i", Value::Null) {
                BehaviorStep::Now(SimOutcome::Err { data, .. }) => {
                    let msg = data["error"].as_str().unwrap().to_lowercase();
                    assert!(msg.contains("operation") || msg.contains("limit"), "{data}");
                }
                other => panic!("expected a limit error, got {other:?}"),
            }
        }

        #[test]
        fn a_one_way_function_still_sends_nothing() {
            let mut f = fn_send();
            f.oneway = true;
            f.returns = None;
            let (schema, proto) = schema_with(vec![f], vec![], vec![]);
            let mut b = BehaviorKind::Script.make(
                &json!({ "source": "params" }),
                &proto.functions[0],
                &schema,
            );
            assert_eq!(
                b.run(ctx(&proto, json!(["x"]))),
                BehaviorStep::Now(SimOutcome::None)
            );
        }
    }
}
