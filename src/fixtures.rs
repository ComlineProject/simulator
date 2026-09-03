//! Shared test fixtures. The engine's unit tests all wire up the same tiny
//! `chat` schema (protocol `Chat`, `send(text) -> Message`); this builds it, and
//! the sessions on it, in one place instead of six. Import it as
//! `use crate::fixtures as chat;`.
#![cfg(test)]

use crate::behavior::BehaviorKind;
use crate::engine::Engine;
use crate::model::{InstanceSpec, Placement, Role, Session};
use crate::shape::{
    ArgShape, FieldShape, FnShape, Framing, ProjectShape, ProtocolShape, SchemaShape, TypeDef,
    TypeRef,
};
use serde_json::json;

/// The connection id `add_connection` assigns the first wire.
pub const CONN: &str = "c1";

fn string() -> TypeRef {
    TypeRef::Prim {
        name: "string".into(),
    }
}

/// `send(text: string) -> Message`, or (oneway) `note(text: string)`.
pub fn fn_shape(name: &str, index: u32, oneway: bool) -> FnShape {
    FnShape {
        name: name.into(),
        index,
        oneway,
        args: vec![ArgShape {
            name: "text".into(),
            ty: string(),
        }],
        returns: (!oneway).then(|| TypeRef::Ref {
            name: "Message".into(),
        }),
        throws: vec![],
    }
}

/// `struct Message { body: string, seq: u64 }`.
pub fn message_type() -> TypeDef {
    TypeDef::Struct {
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
    }
}

/// A `chat` project — protocol `Chat` (datagram) with the given functions, the
/// `Message` type, and a given IR hash (tests vary the hash to exercise skew).
pub fn shape_with(functions: Vec<FnShape>, ir_hash: &str) -> ProjectShape {
    ProjectShape {
        schemas: vec![SchemaShape {
            namespace: "chat".into(),
            ir_hash: ir_hash.into(),
            protocols: vec![ProtocolShape {
                name: "Chat".into(),
                framing: Framing::Datagram,
                functions,
            }],
            errors: vec![],
            types: vec![message_type()],
        }],
    }
}

/// The canonical one: just `send(text) -> Message`. The IR hash is a fixed
/// 16-hex-digit placeholder so it round-trips through the handshake decoder.
pub fn shape() -> ProjectShape {
    shape_with(vec![fn_shape("send", 0, false)], "0x9f2b1c7d4e6a8035")
}

pub fn schema() -> SchemaShape {
    shape().schemas.into_iter().next().unwrap()
}

pub fn proto() -> ProtocolShape {
    schema().protocols.into_iter().next().unwrap()
}

/// `chat-1` (server, `send` replies `{body:"HELLO",seq:1}`) and `chat-2`
/// (client) connected as [`CONN`].
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

/// [`session`] already synced onto a fresh engine.
pub fn engine() -> Engine {
    let mut e = Engine::new();
    e.sync(&session());
    e
}
