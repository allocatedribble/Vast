//! `logi_render_extract` — per-frame snapshots handed to Unreal. POD, `repr(C)`,
//! so the whole buffer crosses the FFI by pointer+count with NO per-entity calls
//! (the batched boundary mandated by ADR-0005). Identity is the stable
//! `LogiEntityId` — the raw `bevy_ecs::Entity` never leaves Rust.

use bevy_ecs::prelude::*;
use logi_schema::LogiEntityId;
use logi_sim::components::{LogiId, Position};

/// One agent's render-relevant state. `repr(C)`: crosses the FFI verbatim.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LogiAgentInstance {
	pub id: LogiEntityId,
	pub x: f32,
	pub y: f32,
	pub z: f32,
	/// 0 = High, 1 = Medium, 2 = Low, 3 = Off (significance-driven; ADR-0003).
	pub sim_lod: u8,
}

/// Owned snapshot buffer. Lives in the world; Unreal borrows a view each frame.
/// Reused across frames to avoid per-frame allocation.
#[derive(Default)]
pub struct RenderSnapshot {
	pub snapshot_id: u64,
	pub sim_tick: u64,
	pub agents: Vec<LogiAgentInstance>,
}

impl RenderSnapshot {
	pub fn clear(&mut self) {
		self.agents.clear();
	}
}

/// Extract render instances from the World into `snapshot.agents` (buffer reused).
/// Leaves `snapshot_id` / `sim_tick` for the caller to stamp.
pub fn extract_agents(world: &mut World, snapshot: &mut RenderSnapshot) {
	snapshot.agents.clear();
	let mut query = world.query::<(&LogiId, &Position)>();
	for (id, pos) in query.iter(world) {
		snapshot.agents.push(LogiAgentInstance {
			id: id.0,
			x: pos.0.x,
			y: pos.0.y,
			z: pos.0.z,
			sim_lod: 0,
		});
	}
}
