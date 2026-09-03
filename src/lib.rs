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
//! - [`clock`] — the virtual clock and its event queue
//! - [`wire`] — one connection's tapped, fault-injecting channel
//! - [`pump`] — the discrete-event pump that ties a call to its reply

pub mod clock;
pub mod faults;
pub mod format;
pub mod frame;
pub mod pump;
pub mod rng;
pub mod wire;

use wasm_bindgen::prelude::*;

use pump::Pump;

/// WASM smoke test: run one `send` call through the pump and return the frame
/// log as a JSON string (or `"error: …"`). The playground's real surface lands
/// here as the port progresses.
#[wasm_bindgen]
pub fn smoke() -> String {
    let mut pump = Pump::new();
    if let Err(e) = pump.call(0, &serde_json::json!(["hi"])) {
        return format!("error: {e:?}");
    }
    pump.run();
    serde_json::to_string(&pump.tap().frames).unwrap_or_else(|_| "[]".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_logs_a_request_and_a_response() {
        let log = smoke();
        let frames: serde_json::Value = serde_json::from_str(&log).unwrap();
        assert_eq!(frames.as_array().unwrap().len(), 2);
        assert_eq!(frames[0]["from"], "client");
        assert_eq!(frames[1]["from"], "server");
    }
}
