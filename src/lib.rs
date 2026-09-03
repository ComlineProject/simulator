//! `comline-simulator` — the simulation engine behind the Comline playground
//! and tutorial. Wires protocol instances over the *real* `comline-runtime`
//! contract (framing / envelope / handshake / dispatch), in-memory, driven as a
//! discrete-event simulation on a single thread so a virtual clock can pause it.
//!
//! Modules:
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
pub mod framing;
pub mod generic;
pub mod model;
pub mod record;
pub mod rng;
pub mod session_codec;
pub mod shape;
pub mod wire;

pub use facade::Sim;

#[cfg(test)]
mod fixtures;
