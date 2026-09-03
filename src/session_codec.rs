//! A [`Session`] ⇄ URL-safe string. `shape` is dropped (it's recomputed from the
//! open schemas); everything else — nodes, instances, behaviours, connections,
//! faults, seed, clock mode — is JSON wrapped in a `{ v, session }` envelope,
//! base64url-encoded. The string goes in the URL fragment (`#s=…`) so a topology
//! is shareable by link.
//!
//! Ported from the playground's `session-codec.ts`. Slightly stricter on a
//! malformed link (a wrong-typed scalar is rejected rather than coerced), but
//! the same in the cases that matter: a well-formed link round-trips, garbage
//! returns `None`.

use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::{alphabet, Engine};
use serde::{Deserialize, Serialize};

use crate::model::Session;
use crate::shape::ProjectShape;

const VERSION: u32 = 1;

/// base64url, no padding on encode, indifferent to it on decode (matches the
/// TS `btoa`/`atob` pair with its manual `+/=` fixups).
const B64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

#[derive(Serialize)]
struct EnvelopeRef<'a> {
    v: u32,
    session: &'a Session,
}

#[derive(Deserialize)]
struct EnvelopeOwned {
    v: u32,
    session: Session,
}

/// Encode `session` to the `#s=…` payload.
pub fn encode_session(session: &Session) -> String {
    let env = EnvelopeRef {
        v: VERSION,
        session,
    };
    let json = serde_json::to_vec(&env).expect("a session always serializes");
    B64.encode(json)
}

/// Decode a payload against the current `shape`. `None` if it isn't a session
/// string this build understands. Instances keep their stored `ir_hash`, so a
/// schema that has moved on since the link was made loads them *stale* — the
/// resync flow then applies.
pub fn decode_session(encoded: &str, shape: ProjectShape) -> Option<Session> {
    let bytes = B64.decode(encoded.trim()).ok()?;
    let env: EnvelopeOwned = serde_json::from_slice(&bytes).ok()?;
    if env.v != VERSION {
        return None;
    }
    let mut session = env.session;
    session.shape = shape;
    session.reseed_counters();
    Some(session)
}

/// Read the `s=` value from a URL fragment / query string (`#s=…` or `&s=…`),
/// if present. The payload is pure base64url so `decodeURIComponent` is a
/// no-op, but a `%XX`-escaped fragment is still unescaped for safety.
pub fn session_from_hash(hash: &str) -> Option<String> {
    let start = ["#s=", "&s=", "?s="]
        .iter()
        .find_map(|p| hash.find(p).map(|i| i + p.len()))
        .or_else(|| hash.strip_prefix("s=").map(|_| 2))?;
    let rest = &hash[start..];
    let value = &rest[..rest.find('&').unwrap_or(rest.len())];
    Some(percent_decode(value))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::behavior::BehaviorKind;
    use crate::model::{InstanceSpec, Placement, Role};
    use crate::shape::{
        ArgShape, FieldShape, FnShape, Framing, ProtocolShape, SchemaShape, TypeDef, TypeRef,
    };

    fn chat_shape() -> ProjectShape {
        ProjectShape {
            schemas: vec![SchemaShape {
                namespace: "chat".into(),
                ir_hash: "0xabcd".into(),
                protocols: vec![ProtocolShape {
                    name: "Chat".into(),
                    framing: Framing::Datagram,
                    functions: vec![FnShape {
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
                            name: "Message".into(),
                        }),
                        throws: vec![],
                    }],
                }],
                errors: vec![],
                types: vec![TypeDef::Struct {
                    name: "Message".into(),
                    fields: vec![FieldShape {
                        name: "seq".into(),
                        ty: TypeRef::Prim { name: "u64".into() },
                        optional: false,
                    }],
                }],
            }],
        }
    }

    fn peopled_session() -> Session {
        let mut s = Session::empty(chat_shape());
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
        let c = s.add_connection(&cli, &srv).unwrap();
        s.set_behavior(&srv, "send", BehaviorKind::Echo, None)
            .unwrap();
        s.connections
            .iter_mut()
            .find(|x| x.id == c)
            .unwrap()
            .faults
            .drop_prob = 0.5;
        s.latency_ms = 25.0;
        s.seed = 99;
        s
    }

    #[test]
    fn round_trips_a_populated_session() {
        let original = peopled_session();
        let encoded = encode_session(&original);
        assert!(
            !encoded.contains(['+', '/', '=']),
            "payload is base64url, unpadded"
        );

        let back = decode_session(&encoded, chat_shape()).expect("decodes");
        assert_eq!(back.instances, original.instances);
        assert_eq!(back.connections, original.connections);
        assert_eq!(back.nodes, original.nodes);
        assert_eq!((back.latency_ms, back.seed), (25.0, 99));
        // the shape is re-injected, never carried in the link
        assert_eq!(back.shape, chat_shape());
    }

    #[test]
    fn a_decoded_session_keeps_adding_ids_without_collision() {
        let encoded = encode_session(&peopled_session());
        let mut back = decode_session(&encoded, chat_shape()).unwrap();
        // peopled_session used i1, i2 — the next must be i3
        let next = back.add_instance(
            InstanceSpec {
                schema_ns: "chat".into(),
                protocol: "Chat".into(),
                role: Role::Client,
            },
            Placement::default(),
        );
        assert_eq!(next, "i3");
    }

    #[test]
    fn rejects_garbage_wrong_version_and_non_base64() {
        assert!(decode_session("not base64!!!", chat_shape()).is_none());
        assert!(decode_session("", chat_shape()).is_none());
        assert!(decode_session(&B64.encode(b"{\"not\":\"a session\"}"), chat_shape()).is_none());

        let wrong_v =
            B64.encode(br#"{"v":2,"session":{"nodes":[],"instances":[],"connections":[]}}"#);
        assert!(decode_session(&wrong_v, chat_shape()).is_none());
    }

    #[test]
    fn decodes_a_minimal_session_with_scalar_defaults() {
        let minimal =
            B64.encode(br#"{"v":1,"session":{"nodes":[],"instances":[],"connections":[]}}"#);
        let s = decode_session(&minimal, chat_shape()).expect("minimal link decodes");
        assert_eq!(s.latency_ms, 0.0);
        assert_eq!(s.call_timeout_ms, 3000.0);
        assert_eq!(s.seed, 1);
        assert_eq!(s.clock_mode, crate::model::ClockMode::Real);
    }

    #[test]
    fn session_from_hash_finds_the_payload() {
        assert_eq!(session_from_hash("#s=abc123"), Some("abc123".to_string()));
        assert_eq!(
            session_from_hash("#foo=1&s=payload_-9&t=2"),
            Some("payload_-9".to_string())
        );
        assert_eq!(session_from_hash("s=bare"), Some("bare".to_string()));
        assert_eq!(session_from_hash("#no-session-here"), None);
        assert_eq!(session_from_hash(""), None);
    }

    #[test]
    fn session_from_hash_unescapes_percent_encoding() {
        // `-` is %2D; a fragment that got fully URL-encoded still resolves
        assert_eq!(session_from_hash("#s=ab%2Dcd"), Some("ab-cd".to_string()));
    }

    #[test]
    fn hash_payload_round_trips_end_to_end() {
        let encoded = encode_session(&peopled_session());
        let url_fragment = format!("#s={encoded}&other=stuff");
        let extracted = session_from_hash(&url_fragment).unwrap();
        assert_eq!(extracted, encoded);
        assert!(decode_session(&extracted, chat_shape()).is_some());
    }
}
