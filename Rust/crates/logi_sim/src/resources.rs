//! Global/singleton simulation state held as Bevy Resources.

use crate::terrain::Terrain;
use bevy_ecs::prelude::*;
use logi_schema::{LogiCameraView, LogiPlayerIntent};

/// Fixed-step clock. `tick` advances exactly once per fixed step.
#[derive(Resource, Clone, Copy, Debug)]
pub struct SimClock {
    pub tick: u64,
    pub fixed_hz: u32,
}

impl Default for SimClock {
    fn default() -> Self {
        Self { tick: 0, fixed_hz: 30 }
    }
}

impl SimClock {
    #[inline]
    pub fn seconds_per_tick(&self) -> f32 {
        if self.fixed_hz > 0 {
            1.0 / self.fixed_hz as f32
        } else {
            0.0
        }
    }
}

/// Deterministic RNG (SplitMix64), seeded from `deterministic_seed` at world
/// creation. **This is the ONLY sanctioned source of randomness in the
/// simulation** (audit M23): every stochastic system MUST draw from this resource
/// via [`SimRng::next_u64`]. Seeded from sim inputs only — **never** `thread_rng`,
/// `rand::random`, a wall-clock, or any per-thread/global entropy, all of which
/// break replay/snapshot determinism (§8 / ADR-0001 Determinism, in force under
/// ADR-0005).
///
/// It is held in the `World` from creation but not yet consumed by any system:
/// the current sim is fully deterministic without randomness. The resource exists
/// so the first stochastic feature reaches for the sanctioned path instead of
/// introducing `thread_rng`. The same-seed/different-seed contract is locked by
/// `tests::sim_rng_is_seed_deterministic`.
#[derive(Resource, Clone, Copy, Debug)]
pub struct SimRng {
    pub state: u64,
}

impl SimRng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// The terrain authority, held as a simulation Resource.
#[derive(Resource, Default)]
pub struct TerrainResource(pub Terrain);

/// Latest per-frame Unreal input, ingested at the `InputIngest` phase. Read by
/// significance/representation systems; non-authoritative.
#[derive(Resource, Default)]
pub struct UnrealFrameInput {
    pub cameras: Vec<LogiCameraView>,
    pub intents: Vec<LogiPlayerIntent>,
}

/// Per-frame representation budgets (ADR-0003). Fidelity is allocated TOP-DOWN by
/// significance against these caps — never by raw distance.
#[derive(Resource, Clone, Copy, Debug)]
pub struct RepresentationBudget {
    pub high_res_actors: u32,
    pub low_res_actors: u32,
    pub instanced_meshes: u32,
}

impl Default for RepresentationBudget {
    fn default() -> Self {
        Self {
            high_res_actors: 64,
            low_res_actors: 256,
            instanced_meshes: 100_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SimRng;

    /// M23: the sanctioned RNG is purely seed-deterministic — the same seed yields the
    /// same sequence (replay equality) and different seeds diverge — with no wall-clock
    /// or thread-local entropy anywhere. Locks the contract so the first stochastic
    /// system can rely on `SimRng` instead of reaching for `thread_rng`.
    #[test]
    fn sim_rng_is_seed_deterministic() {
        let draw = |seed: u64| {
            let mut rng = SimRng::new(seed);
            [rng.next_u64(), rng.next_u64(), rng.next_u64(), rng.next_u64()]
        };
        // Same seed -> identical sequence (replay-stable).
        assert_eq!(draw(0xDEAD_BEEF), draw(0xDEAD_BEEF));
        // Different seed -> different sequence.
        assert_ne!(draw(1), draw(2));
        // A stream advances (not a constant) and is a pure function of prior state.
        let mut a = SimRng::new(42);
        let mut b = SimRng::new(42);
        let first = a.next_u64();
        assert_ne!(first, a.next_u64(), "consecutive draws advance state");
        assert_eq!(first, b.next_u64(), "same-seed streams stay in lockstep");
    }
}
