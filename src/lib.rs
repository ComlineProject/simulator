//! `comline-simulator` — the simulation engine behind the Comline playground
//! and tutorial. Wires two protocol instances over the *real* `comline-runtime`
//! contract, in-memory, pumped on a single thread so a virtual clock can pause
//! it. The `smoke_send` spike proves a `send` round-trip works against the
//! runtime's framing / envelope / dispatch; the engine (faults / clock /
//! behaviours / record-replay) is being ported on top of it module by module.

pub mod faults;
pub mod frame;
pub mod rng;

use std::collections::VecDeque;

use comline_runtime::contract::{
    BufMut, DatagramFraming, Dispatch, Envelope, Framing, Kind, Reply, RuntimeError, WireFormat,
};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

// ── JSON wire format (the runtime's own is `std`-gated behind `rmp-serde`) ──

struct Json;

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

// ── a tapped in-memory channel pair ────────────────────────────────────────

#[derive(Default)]
struct Chan {
    /// client → server
    a2b: VecDeque<Vec<u8>>,
    /// server → client
    b2a: VecDeque<Vec<u8>>,
    /// every frame, `(from, bytes)` — the frame log
    log: Vec<(&'static str, Vec<u8>)>,
}

// ── a dispatcher that replies with a fixed value ──────────────────────────

struct ConstDispatch {
    /// serialised `Message` to send back for `send`
    reply: Vec<u8>,
}

const CALLS: &[&str] = &["send"];

impl Dispatch for ConstDispatch {
    fn calls(&self) -> &'static [&'static str] {
        CALLS
    }

    fn dispatch<W: WireFormat>(
        &self,
        call: Kind,
        _params: &[u8],
        _format: &W,
        reply: &mut Reply,
    ) -> Result<(), RuntimeError> {
        match call.resolve(CALLS) {
            Some(0) => {
                reply.ok(&self.reply);
                Ok(())
            }
            _ => Err(RuntimeError::UnknownCall),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Message {
    body: String,
    seq: u64,
}

/// One `send` call, client → server → back, pumped by hand. Returns the frame
/// log (`"from → len B"` lines) so the caller can see the wire traffic.
pub fn smoke_send(text: &str, reply_body: &str) -> Result<Vec<String>, String> {
    let fmt = Json;
    let framing = DatagramFraming;

    let mut reply_buf = Vec::new();
    fmt.encode(
        &Message {
            body: reply_body.into(),
            seq: 1,
        },
        &mut reply_buf,
    )
    .map_err(|e| format!("{e:?}"))?;
    let dispatch = ConstDispatch { reply: reply_buf };

    let mut chan = Chan::default();

    // client: frame the request, push it onto the wire
    let mut req = Vec::new();
    framing
        .encode_request(0u16.into(), 0, &(text,), &fmt, &mut req)
        .map_err(|e| format!("encode_request: {e:?}"))?;
    chan.log.push(("client", req.clone()));
    chan.a2b.push_back(req);

    // server pump: decode the request, dispatch, frame the response
    let inbound = chan.a2b.pop_front().ok_or("no request queued")?;
    let request = framing
        .decode_request(&inbound)
        .ok_or("bad request frame")?;
    let mut body = Vec::new();
    let outcome = {
        let mut reply = Reply::new(&mut body);
        dispatch
            .dispatch(Kind::Id(0), request.params, &fmt, &mut reply)
            .map_err(|e| format!("dispatch: {e:?}"))?;
        reply.outcome()
    };
    let mut resp = Vec::new();
    match outcome {
        comline_runtime::contract::Outcome::Ok => {
            framing.encode_response_ok(request.request_id, &body, &mut resp)
        }
        comline_runtime::contract::Outcome::Err(id) => {
            framing.encode_response_err(request.request_id, id, &body, &mut resp)
        }
        comline_runtime::contract::Outcome::None => return Err("one-way, no reply".into()),
    }
    chan.log.push(("server", resp.clone()));
    chan.b2a.push_back(resp);

    // client: read the response
    let inbound = chan.b2a.pop_front().ok_or("no response queued")?;
    let (echoed, envelope) = framing
        .decode_response(&inbound)
        .ok_or("bad response frame")?;
    if echoed != 0 {
        return Err(format!("request-id mismatch: {echoed}"));
    }
    let value: Message = match envelope {
        Envelope::Ok(payload) => fmt
            .decode(payload)
            .map_err(|e| format!("decode ok: {e:?}"))?,
        Envelope::Err { id, .. } => return Err(format!("remote error, ordinal {id}")),
    };
    if value.body != reply_body {
        return Err(format!("wrong reply body: {}", value.body));
    }

    Ok(chan
        .log
        .iter()
        .map(|(from, bytes)| format!("{from} → {} B", bytes.len()))
        .collect())
}

/// WASM entry — returns the frame log as a JSON string, or `"error: …"`.
#[wasm_bindgen]
pub fn smoke() -> String {
    match smoke_send("hi", "HELLO") {
        Ok(log) => serde_json::to_string(&log).unwrap_or_else(|_| "[]".into()),
        Err(e) => format!("error: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_round_trips_over_the_real_contract() {
        let log = smoke_send("hi", "HELLO").expect("round trip");
        assert_eq!(log.len(), 2, "one request frame, one response frame");
        assert!(log[0].starts_with("client → "));
        assert!(log[1].starts_with("server → "));
    }
}
