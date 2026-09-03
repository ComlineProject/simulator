//! The shape of a compiled project as the simulator sees it — a 1:1 mirror of
//! the playground editor wasm's `describe_project` output. Everything here is
//! derived from the frozen IR by that function; the simulator only *reads* this
//! projection, it never re-implements the compiler.
//!
//! Ported from the playground's `shape.ts`. The two wasm modules can't share
//! Rust values, so the schema crosses between them as this JSON.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ProjectShape {
    pub schemas: Vec<SchemaShape>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct SchemaShape {
    /// `::`-joined, e.g. `chat` or `wire::frame`.
    pub namespace: String,
    /// `0x`-prefixed 16 hex digits — the value the generators emit as `IR_HASH`,
    /// so a sim connection's handshake mirrors a real one.
    pub ir_hash: String,
    pub protocols: Vec<ProtocolShape>,
    pub errors: Vec<ErrorShape>,
    /// Every struct / enum in the schema, for the call form's nested inputs.
    pub types: Vec<TypeDef>,
}

/// How a protocol frames its calls on the wire.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Framing {
    Datagram,
    Jsonrpc,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ProtocolShape {
    pub name: String,
    pub framing: Framing,
    pub functions: Vec<FnShape>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct FnShape {
    pub name: String,
    /// 0-based position in the protocol — the `Call` id resolution matches.
    pub index: u32,
    /// No return at all — a fire-and-forget `notify`.
    pub oneway: bool,
    pub args: Vec<ArgShape>,
    pub returns: Option<TypeRef>,
    pub throws: Vec<ThrowShape>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ArgShape {
    pub name: String,
    pub ty: TypeRef,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ThrowShape {
    pub ordinal: u16,
    /// The error's name, or `"<unresolved>"` for a bare `throws` slot.
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ErrorShape {
    pub ordinal: u16,
    pub name: String,
    pub message: String,
    pub fields: Vec<FieldShape>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct FieldShape {
    pub name: String,
    pub ty: TypeRef,
    pub optional: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TypeDef {
    Struct {
        name: String,
        fields: Vec<FieldShape>,
    },
    Enum {
        name: String,
        variants: Vec<String>,
    },
}

impl TypeDef {
    pub fn name(&self) -> &str {
        match self {
            TypeDef::Struct { name, .. } | TypeDef::Enum { name, .. } => name,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TypeRef {
    Prim { name: String },
    Ref { name: String },
    Array { of: Box<TypeRef> },
    Unit,
    Union { of: Vec<TypeRef> },
}

// ── helpers ─────────────────────────────────────────────────────────────

/// The protocol named `name` in schema `ns`, with its schema.
pub fn find_protocol<'a>(
    shape: &'a ProjectShape,
    ns: &str,
    name: &str,
) -> Option<(&'a SchemaShape, &'a ProtocolShape)> {
    let schema = shape.schemas.iter().find(|s| s.namespace == ns)?;
    let protocol = schema.protocols.iter().find(|p| p.name == name)?;
    Some((schema, protocol))
}

/// A short label for a `TypeRef` (`u64`, `Message[]`, `A | B`, `()`).
pub fn type_label(ty: &TypeRef) -> String {
    match ty {
        TypeRef::Unit => "()".to_string(),
        TypeRef::Prim { name } | TypeRef::Ref { name } => name.clone(),
        TypeRef::Array { of } => format!("{}[]", type_label(of)),
        TypeRef::Union { of } => of.iter().map(type_label).collect::<Vec<_>>().join(" | "),
    }
}

/// A zero value for `ty`, for seeding "reply with value" and the call form.
/// `ref` types recurse through `types`; unknown / recursive → `null`.
pub fn zero_value(ty: &TypeRef, types: &[TypeDef]) -> Value {
    zero_value_seen(ty, types, &mut Vec::new())
}

fn zero_value_seen(ty: &TypeRef, types: &[TypeDef], seen: &mut Vec<String>) -> Value {
    match ty {
        TypeRef::Unit => Value::Null,
        TypeRef::Array { .. } => Value::Array(Vec::new()),
        TypeRef::Union { of } => of
            .first()
            .map_or(Value::Null, |t| zero_value_seen(t, types, seen)),
        TypeRef::Prim { name } => zero_prim(name),
        TypeRef::Ref { name } => {
            if seen.iter().any(|s| s == name) {
                return Value::Null;
            }
            let Some(def) = types.iter().find(|t| t.name() == name) else {
                return Value::Null;
            };
            match def {
                TypeDef::Enum { variants, .. } => variants
                    .first()
                    .map_or(Value::Null, |v| Value::String(v.clone())),
                TypeDef::Struct { fields, .. } => {
                    seen.push(name.clone());
                    let mut obj = Map::new();
                    for f in fields {
                        obj.insert(f.name.clone(), zero_value_seen(&f.ty, types, seen));
                    }
                    seen.pop();
                    Value::Object(obj)
                }
            }
        }
    }
}

fn zero_prim(name: &str) -> Value {
    match name {
        "bool" => Value::Bool(false),
        "string" | "str" => Value::String(String::new()),
        n if is_numeric_prim(n) => Value::from(0),
        _ => Value::Null,
    }
}

/// A Comline integer / float primitive — `u8`..`u128`, `s8`..`s128`, `f32` /
/// `f64` / `float`. Mirrors the regexes in `shape.ts` / `behavior.ts`.
pub fn is_numeric_prim(name: &str) -> bool {
    matches!(
        name,
        "u8" | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "s8"
            | "s16"
            | "s32"
            | "s64"
            | "s128"
            | "f32"
            | "f64"
            | "float"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A hand-authored mirror of `describe_project`'s output for the chat schema
    /// used across the playground tests:
    ///
    /// ```text
    /// struct Message { body: string, seq: u64 }
    /// protocol Chat { function send(text: string) -> Message; }
    /// ```
    ///
    /// Pinned here so `shape.rs` deserialization stays honest against the
    /// projection; regenerated for real once the playground is rewired.
    const CHAT_DESCRIBE_JSON: &str = include_str!("../tests/fixtures/chat.describe.json");

    #[test]
    fn deserializes_the_describe_project_projection() {
        let shape: ProjectShape = serde_json::from_str(CHAT_DESCRIBE_JSON).unwrap();
        let (schema, proto) = find_protocol(&shape, "chat", "Chat").unwrap();

        assert!(schema.ir_hash.starts_with("0x"));
        assert_eq!(proto.framing, Framing::Datagram);
        assert_eq!(proto.functions.len(), 1);

        let send = &proto.functions[0];
        assert_eq!(send.name, "send");
        assert_eq!(send.index, 0);
        assert!(!send.oneway);
        assert!(send.throws.is_empty());
        assert_eq!(send.args[0].name, "text");
        assert_eq!(
            send.args[0].ty,
            TypeRef::Prim {
                name: "string".into()
            }
        );
        assert_eq!(
            send.returns,
            Some(TypeRef::Ref {
                name: "Message".into()
            })
        );

        assert_eq!(schema.types.len(), 1);
        assert_eq!(schema.types[0].name(), "Message");
    }

    #[test]
    fn re_serializes_to_the_same_json() {
        let shape: ProjectShape = serde_json::from_str(CHAT_DESCRIBE_JSON).unwrap();
        let round: ProjectShape =
            serde_json::from_value(serde_json::to_value(&shape).unwrap()).unwrap();
        assert_eq!(shape, round);
    }

    #[test]
    fn type_ref_tag_matches_the_projection_wire_form() {
        let v: TypeRef = serde_json::from_value(
            json!({ "kind": "array", "of": { "kind": "ref", "name": "Message" } }),
        )
        .unwrap();
        assert_eq!(
            v,
            TypeRef::Array {
                of: Box::new(TypeRef::Ref {
                    name: "Message".into()
                })
            }
        );
        assert_eq!(
            serde_json::from_value::<TypeRef>(json!({ "kind": "unit" })).unwrap(),
            TypeRef::Unit
        );
    }

    #[test]
    fn zero_value_walks_structs_enums_and_prims() {
        let types = vec![
            TypeDef::Struct {
                name: "Message".into(),
                fields: vec![
                    FieldShape {
                        name: "body".into(),
                        ty: TypeRef::Prim {
                            name: "string".into(),
                        },
                        optional: false,
                    },
                    FieldShape {
                        name: "seq".into(),
                        ty: TypeRef::Prim { name: "u64".into() },
                        optional: false,
                    },
                ],
            },
            TypeDef::Enum {
                name: "Status".into(),
                variants: vec!["Idle".into(), "Busy".into()],
            },
        ];

        assert_eq!(
            zero_value(
                &TypeRef::Ref {
                    name: "Message".into()
                },
                &types
            ),
            json!({ "body": "", "seq": 0 })
        );
        assert_eq!(
            zero_value(
                &TypeRef::Ref {
                    name: "Status".into()
                },
                &types
            ),
            json!("Idle")
        );
        assert_eq!(
            zero_value(
                &TypeRef::Array {
                    of: Box::new(TypeRef::Unit)
                },
                &types
            ),
            json!([])
        );
        assert_eq!(
            zero_value(
                &TypeRef::Prim {
                    name: "bool".into()
                },
                &types
            ),
            json!(false)
        );
        assert_eq!(
            zero_value(
                &TypeRef::Ref {
                    name: "Missing".into()
                },
                &types
            ),
            Value::Null
        );
    }

    #[test]
    fn zero_value_stops_on_a_recursive_struct() {
        let types = vec![TypeDef::Struct {
            name: "Node".into(),
            fields: vec![FieldShape {
                name: "next".into(),
                ty: TypeRef::Ref {
                    name: "Node".into(),
                },
                optional: true,
            }],
        }];
        assert_eq!(
            zero_value(
                &TypeRef::Ref {
                    name: "Node".into()
                },
                &types
            ),
            json!({ "next": null })
        );
    }

    #[test]
    fn type_label_reads_back_the_signature_syntax() {
        assert_eq!(type_label(&TypeRef::Unit), "()");
        assert_eq!(
            type_label(&TypeRef::Array {
                of: Box::new(TypeRef::Ref {
                    name: "Message".into()
                })
            }),
            "Message[]"
        );
        assert_eq!(
            type_label(&TypeRef::Union {
                of: vec![
                    TypeRef::Prim { name: "A".into() },
                    TypeRef::Prim { name: "B".into() },
                ]
            }),
            "A | B"
        );
    }
}
