//! `comline-simulator` — the simulation engine behind the Comline playground
//! and tutorial. Wires protocol instances over the *real* `comline-runtime`
//! contract (framing / envelope / handshake / dispatch), in-memory, driven as a
//! discrete-event simulation on a single thread so a virtual clock can pause it.
//!
//! The engine is being ported from the playground's TypeScript (`app/src/sim/`)
//! module by module:
//!
//! - [`rng`] — seeded PRNG, bit-for-bit with the JS reference
//! - [`faults`] — the unreliable-wire spec and its transforms
//! - [`frame`] — the frame tap the inspector reads
//! - [`format`] — the JSON [`WireFormat`](comline_runtime::contract::WireFormat)
//! - [`shape`] — the compiled-project projection the playground's `describe_project` emits
//! - [`clock`] — the virtual clock and its event queue
//! - [`wire`] — one connection's tapped, fault-injecting channel
//! - [`behavior`] — what a server instance does for one function
//! - [`generic`] — a dispatcher driven by a [`shape::ProtocolShape`], no codegen
//! - [`model`] — the session: nodes, instances, connections and their operations
//! - [`session_codec`] — the session ⇄ `#s=…` shareable link
//! - [`record`] — record & replay input capture
//! - [`engine`] — many connections over one clock; the discrete-event pump
//! - [`framedecode`] — a raw frame → the inspector's decoded view
//! - [`facade`] — the `#[wasm_bindgen]` `Sim` surface the playground UI drives

pub mod behavior;
pub mod clock;
pub mod engine;
pub mod facade;
pub mod faults;
pub mod format;
pub mod frame;
pub mod framedecode;
pub mod generic;
pub mod model;
pub mod record;
pub mod rng;
pub mod session_codec;
pub mod shape;
pub mod wire;

pub use facade::Sim;

use wasm_bindgen::prelude::*;

/// WASM smoke test: run one `send` call through the engine and return the frame
/// log as a JSON string (or `"error: …"`). The playground's real surface lands
/// here as the port progresses.
#[wasm_bindgen]
pub fn smoke() -> String {
    let mut sim = engine::chat::engine();
    if let Err(e) = sim.call(engine::chat::CONN, "send", &serde_json::json!(["hi"])) {
        return format!("error: {e:?}");
    }
    sim.run();
    match sim.tap(engine::chat::CONN) {
        Some(tap) => serde_json::to_string(&tap.frames).unwrap_or_else(|_| "[]".into()),
        None => "[]".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_logs_a_handshake_a_request_and_a_response() {
        let log = smoke();
        let frames: Vec<serde_json::Value> = serde_json::from_str(&log).unwrap();
        // two handshake frames, then the request and the response
        assert_eq!(frames.len(), 4);
        assert_eq!(frames[0]["kind"], "handshake");
        assert_eq!(frames[2]["kind"], "request");
        assert_eq!(frames[2]["from"], "chat-2");
        assert_eq!(frames[3]["from"], "chat-1");
    }
}
