//! `logi_render_extract` — per-frame snapshots handed to Unreal. POD, `repr(C)`,
//! so the whole buffer crosses the FFI by pointer+len with NO per-entity calls
//! (the batched boundary mandated by ADR-0005).

use bevy_ecs::prelude::*;
use logi_sim::components::{Agent, Position};

/// One agent's render-relevant state. `repr(C)`: crosses the FFI verbatim.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AgentInstance {
    /// Bevy entity bits (stable within a runtime instance).
    pub entity: u64,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub agent_id: u64,
    /// 0 = High, 1 = Medium, 2 = Low, 3 = Off (significance-driven; ADR-0003).
    pub sim_lod: u8,
}

/// Owned snapshot buffer. Lives in the runtime; Unreal borrows a view each frame.
/// Reused across frames to avoid per-frame allocation.
#[derive(Default)]
pub struct RenderSnapshot {
    pub agents: Vec<AgentInstance>,
}

impl RenderSnapshot {
    pub fn clear(&mut self) {
        self.agents.clear();
    }
}

/// Extract render instances from the World into `snapshot` (buffer reused).
pub fn extract_agents(world: &mut World, snapshot: &mut RenderSnapshot) {
    snapshot.clear();
    let mut query = world.query::<(Entity, &Agent, &Position)>();
    for (entity, agent, pos) in query.iter(world) {
        snapshot.agents.push(AgentInstance {
            entity: entity.to_bits(),
            x: pos.0.x,
            y: pos.0.y,
            z: pos.0.z,
            agent_id: agent.id.0,
            sim_lod: 0,
        });
    }
}
