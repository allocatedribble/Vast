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

/// Deterministic RNG (SplitMix64). Seeded from sim inputs only — never wall-clock,
/// never `thread_rng` (ADR-0001 Determinism, still in force under ADR-0005).
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
