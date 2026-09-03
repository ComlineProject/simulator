//! A tiny seeded PRNG (mulberry32), ported byte-for-byte from the playground's
//! `rng.ts`. The fault rolls draw from this instead of a system RNG so a stepped
//! run with a fixed `Session` seed produces the same frame sequence every time —
//! the basis for record & replay.

/// mulberry32: 32 bits of state, an `f64` in `[0, 1)` per step. Matches the
/// JavaScript reference bit for bit (same seed → same stream), so a recording
/// made in either engine replays identically.
#[derive(Clone, Debug)]
pub struct Mulberry32 {
    state: u32,
}

impl Mulberry32 {
    pub fn new(seed: u32) -> Self {
        Self { state: seed }
    }

    /// The next draw in `[0, 1)`.
    ///
    /// `wrapping_*` on `u32` reproduces JS `| 0` / `Math.imul`, and `>>` on
    /// `u32` is the logical shift JS writes as `>>> `.
    pub fn next_f64(&mut self) -> f64 {
        self.state = self.state.wrapping_add(0x6d2b_79f5);
        let mut t = (self.state ^ (self.state >> 15)).wrapping_mul(1 | self.state);
        t = t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t)) ^ t;
        f64::from(t ^ (t >> 14)) / 4_294_967_296.0
    }

    /// `true` with probability `p` — the shape the fault rolls want.
    pub fn chance(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference streams captured from the `rng.ts` implementation running under
    /// Node. If this breaks, replays made by the two engines have diverged.
    #[test]
    fn matches_the_javascript_reference_stream() {
        let cases: &[(u32, [f64; 6])] = &[
            (
                1,
                [
                    0.6270739405881613,
                    0.002735721180215478,
                    0.5274470399599522,
                    0.9810509674716741,
                    0.9683778982143849,
                    0.281103502959013,
                ],
            ),
            (
                42,
                [
                    0.6011037519201636,
                    0.44829055899754167,
                    0.8524657934904099,
                    0.6697340414393693,
                    0.17481389874592423,
                    0.5265925421845168,
                ],
            ),
            (
                123_456_789,
                [
                    0.2577907438389957,
                    0.9707721115555614,
                    0.7853280142880976,
                    0.20616457983851433,
                    0.30307188746519387,
                    0.7470660470426083,
                ],
            ),
        ];

        for &(seed, expected) in cases {
            let mut rng = Mulberry32::new(seed);
            for (i, want) in expected.iter().enumerate() {
                let got = rng.next_f64();
                assert!(
                    (got - want).abs() < 1e-15,
                    "seed {seed}, draw {i}: got {got}, want {want}",
                );
            }
        }
    }

    #[test]
    fn draws_stay_in_the_unit_interval() {
        let mut rng = Mulberry32::new(7);
        for _ in 0..10_000 {
            let x = rng.next_f64();
            assert!((0.0..1.0).contains(&x), "{x} out of range");
        }
    }

    #[test]
    fn same_seed_same_stream() {
        let mut a = Mulberry32::new(99);
        let mut b = Mulberry32::new(99);
        for _ in 0..256 {
            assert_eq!(a.next_f64().to_bits(), b.next_f64().to_bits());
        }
    }
}
