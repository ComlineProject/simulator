//! The framings a connection can speak, all of them `comline-runtime`'s:
//! `DatagramFraming`, `Datagram2Framing` and `JsonRpcFraming`. The simulator
//! frames and parses through them, so its frames are the runtime's, byte for
//! byte.
//!
//! [`WireFraming`] is the enum the engine holds. It is *not* an impl of the
//! runtime's `Framing` trait: a name-oriented framing needs the function name,
//! and that comes from a runtime-parsed `ProtocolShape` (not a `&'static str`
//! the way `contract::Call` wants it). So requests go out through the
//! framings' `encode_request_body`, which takes `(call_id, name)` and a body
//! the codec has already laid out.
//!
//! **Request ids are the simulator's own**, not a runtime `State`'s: it plays
//! both ends of every connection on one simulated clock, so the engine hands
//! out ids itself and tracks what's in flight, timed out or answered. The
//! framings only write the id they're given.

use comline_runtime::contract::{
    ConnectionFraming, Datagram2Framing, DatagramFraming, Envelope, Frame, Framing, ProtocolNumber,
    Request,
};
use comline_runtime::framing::JsonRpcFraming;

/// How a connection frames its calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireFraming {
    Datagram,
    /// `datagram@2`, the default: every frame carries its kind and the
    /// protocol's number on the connection.
    Datagram2(ProtocolNumber),
    /// JSON-RPC 2.0, as a lockstep `Client` speaks it: the simulator's
    /// JSON-RPC protocols are all provided by the server, and the Rust
    /// generator puts such a package on a `Client`, not a `Connection`. So
    /// a one-way call still carries its `id` (`Client::notify`); only on a
    /// `Connection` would it be a notification without one.
    Jsonrpc,
}

impl WireFraming {
    pub fn name(self) -> &'static str {
        match self {
            WireFraming::Datagram => comline_runtime::contract::FRAMING_DATAGRAM,
            WireFraming::Datagram2(_) => comline_runtime::contract::FRAMING_DATAGRAM2,
            WireFraming::Jsonrpc => comline_runtime::contract::FRAMING_JSONRPC,
        }
    }

    /// `true` if this framing carries the call by name (JSON-RPC) rather than by
    /// the append-only ordinal.
    pub fn is_named(self) -> bool {
        matches!(self, WireFraming::Jsonrpc)
    }

    /// Frame a request around `body`, the params already encoded by the
    /// connection's codec. `call_name` is for JSON-RPC; the datagram framings
    /// ignore it. `one_way` is for a framing that can put it on the wire:
    /// `datagram@2` can't, and the simulator's JSON-RPC is a `Client`'s (see
    /// [`WireFraming::Jsonrpc`]), so today no frame depends on it.
    pub fn encode_request(
        self,
        call_id: u16,
        call_name: &str,
        request_id: u64,
        one_way: bool,
        body: &[u8],
        out: &mut Vec<u8>,
    ) {
        match self {
            WireFraming::Datagram => {
                DatagramFraming.encode_request_body(call_id, call_name, request_id, body, out)
            }
            WireFraming::Datagram2(protocol) => ConnectionFraming::encode_request_body(
                &Datagram2Framing,
                protocol,
                call_id,
                call_name,
                request_id,
                one_way,
                body,
                out,
            ),
            WireFraming::Jsonrpc => Framing::encode_request_body(
                &JsonRpcFraming,
                call_id,
                call_name,
                request_id,
                body,
                out,
            ),
        }
    }

    pub fn decode_request<'f>(self, frame: &'f [u8]) -> Option<Request<'f>> {
        match self {
            WireFraming::Datagram => DatagramFraming.decode_request(frame),
            WireFraming::Datagram2(_) => match Datagram2Framing.decode(frame)? {
                Frame::Request { request, .. } => Some(request),
                Frame::Reply { .. } => None,
            },
            WireFraming::Jsonrpc => Framing::decode_request(&JsonRpcFraming, frame),
        }
    }

    pub fn encode_response_ok(self, request_id: u64, payload: &[u8], out: &mut Vec<u8>) {
        match self {
            WireFraming::Datagram => DatagramFraming.encode_response_ok(request_id, payload, out),
            WireFraming::Datagram2(protocol) => {
                Datagram2Framing.encode_reply(protocol, request_id, &Envelope::Ok(payload), out)
            }
            WireFraming::Jsonrpc => JsonRpcFraming.encode_response_ok(request_id, payload, out),
        }
    }

    pub fn encode_response_err(self, request_id: u64, id: u16, body: &[u8], out: &mut Vec<u8>) {
        match self {
            WireFraming::Datagram => DatagramFraming.encode_response_err(request_id, id, body, out),
            WireFraming::Datagram2(protocol) => Datagram2Framing.encode_reply(
                protocol,
                request_id,
                &Envelope::Err { id, body },
                out,
            ),
            WireFraming::Jsonrpc => JsonRpcFraming.encode_response_err(request_id, id, body, out),
        }
    }

    /// Frame a provider fault — a failure the schema declares no error for.
    /// Each framing renders it in its own idiom: the datagram framing has a
    /// third tag, JSON-RPC uses its reserved `-32603 Internal error`.
    pub fn encode_response_fault(self, request_id: u64, out: &mut Vec<u8>) {
        match self {
            WireFraming::Datagram => DatagramFraming.encode_response_fault(request_id, out),
            WireFraming::Datagram2(protocol) => {
                Datagram2Framing.encode_reply(protocol, request_id, &Envelope::Fault, out)
            }
            WireFraming::Jsonrpc => JsonRpcFraming.encode_response_fault(request_id, out),
        }
    }

    pub fn decode_response<'f>(self, frame: &'f [u8]) -> Option<(u64, Envelope<'f>)> {
        match self {
            WireFraming::Datagram => DatagramFraming.decode_response(frame),
            WireFraming::Datagram2(_) => match Datagram2Framing.decode(frame)? {
                Frame::Reply {
                    request_id,
                    envelope,
                    ..
                } => Some((request_id, envelope)),
                Frame::Request { .. } => None,
            },
            WireFraming::Jsonrpc => JsonRpcFraming.decode_response(frame),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use comline_runtime::contract::RequestCall;

    /// Params as the JSON codec lays them out.
    fn json<P: serde::Serialize + ?Sized>(params: &P) -> Vec<u8> {
        serde_json::to_vec(params).unwrap()
    }

    #[test]
    fn jsonrpc_request_round_trips() {
        let f = WireFraming::Jsonrpc;
        let mut frame = Vec::new();
        f.encode_request(0, "greet", 1, false, &json(&(7u32, "x")), &mut frame);
        assert_eq!(
            std::str::from_utf8(&frame).unwrap(),
            r#"{"jsonrpc":"2.0","method":"greet","params":[7,"x"],"id":1}"#
        );

        let req = f.decode_request(&frame).unwrap();
        assert_eq!(req.call, RequestCall::Name("greet".into()));
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
    fn a_fault_round_trips_over_both_framings() {
        for f in [
            WireFraming::Datagram,
            WireFraming::Datagram2(3),
            WireFraming::Jsonrpc,
        ] {
            let mut frame = Vec::new();
            f.encode_response_fault(9, &mut frame);
            assert_eq!(
                f.decode_response(&frame),
                Some((9, Envelope::Fault)),
                "{f:?}"
            );
        }
    }

    #[test]
    fn jsonrpc_renders_a_fault_as_its_own_internal_error() {
        let mut frame = Vec::new();
        WireFraming::Jsonrpc.encode_response_fault(4, &mut frame);
        assert_eq!(
            std::str::from_utf8(&frame).unwrap(),
            r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"Internal error"},"id":4}"#
        );
    }

    #[test]
    fn a_reserved_jsonrpc_code_is_not_mistaken_for_an_ordinal() {
        // -32601 is "method not found", not error ordinal 33039 (`-32601 as u16`).
        let frame = br#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"nope"},"id":1}"#;
        assert_eq!(
            WireFraming::Jsonrpc.decode_response(frame),
            Some((1, Envelope::Fault))
        );
    }

    #[test]
    fn datagram_still_works_through_the_enum() {
        let f = WireFraming::Datagram;
        let mut frame = Vec::new();
        f.encode_request(2, "ignored", 5, false, &json(&("hi",)), &mut frame);
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
        WireFraming::Datagram.encode_request(0, "send", 1, false, &json(&("hi",)), &mut dg);
        let mut rpc = Vec::new();
        WireFraming::Jsonrpc.encode_request(0, "send", 1, false, &json(&("hi",)), &mut rpc);

        // datagram: [call_id u16 LE][request_id u64 LE][json params]
        assert_eq!(&dg[..2], &[0, 0]);
        assert!(dg.ends_with(br#"["hi"]"#));
        // json-rpc: a self-describing text frame
        assert!(std::str::from_utf8(&rpc)
            .unwrap()
            .contains(r#""method":"send""#));
    }

    #[test]
    fn datagram2_frames_carry_their_kind_and_protocol() {
        let f = WireFraming::Datagram2(3);
        let mut req = Vec::new();
        f.encode_request(2, "ignored", 5, false, &json(&("hi",)), &mut req);
        // [kind 0][protocol varint][call_id u16 LE][request_id u64 LE][params]
        assert_eq!(&req[..4], &[0, 3, 2, 0]);
        let decoded = f.decode_request(&req).unwrap();
        assert_eq!(decoded.call, RequestCall::Id(2));
        assert_eq!(decoded.request_id, 5);
        // A request is never mistaken for a reply, which datagram@1 can't tell.
        assert_eq!(f.decode_response(&req), None);

        let mut resp = Vec::new();
        f.encode_response_ok(5, b"body", &mut resp);
        assert_eq!(&resp[..2], &[1, 3]);
        assert_eq!(f.decode_response(&resp), Some((5, Envelope::Ok(b"body"))));
        assert_eq!(f.decode_request(&resp), None);

        let mut err = Vec::new();
        f.encode_response_err(5, 1, b"why", &mut err);
        assert_eq!(
            f.decode_response(&err),
            Some((
                5,
                Envelope::Err {
                    id: 1,
                    body: b"why"
                }
            ))
        );
    }

    // The cases below are runtime#45's `framing-jsonrpc/found-*` conformance
    // vectors (runtime#41, #42), byte for byte.

    #[test]
    fn a_method_name_reads_in_any_json_spelling() {
        for (frame, name) in [
            // found-request-method-escaped-slash
            (
                &br#"{"jsonrpc":"2.0","method":"app.Chat\/count","params":[],"id":1}"#[..],
                "app.Chat/count",
            ),
            // found-request-method-unicode-escapes
            (
                br#"{"jsonrpc":"2.0","method":"\u0067reet\u00E9\ud83d\ude00","params":[],"id":1}"#,
                "greet\u{e9}\u{1f600}",
            ),
        ] {
            let req = WireFraming::Jsonrpc
                .decode_request(frame)
                .unwrap_or_else(|| panic!("refused {name:?}"));
            let RequestCall::Name(read) = req.call else {
                panic!("{:?}", req.call)
            };
            assert!(read == name, "{name:?}");
            assert_eq!(read.to_string(), name);
            assert_eq!(req.request_id, 1);
            assert_eq!(req.params, b"[]");
        }
    }

    #[test]
    fn a_method_name_is_written_as_serde_json_writes_it() {
        // found-request-method-needs-escapes
        let name = "app.Chat/say \"hi\" \\ \n\t\u{1}\u{1f}\u{7f} \u{e9}\u{1f600}";
        let mut frame = Vec::new();
        WireFraming::Jsonrpc.encode_request(0, name, 6, false, &json(&("hi",)), &mut frame);
        let expected = format!(
            r#"{{"jsonrpc":"2.0","method":{},"params":["hi"],"id":6}}"#,
            serde_json::to_string(name).unwrap()
        );
        assert_eq!(std::str::from_utf8(&frame).unwrap(), expected);
        let value: serde_json::Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(value["method"], name);

        let req = WireFraming::Jsonrpc.decode_request(&frame).unwrap();
        let RequestCall::Name(read) = req.call else {
            panic!("{:?}", req.call)
        };
        assert!(read == name);
    }

    #[test]
    fn invalid_utf8_anywhere_refuses_the_frame() {
        let requests: [&[u8]; 2] = [
            // found-reject-request-invalid-utf8-in-jsonrpc
            b"{\"jsonrpc\":\"2.\xff\",\"method\":\"greet\",\"params\":[],\"id\":1}",
            // found-reject-request-invalid-utf8-in-unknown-member
            b"{\"jsonrpc\":\"2.0\",\"method\":\"greet\",\"params\":[],\"x\":\"\xff\",\"id\":1}",
        ];
        for frame in requests {
            assert_eq!(
                WireFraming::Jsonrpc.decode_request(frame),
                None,
                "{}",
                String::from_utf8_lossy(frame)
            );
        }
        let responses: [&[u8]; 3] = [
            // found-reject-response-invalid-utf8-in-jsonrpc
            b"{\"jsonrpc\":\"2.\xff\",\"result\":1,\"id\":1}",
            // found-reject-response-invalid-utf8-in-unknown-member
            b"{\"jsonrpc\":\"2.0\",\"result\":1,\"x\":\"\xff\",\"id\":1}",
            // found-reject-response-invalid-utf8-in-error-message
            b"{\"jsonrpc\":\"2.0\",\"error\":{\"code\":1,\"message\":\"\xff\"},\"id\":1}",
        ];
        for frame in responses {
            assert_eq!(
                WireFraming::Jsonrpc.decode_response(frame),
                None,
                "{}",
                String::from_utf8_lossy(frame)
            );
        }
    }

    #[test]
    fn a_method_that_is_not_a_json_string_is_refused() {
        for frame in [
            &br#"{"jsonrpc":"2.0","method":7,"params":[],"id":1}"#[..],
            br#"{"jsonrpc":"2.0","method":"\ud800","params":[],"id":1}"#,
        ] {
            assert_eq!(WireFraming::Jsonrpc.decode_request(frame), None);
        }
    }

    #[test]
    fn a_one_way_call_is_framed_as_a_client_frames_it() {
        // No framing the simulator speaks puts one-way on the wire:
        // datagram@1 and @2 have no bit for it, and its JSON-RPC is a
        // lockstep `Client`'s, whose `notify` keeps the `id`.
        let body = json(&("hi",));
        for f in [
            WireFraming::Datagram,
            WireFraming::Datagram2(3),
            WireFraming::Jsonrpc,
        ] {
            let (mut call, mut note) = (Vec::new(), Vec::new());
            f.encode_request(0, "note", 7, false, &body, &mut call);
            f.encode_request(0, "note", 7, true, &body, &mut note);
            assert_eq!(note, call, "{f:?}");
            assert_eq!(f.decode_request(&note).unwrap().request_id, 7, "{f:?}");
        }
        let mut note = Vec::new();
        WireFraming::Jsonrpc.encode_request(0, "note", 7, true, &body, &mut note);
        assert_eq!(
            std::str::from_utf8(&note).unwrap(),
            r#"{"jsonrpc":"2.0","method":"note","params":["hi"],"id":7}"#
        );
    }
}
