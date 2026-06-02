//! `logi_render_extract` — per-frame snapshots handed to Unreal. POD, `repr(C)`,
//! so the whole buffer crosses the FFI by pointer+count with NO per-entity calls
//! (the batched boundary mandated by ADR-0005). Identity is the stable
//! `LogiEntityId`; the raw `bevy_ecs::Entity` never leaves Rust. Carries the
//! Bevy-computed representation tier for Unreal to execute (ADR-0003).

use bevy_ecs::prelude::*;
use logi_schema::LogiEntityId;
use logi_sim::components::{Representation, StableId, Transform3d};

/// One agent's render-relevant state. `repr(C)`: crosses the FFI verbatim.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LogiAgentInstance {
	pub id: LogiEntityId,
	/// World translation in cm (f64 for Unreal large-world coordinates).
	pub translation_cm: [f64; 3],
	/// `RepresentationTier` as u8 (0=None, 1=ISM, 2=LowRes, 3=HighRes). ADR-0003.
	pub tier: u8,
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
	let mut query = world.query::<(&StableId, &Transform3d, &Representation)>();
	for (id, transform, rep) in query.iter(world) {
		snapshot.agents.push(LogiAgentInstance {
			id: id.0,
			translation_cm: transform.translation_cm,
			tier: rep.current_tier as u8,
		});
	}
}
