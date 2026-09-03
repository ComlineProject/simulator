//! The framings a connection can speak. `DatagramFraming` comes from
//! `comline-runtime`; JSON-RPC 2.0 is vendored here — it is the same code the
//! runtime ships, but the runtime keeps it behind `std` (module organisation,
//! not a real `std` need), and `std` won't build for `wasm32`.
//!
//! [`WireFraming`] is the enum the engine holds. It is *not* an impl of the
//! runtime's `Framing` trait: a name-oriented framing needs the function name,
//! and that comes from a runtime-parsed `ProtocolShape` (not a `&'static str`
//! the way `contract::Call` wants it), so the surface takes `(call_id, name)`.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use comline_runtime::contract::{
    BufMut, Call, DatagramFraming, Envelope, Framing, Request, RequestCall, RuntimeError,
    WireFormat,
};

/// How a connection frames its calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireFraming {
    Datagram,
    Jsonrpc,
}

impl WireFraming {
    pub fn name(self) -> &'static str {
        match self {
            WireFraming::Datagram => comline_runtime::contract::FRAMING_DATAGRAM,
            WireFraming::Jsonrpc => "jsonrpc-2.0",
        }
    }

    /// `true` if this framing carries the call by name (JSON-RPC) rather than by
    /// the append-only ordinal.
    pub fn is_named(self) -> bool {
        matches!(self, WireFraming::Jsonrpc)
    }

    pub fn encode_request<W, P>(
        self,
        call_id: u16,
        call_name: &str,
        request_id: u64,
        params: &P,
        fmt: &W,
        out: &mut Vec<u8>,
    ) -> Result<(), RuntimeError>
    where
        W: WireFormat,
        P: Serialize + ?Sized,
    {
        match self {
            WireFraming::Datagram => {
                DatagramFraming.encode_request(Call::new(call_id, ""), request_id, params, fmt, out)
            }
            WireFraming::Jsonrpc => jsonrpc_encode_request(call_name, request_id, params, fmt, out),
        }
    }

    pub fn decode_request<'f>(self, frame: &'f [u8]) -> Option<Request<'f>> {
        match self {
            WireFraming::Datagram => DatagramFraming.decode_request(frame),
            WireFraming::Jsonrpc => jsonrpc_decode_request(frame),
        }
    }

    pub fn encode_response_ok(self, request_id: u64, payload: &[u8], out: &mut Vec<u8>) {
        match self {
            WireFraming::Datagram => DatagramFraming.encode_response_ok(request_id, payload, out),
            WireFraming::Jsonrpc => jsonrpc_encode_response_ok(request_id, payload, out),
        }
    }

    pub fn encode_response_err(self, request_id: u64, id: u16, body: &[u8], out: &mut Vec<u8>) {
        match self {
            WireFraming::Datagram => DatagramFraming.encode_response_err(request_id, id, body, out),
            WireFraming::Jsonrpc => jsonrpc_encode_response_err(request_id, id, body, out),
        }
    }

    pub fn decode_response<'f>(self, frame: &'f [u8]) -> Option<(u64, Envelope<'f>)> {
        match self {
            WireFraming::Datagram => DatagramFraming.decode_response(frame),
            WireFraming::Jsonrpc => jsonrpc_decode_response(frame),
        }
    }
}

// ── JSON-RPC 2.0 (verbatim from comline-runtime's `framing` module) ────────

fn u64_bytes(n: u64) -> Vec<u8> {
    n.to_string().into_bytes()
}

fn jsonrpc_encode_request<W, P>(
    method: &str,
    request_id: u64,
    params: &P,
    fmt: &W,
    out: &mut Vec<u8>,
) -> Result<(), RuntimeError>
where
    W: WireFormat,
    P: Serialize + ?Sized,
{
    // method names are generated identifiers — no JSON escaping needed.
    out.put_slice(br#"{"jsonrpc":"2.0","method":""#);
    out.put_slice(method.as_bytes());
    out.put_slice(br#"","params":"#);
    fmt.encode(params, out)?;
    out.put_slice(br#","id":"#);
    out.put_slice(&u64_bytes(request_id));
    out.put_slice(b"}");
    Ok(())
}

fn jsonrpc_decode_request(frame: &[u8]) -> Option<Request<'_>> {
    #[derive(Deserialize)]
    struct ReqIn<'a> {
        method: &'a str,
        #[serde(borrow, default)]
        params: Option<&'a RawValue>,
        #[serde(default)]
        id: Option<u64>,
    }
    let r: ReqIn = serde_json::from_slice(frame).ok()?;
    Some(Request {
        call: RequestCall::Name(r.method),
        request_id: r.id.unwrap_or(0),
        params: r.params.map(|p| p.get().as_bytes()).unwrap_or(b"null"),
    })
}

fn jsonrpc_encode_response_ok(request_id: u64, payload: &[u8], out: &mut Vec<u8>) {
    out.put_slice(br#"{"jsonrpc":"2.0","result":"#);
    out.put_slice(if payload.is_empty() { b"null" } else { payload });
    out.put_slice(br#","id":"#);
    out.put_slice(&u64_bytes(request_id));
    out.put_slice(b"}");
}

fn jsonrpc_encode_response_err(request_id: u64, id: u16, body: &[u8], out: &mut Vec<u8>) {
    out.put_slice(br#"{"jsonrpc":"2.0","error":{"code":"#);
    out.put_slice(&u64_bytes(u64::from(id)));
    out.put_slice(br#","message":"application error","data":"#);
    out.put_slice(if body.is_empty() { b"null" } else { body });
    out.put_slice(br#"},"id":"#);
    out.put_slice(&u64_bytes(request_id));
    out.put_slice(b"}");
}

fn jsonrpc_decode_response(frame: &[u8]) -> Option<(u64, Envelope<'_>)> {
    #[derive(Deserialize)]
    struct ErrIn<'a> {
        code: i64,
        #[serde(borrow, default)]
        data: Option<&'a RawValue>,
    }
    #[derive(Deserialize)]
    struct RespIn<'a> {
        #[serde(borrow, default)]
        result: Option<&'a RawValue>,
        #[serde(borrow, default)]
        error: Option<ErrIn<'a>>,
        id: u64,
    }
    let r: RespIn = serde_json::from_slice(frame).ok()?;
    if let Some(e) = r.error {
        Some((
            r.id,
            Envelope::Err {
                id: e.code as u16,
                body: e.data.map(|d| d.get().as_bytes()).unwrap_or(b"null"),
            },
        ))
    } else {
        Some((
            r.id,
            Envelope::Ok(r.result.map(|p| p.get().as_bytes()).unwrap_or(b"null")),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Json;

    #[test]
    fn jsonrpc_request_round_trips() {
        let f = WireFraming::Jsonrpc;
        let mut frame = Vec::new();
        f.encode_request(0, "greet", 1, &(7u32, "x"), &Json, &mut frame)
            .unwrap();
        assert_eq!(
            std::str::from_utf8(&frame).unwrap(),
            r#"{"jsonrpc":"2.0","method":"greet","params":[7,"x"],"id":1}"#
        );

        let req = f.decode_request(&frame).unwrap();
        assert_eq!(req.call, RequestCall::Name("greet"));
        assert_eq!(req.request_id, 1);
        assert_eq!(req.params, br#"[7,"x"]"#);
    }

    #[test]
    fn jsonrpc_ok_and_err_responses_round_trip() {
        let f = WireFraming::Jsonrpc;

        let mut ok = Vec::new();
        f.encode_response_ok(9, br#"{"body":"hi"}"#, &mut ok);
        assert_eq!(
            f.decode_response(&ok),
            Some((9, Envelope::Ok(br#"{"body":"hi"}"#)))
        );

        let mut err = Vec::new();
        f.encode_response_err(9, 3, br#"{"why":"no"}"#, &mut err);
        assert_eq!(
            f.decode_response(&err),
            Some((
                9,
                Envelope::Err {
                    id: 3,
                    body: br#"{"why":"no"}"#
                }
            ))
        );
    }

    #[test]
    fn datagram_still_works_through_the_enum() {
        let f = WireFraming::Datagram;
        let mut frame = Vec::new();
        f.encode_request(2, "ignored", 5, &("hi",), &Json, &mut frame)
            .unwrap();
        let req = f.decode_request(&frame).unwrap();
        assert_eq!(req.call, RequestCall::Id(2));
        assert_eq!(req.request_id, 5);

        let mut resp = Vec::new();
        f.encode_response_ok(5, b"body", &mut resp);
        assert_eq!(f.decode_response(&resp), Some((5, Envelope::Ok(b"body"))));
    }

    #[test]
    fn the_two_request_frames_differ_as_their_specs_say() {
        let mut dg = Vec::new();
        WireFraming::Datagram
            .encode_request(0, "send", 1, &("hi",), &Json, &mut dg)
            .unwrap();
        let mut rpc = Vec::new();
        WireFraming::Jsonrpc
            .encode_request(0, "send", 1, &("hi",), &Json, &mut rpc)
            .unwrap();

        // datagram: [call_id u16 LE][request_id u64 LE][json params]
        assert_eq!(&dg[..2], &[0, 0]);
        assert!(dg.ends_with(br#"["hi"]"#));
        // json-rpc: a self-describing text frame
        assert!(std::str::from_utf8(&rpc)
            .unwrap()
            .contains(r#""method":"send""#));
    }
}
