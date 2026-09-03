//! The simulator's [`WireFormat`]s. The runtime's own JSON / MessagePack formats
//! live behind its `std` feature (which pulls `libloading` and won't build for
//! `wasm32`), so the simulator carries its own — small wrappers over `serde_json`
//! and `rmp-serde`.
//!
//! [`Codec`] is the enum the engine holds so a connection can pick a format; it
//! implements [`WireFormat`] by dispatch.

use comline_runtime::contract::{BufMut, RuntimeError, WireFormat};
use serde::{Deserialize, Serialize};

/// JSON via `serde_json`, appended straight into the frame buffer.
pub struct Json;

impl WireFormat for Json {
    fn name(&self) -> &'static str {
        "json"
    }

    fn encode<T: Serialize + ?Sized>(
        &self,
        value: &T,
        out: &mut dyn BufMut,
    ) -> Result<(), RuntimeError> {
        let bytes = serde_json::to_vec(value).map_err(|_| RuntimeError::Serialization)?;
        out.put_slice(&bytes);
        Ok(())
    }

    fn decode<'de, T: Deserialize<'de>>(&self, bytes: &'de [u8]) -> Result<T, RuntimeError> {
        serde_json::from_slice(bytes).map_err(|_| RuntimeError::Serialization)
    }
}

/// MessagePack via `rmp-serde` (self-describing, field-name maps so a decoded
/// `Value` round-trips).
pub struct MsgPack;

impl WireFormat for MsgPack {
    fn name(&self) -> &'static str {
        "msgpack"
    }

    fn encode<T: Serialize + ?Sized>(
        &self,
        value: &T,
        out: &mut dyn BufMut,
    ) -> Result<(), RuntimeError> {
        let bytes = rmp_serde::to_vec_named(value).map_err(|_| RuntimeError::Serialization)?;
        out.put_slice(&bytes);
        Ok(())
    }

    fn decode<'de, T: Deserialize<'de>>(&self, bytes: &'de [u8]) -> Result<T, RuntimeError> {
        rmp_serde::from_slice(bytes).map_err(|_| RuntimeError::Serialization)
    }
}

/// The wire format a connection speaks. Serializes as its lowercase name so it
/// round-trips in a session link.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    #[default]
    Json,
    Msgpack,
}

impl Codec {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "json" => Some(Codec::Json),
            "msgpack" => Some(Codec::Msgpack),
            _ => None,
        }
    }
}

impl WireFormat for Codec {
    fn name(&self) -> &'static str {
        match self {
            Codec::Json => "json",
            Codec::Msgpack => "msgpack",
        }
    }

    fn encode<T: Serialize + ?Sized>(
        &self,
        value: &T,
        out: &mut dyn BufMut,
    ) -> Result<(), RuntimeError> {
        match self {
            Codec::Json => Json.encode(value, out),
            Codec::Msgpack => MsgPack.encode(value, out),
        }
    }

    fn decode<'de, T: Deserialize<'de>>(&self, bytes: &'de [u8]) -> Result<T, RuntimeError> {
        match self {
            Codec::Json => Json.decode(bytes),
            Codec::Msgpack => MsgPack.decode(bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip(codec: Codec, value: &serde_json::Value) -> serde_json::Value {
        let mut buf = Vec::new();
        codec.encode(value, &mut buf).unwrap();
        codec.decode::<serde_json::Value>(&buf).unwrap()
    }

    #[test]
    fn both_codecs_round_trip_a_value() {
        let v = json!({ "body": "hi", "seq": 7, "tags": ["a", "b"], "ok": true, "n": null });
        assert_eq!(round_trip(Codec::Json, &v), v);
        assert_eq!(round_trip(Codec::Msgpack, &v), v);
    }

    #[test]
    fn msgpack_is_more_compact_than_json_here() {
        let v = json!({ "body": "hello world", "seq": 1 });
        let mut j = Vec::new();
        let mut m = Vec::new();
        Codec::Json.encode(&v, &mut j).unwrap();
        Codec::Msgpack.encode(&v, &mut m).unwrap();
        assert!(m.len() < j.len(), "json {} vs msgpack {}", j.len(), m.len());
    }

    #[test]
    fn codec_round_trips_through_lowercase_json() {
        for c in [Codec::Json, Codec::Msgpack] {
            let s = serde_json::to_string(&c).unwrap();
            assert_eq!(serde_json::from_str::<Codec>(&s).unwrap(), c);
            assert_eq!(Codec::parse(c.name()), Some(c));
        }
    }
}
