//! Read a raw tap [`Frame`] back into something the inspector can show — the same
//! framing / handshake decoders the engine uses, plus a decode of the sub-frame
//! body with the connection's wire format. Ported from `framedecode.ts`.

use comline_runtime::contract::{name_hash, Envelope, Handshake, RequestCall, WireFormat};
use serde::Serialize;
use serde_json::Value;

use crate::format::Codec;
use crate::frame::Frame;
use crate::framing::WireFraming;

/// Everything the inspector renders for one frame.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameDetail {
    /// `"handshake"` | `"request"` | `"response"` | `"unknown"`.
    pub kind: &'static str,
    /// The framing name, or `"undecodable"` when the body would not parse.
    pub framing: String,
    /// request: the function name (resolved from the call address).
    #[serde(rename = "fn", skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// request: the decoded params.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// response: the decoded ok body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<Value>,
    /// response: a raised error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub err: Option<ErrDetail>,
    /// handshake: the fields it carries (names resolved where known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handshake: Option<HandshakeDetail>,
}

#[derive(Debug, Serialize)]
pub struct ErrDetail {
    pub ordinal: u16,
    pub body: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandshakeDetail {
    pub ir_hash: String,
    pub wire_format: String,
    pub framing: String,
    pub caps: u32,
}

/// What the decoder needs from the connection to resolve a frame.
pub struct DecodeCtx<'a> {
    pub client_name: &'a str,
    pub server_name: &'a str,
    pub framing: WireFraming,
    pub codec: Codec,
    /// Function names in protocol order — resolves a datagram request's call id.
    pub fn_names: &'a [&'a str],
}

impl FrameDetail {
    fn bare(kind: &'static str, framing: String) -> Self {
        Self {
            kind,
            framing,
            function: None,
            request_id: None,
            params: None,
            ok: None,
            err: None,
            handshake: None,
        }
    }
}

/// FNV-1a hash → readable name, for the handshake's wire-format / framing.
fn name_of(hash: u64) -> String {
    for name in [
        "json",
        "msgpack",
        WireFraming::Datagram.name(),
        WireFraming::Jsonrpc.name(),
    ] {
        if name_hash(name) == hash {
            return name.to_string();
        }
    }
    format!("{hash:#x}")
}

/// `(value, decoded_ok)`. An empty slice is `null`; a slice the codec will not
/// decode is a `"<N undecodable bytes>"` placeholder with `decoded_ok = false`.
///
/// A JSON-RPC frame carries its params / body as raw JSON regardless of the
/// wire format, so those are always JSON-decoded; a datagram frame's body is
/// whatever the connection's codec produced.
fn body_of(bytes: &[u8], codec: Codec, always_json: bool) -> (Value, bool) {
    if bytes.is_empty() {
        return (Value::Null, true);
    }
    let decoded = if always_json {
        serde_json::from_slice::<Value>(bytes).ok()
    } else {
        codec.decode::<Value>(bytes).ok()
    };
    match decoded {
        Some(v) => (v, true),
        None => (
            Value::String(format!("<{} undecodable bytes>", bytes.len())),
            false,
        ),
    }
}

/// Decode `frame` against `ctx`.
pub fn describe_frame(frame: &Frame, ctx: &DecodeCtx<'_>) -> FrameDetail {
    let bytes = &frame.bytes;

    // Handshake — unambiguous: fixed length + `CO` magic.
    if bytes.len() == 31 {
        if let Some(hs) = Handshake::decode(bytes) {
            let mut d = FrameDetail::bare("handshake", ctx.framing.name().to_string());
            d.handshake = Some(HandshakeDetail {
                ir_hash: format!("{:#018x}", hs.ir_hash),
                wire_format: name_of(hs.wire_format),
                framing: name_of(hs.framing),
                caps: hs.capabilities,
            });
            return d;
        }
    }

    let framing = ctx.framing;
    let name = framing.name().to_string();
    let jsonrpc = framing.is_named();

    if frame.from == ctx.client_name {
        let Some(req) = framing.decode_request(bytes) else {
            return FrameDetail::bare("unknown", name);
        };
        let function = match req.call {
            RequestCall::Id(id) => ctx
                .fn_names
                .get(id as usize)
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("#{id}")),
            RequestCall::Name(n) => n.to_string(),
        };
        let (params, ok) = body_of(req.params, ctx.codec, jsonrpc);
        let mut d = FrameDetail::bare("request", if ok { name } else { "undecodable".into() });
        d.function = Some(function);
        d.request_id = Some(req.request_id.to_string());
        d.params = Some(params);
        return d;
    }

    let Some((request_id, envelope)) = framing.decode_response(bytes) else {
        return FrameDetail::bare("unknown", name);
    };
    let mut d = FrameDetail::bare("response", name);
    d.request_id = Some(request_id.to_string());
    match envelope {
        Envelope::Ok(payload) => {
            let (value, ok) = body_of(payload, ctx.codec, jsonrpc);
            d.ok = Some(value);
            if !ok {
                d.framing = "undecodable".into();
            }
        }
        Envelope::Err { id, body } => {
            let (value, ok) = body_of(body, ctx.codec, jsonrpc);
            d.err = Some(ErrDetail {
                ordinal: id,
                body: value,
            });
            if !ok {
                d.framing = "undecodable".into();
            }
        }
    }
    d
}

/// `de ad be ef` — space-separated hex, for the raw-bytes view.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::fixtures as chat;
    use serde_json::json;

    fn detail(engine: &Engine, conn: &str, seq: u32) -> FrameDetail {
        let info = engine.wire_info(conn).unwrap();
        let frame = engine
            .tap(conn)
            .unwrap()
            .frames
            .iter()
            .find(|f| f.seq == seq)
            .unwrap();
        describe_frame(
            frame,
            &DecodeCtx {
                client_name: info.client_name,
                server_name: info.server_name,
                framing: info.framing,
                codec: info.codec,
                fn_names: &info.fn_names,
            },
        )
    }

    #[test]
    fn decodes_the_handshake_request_and_response_of_a_round_trip() {
        let mut e = chat::engine();
        e.call(chat::CONN, "send", &json!(["hi"])).unwrap();
        e.run();

        // frames: 1,2 handshake · 3 request · 4 response
        let hs = detail(&e, chat::CONN, 1);
        assert_eq!(hs.kind, "handshake");
        let h = hs.handshake.unwrap();
        assert_eq!(h.wire_format, "json");
        assert_eq!(h.framing, WireFraming::Datagram.name());
        assert_eq!(h.ir_hash, "0x9f2b1c7d4e6a8035"); // the `chat` fixture's placeholder hash

        let req = detail(&e, chat::CONN, 3);
        assert_eq!(req.kind, "request");
        assert_eq!(req.function.as_deref(), Some("send"));
        assert_eq!(req.params, Some(json!(["hi"])));
        assert_eq!(req.request_id.as_deref(), Some("1"));

        let res = detail(&e, chat::CONN, 4);
        assert_eq!(res.kind, "response");
        assert_eq!(res.request_id.as_deref(), Some("1"));
        assert_eq!(res.ok, Some(json!({ "body": "HELLO", "seq": 1 })));
        assert!(res.err.is_none());
    }

    #[test]
    fn a_corrupted_response_body_reads_as_undecodable() {
        let mut session = chat::session();
        session.connections[0].faults.corrupt_prob = 1.0;
        session.connections[0].faults.apply_to = crate::faults::FaultDir::Responses;
        session.call_timeout_ms = 0.0;
        let mut e = Engine::new();
        e.sync(&session);
        e.call(chat::CONN, "send", &json!(["hi"])).unwrap();
        e.run();

        let res = detail(&e, chat::CONN, 4);
        assert_eq!(res.kind, "response");
        assert_eq!(res.framing, "undecodable");
    }

    #[test]
    fn a_raised_error_decodes_its_ordinal_and_body() {
        let mut session = chat::session();
        session
            .set_behavior(
                &session
                    .instances
                    .iter()
                    .find(|i| i.role == crate::model::Role::Server)
                    .unwrap()
                    .id
                    .clone(),
                "send",
                crate::behavior::BehaviorKind::Raise,
                Some(json!({ "ordinal": 4, "data": { "why": "no" } })),
            )
            .unwrap();
        let mut e = Engine::new();
        e.sync(&session);
        e.call(chat::CONN, "send", &json!(["hi"])).unwrap();
        e.run();

        let res = detail(&e, chat::CONN, 4);
        let err = res.err.unwrap();
        assert_eq!(err.ordinal, 4);
        assert_eq!(err.body, json!({ "why": "no" }));
    }

    #[test]
    fn to_hex_is_space_separated_lowercase() {
        assert_eq!(to_hex(&[0xde, 0xad, 0x00, 0x0f]), "de ad 00 0f");
    }
}
