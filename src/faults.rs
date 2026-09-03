//! What an unreliable wire does to a frame. One [`FaultSpec`] per connection; the
//! engine applies it to that connection's frames in both directions, so a tweak
//! in the inspector takes effect on the next frame with no reconnect. Ported from
//! the playground's `faults.ts`.
//!
//! The field names serialize in `camelCase` to stay wire-compatible with session
//! links and recordings produced by the TypeScript engine.

use serde::{Deserialize, Serialize};

use crate::rng::Mulberry32;

/// Which frames a connection's faults act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FaultDir {
    Requests,
    Responses,
    Both,
}

/// The direction of a frame currently in flight — matched against
/// [`FaultSpec::apply_to`] by [`fault_applies_to`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Request,
    Response,
}

/// The unreliable-wire spec for one connection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FaultSpec {
    /// `0..=1` — probability a frame is dropped outright.
    pub drop_prob: f64,
    /// Uniform delivery delay, ms. `0..0` = none.
    pub delay_min: f64,
    pub delay_max: f64,
    /// Hold up to N frames, then release them shuffled. `0` = keep order.
    pub reorder_window: u32,
    /// `0..=1` — probability a body byte is flipped before delivery.
    pub corrupt_prob: f64,
    /// Hard cut both directions — every frame dropped until cleared.
    pub partition: bool,
    /// Which direction the drop / delay / reorder / corrupt apply to.
    pub apply_to: FaultDir,
}

impl Default for FaultSpec {
    fn default() -> Self {
        Self {
            drop_prob: 0.0,
            delay_min: 0.0,
            delay_max: 0.0,
            reorder_window: 0,
            corrupt_prob: 0.0,
            partition: false,
            apply_to: FaultDir::Both,
        }
    }
}

/// A clean, ordered, immediate pass-through.
pub fn no_faults() -> FaultSpec {
    FaultSpec::default()
}

/// Any behaviour that isn't a clean, ordered, immediate pass-through.
pub fn faults_active(f: &FaultSpec) -> bool {
    f.partition
        || f.drop_prob > 0.0
        || f.corrupt_prob > 0.0
        || f.reorder_window > 0
        || f.delay_max > 0.0
}

/// Whether `f` acts on a frame travelling in `dir`.
pub fn fault_applies_to(f: &FaultSpec, dir: Direction) -> bool {
    match f.apply_to {
        FaultDir::Both => true,
        FaultDir::Requests => dir == Direction::Request,
        FaultDir::Responses => dir == Direction::Response,
    }
}

/// A copy of `bytes` with one byte in its back half flipped — enough to make the
/// body fail to decode without mangling the frame header.
pub fn corrupt_bytes(bytes: &[u8], rng: &mut Mulberry32) -> Vec<u8> {
    let mut out = bytes.to_vec();
    if out.is_empty() {
        return out;
    }
    let half = out.len() as f64 / 2.0;
    let i = (half + rng.next_f64() * half).floor() as usize;
    let i = i.min(out.len() - 1);
    out[i] ^= 0xff;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_spec_is_inert() {
        assert!(!faults_active(&no_faults()));
    }

    #[test]
    fn every_knob_registers_as_active() {
        let on = |f: fn(&mut FaultSpec)| {
            let mut s = no_faults();
            f(&mut s);
            faults_active(&s)
        };
        assert!(on(|s| s.partition = true));
        assert!(on(|s| s.drop_prob = 0.1));
        assert!(on(|s| s.corrupt_prob = 0.1));
        assert!(on(|s| s.reorder_window = 2));
        assert!(on(|s| s.delay_max = 50.0));
        // delay_min alone (without a max) is not a fault
        assert!(!on(|s| s.delay_min = 50.0));
    }

    #[test]
    fn apply_to_gates_direction() {
        let mut s = no_faults();

        s.apply_to = FaultDir::Both;
        assert!(fault_applies_to(&s, Direction::Request));
        assert!(fault_applies_to(&s, Direction::Response));

        s.apply_to = FaultDir::Requests;
        assert!(fault_applies_to(&s, Direction::Request));
        assert!(!fault_applies_to(&s, Direction::Response));

        s.apply_to = FaultDir::Responses;
        assert!(!fault_applies_to(&s, Direction::Request));
        assert!(fault_applies_to(&s, Direction::Response));
    }

    #[test]
    fn corrupt_flips_one_byte_in_the_back_half() {
        let original = [0u8; 10];
        let mut rng = Mulberry32::new(1);
        let out = corrupt_bytes(&original, &mut rng);

        assert_eq!(out.len(), original.len());
        let flipped: Vec<usize> = (0..out.len()).filter(|&i| out[i] != original[i]).collect();
        // seed 1's first draw (0.627…) lands the flip at index 8.
        assert_eq!(flipped, vec![8]);
        assert_eq!(out[8], 0xff);
    }

    #[test]
    fn corrupt_leaves_an_empty_slice_alone() {
        let mut rng = Mulberry32::new(1);
        assert!(corrupt_bytes(&[], &mut rng).is_empty());
    }

    #[test]
    fn spec_round_trips_through_camel_case_json() {
        let mut s = no_faults();
        s.drop_prob = 0.25;
        s.reorder_window = 3;
        s.apply_to = FaultDir::Responses;

        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"dropProb\":0.25"), "{json}");
        assert!(json.contains("\"reorderWindow\":3"), "{json}");
        assert!(json.contains("\"applyTo\":\"responses\""), "{json}");

        let back: FaultSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }
}
