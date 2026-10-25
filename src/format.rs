//! The simulator's [`WireFormat`]s. The runtime's own JSON / MessagePack formats
//! live behind its `std` feature (which pulls `libloading` and won't build for
//! `wasm32`), so the simulator carries its own — small wrappers over `serde_json`
//! and `rmp-serde`.
//!
//! [`Codec`] is the enum the engine holds so a connection can pick a format; it
//! implements [`WireFormat`] by dispatch. `packed@1` puts no tags on the wire,
//! so an untyped value needs the schema to lay it out: the engine encodes and
//! decodes bodies with [`Codec::encode_body`] / [`Codec::decode_body`].

use comline_runtime::contract::{
    BufMut, RuntimeError, WireFormat, FORMAT_JSON, FORMAT_MSGPACK, FORMAT_PACKED,
};
use comline_runtime::format::Packed;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::packed::{self, Body};
use crate::shape::TypeDef;

/// JSON via `serde_json`, appended straight into the frame buffer.
pub struct Json;

impl WireFormat for Json {
    fn name(&self) -> &'static str {
        FORMAT_JSON
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
        FORMAT_MSGPACK
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
    Packed,
    Json,
    Msgpack,
}

impl Codec {
    /// JSON — what a connection in a session link from before `packed@1`
    /// speaks (its `wireFormat` key is absent).
    pub fn json() -> Self {
        Codec::Json
    }

    /// The short key a session link and the UI use (`packed`, `json`,
    /// `msgpack`), the same spelling [`Codec::parse`] reads. The handshake uses
    /// the versioned [`WireFormat::name`] instead.
    pub fn label(self) -> &'static str {
        match self {
            Codec::Packed => "packed",
            Codec::Json => "json",
            Codec::Msgpack => "msgpack",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "packed" => Some(Codec::Packed),
            "json" => Some(Codec::Json),
            "msgpack" => Some(Codec::Msgpack),
            _ => None,
        }
    }

    /// Encode a call's body, laid out as `body`. `packed@1` needs the schema's
    /// `types` to know the layout; the self-describing formats ignore both.
    pub fn encode_body(
        &self,
        value: &Value,
        body: Body<'_>,
        types: &[TypeDef],
        out: &mut Vec<u8>,
    ) -> Result<(), RuntimeError> {
        match self {
            Codec::Packed => packed::encode(value, body, types, out),
            _ => self.encode(value, out),
        }
    }

    /// Decode a call's body, laid out as `body`, into the simulator's JSON
    /// shape. See [`Codec::encode_body`].
    pub fn decode_body(
        &self,
        bytes: &[u8],
        body: Body<'_>,
        types: &[TypeDef],
    ) -> Result<Value, RuntimeError> {
        match self {
            Codec::Packed => packed::decode(bytes, body, types),
            _ => self.decode::<Value>(bytes),
        }
    }
}

impl WireFormat for Codec {
    fn name(&self) -> &'static str {
        match self {
            Codec::Packed => FORMAT_PACKED,
            Codec::Json => FORMAT_JSON,
            Codec::Msgpack => FORMAT_MSGPACK,
        }
    }

    fn encode<T: Serialize + ?Sized>(
        &self,
        value: &T,
        out: &mut dyn BufMut,
    ) -> Result<(), RuntimeError> {
        match self {
            Codec::Packed => Packed.encode(value, out),
            Codec::Json => Json.encode(value, out),
            Codec::Msgpack => MsgPack.encode(value, out),
        }
    }

    fn decode<'de, T: Deserialize<'de>>(&self, bytes: &'de [u8]) -> Result<T, RuntimeError> {
        match self {
            Codec::Packed => Packed.decode(bytes),
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

    // runtime's `format-json/found-f64-exact-digits`: serde_json's default
    // float parser reads this one ulp off.
    #[test]
    fn an_f64_reads_back_exactly() {
        let x = 4.520618856987477e-279_f64;
        let bytes = b"4.520618856987477e-279";
        assert_eq!(Json.decode::<f64>(bytes).map(f64::to_bits), Ok(x.to_bits()));
        let value: serde_json::Value = Json.decode(bytes).unwrap();
        assert_eq!(value.as_f64().map(f64::to_bits), Some(x.to_bits()));

        let mut buf = Vec::new();
        Json.encode(&x, &mut buf).unwrap();
        assert_eq!(buf, bytes);
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
        for c in [Codec::Packed, Codec::Json, Codec::Msgpack] {
            let s = serde_json::to_string(&c).unwrap();
            assert_eq!(s, format!("\"{}\"", c.label()));
            assert_eq!(serde_json::from_str::<Codec>(&s).unwrap(), c);
            assert_eq!(Codec::parse(c.label()), Some(c));
        }
        assert_eq!(Codec::parse("nope"), None);
    }

    #[test]
    fn packed_is_the_default() {
        assert_eq!(Codec::default(), Codec::Packed);
        assert_eq!(Codec::default().name(), FORMAT_PACKED);
        assert_eq!(Codec::json(), Codec::Json);
    }
}
