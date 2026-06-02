//! `logi_schema` — foundational, dependency-light POD types shared across the
//! LogiUE Rust workspace and across the FFI boundary: the stable simulation
//! identity, IDs, vectors, camera/intent/spawn inputs, and spawn recipes.
//! No `bevy`, no Unreal. (`bevy_ecs::Entity` is NEVER the durable identity.)

use serde::{Deserialize, Serialize};

/// Minimal POD 3-vector. `repr(C)`; `f32` for parity with Unreal render data.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Vec3 {
	pub x: f32,
	pub y: f32,
	pub z: f32,
}

impl Vec3 {
	pub const ZERO: Vec3 = Vec3 { x: 0.0, y: 0.0, z: 0.0 };
	pub const fn new(x: f32, y: f32, z: f32) -> Self {
		Self { x, y, z }
	}
}

/// The durable simulation identity exposed to Unreal. Stable across the entity's
/// life and independent of `bevy_ecs::Entity` (which is recycled and internal).
/// `index == 0` (with `generation == 0`) denotes "invalid / auto-allocate".
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LogiEntityId {
	pub index: u64,
	pub generation: u32,
	pub shard: u16,
	pub kind: u16,
}

impl LogiEntityId {
	#[inline]
	pub const fn is_valid(self) -> bool {
		!(self.index == 0 && self.generation == 0)
	}
}

/// `LogiInitDesc::flags` bit: run the sim with the bounded-parallel scheduler
/// (multi-threaded executor + a sized `ComputeTaskPool`). Default (bit unset) is
/// the single-threaded deterministic scheduler.
pub const LOGI_FLAG_BOUNDED_PARALLEL: u32 = 1 << 0;

/// Domain IDs (deterministic, stable). 0 == invalid.
macro_rules! logi_id {
	($name:ident) => {
		#[repr(transparent)]
		#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
		pub struct $name(pub u64);
		impl $name {
			#[inline]
			pub const fn new(v: u64) -> Self {
				Self(v)
			}
			#[inline]
			pub const fn is_valid(self) -> bool {
				self.0 != 0
			}
		}
	};
}

logi_id!(LogiTileId);
logi_id!(LogiRouteId);
logi_id!(LogiAgentId);
logi_id!(LogiShipmentId);

// --- Inputs that cross the FFI (Unreal -> Rust), all plain data --------------

/// A camera view feeding the significance model (ADR-0003). World-scale, so f64.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LogiCameraView {
	pub position: [f64; 3],
	pub forward: [f64; 3],
	pub fov_degrees: f32,
	pub screen_height_px: f32,
}

/// A player intent (select, pin, command). Non-authoritative input.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LogiPlayerIntent {
	pub kind: u32,
	pub target: LogiEntityId,
	pub params: [f64; 4],
}

/// A request to spawn one entity. `requested_id` invalid => auto-allocate.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LogiSpawnCommand {
	pub kind: u16,
	pub requested_id: LogiEntityId,
	pub position: [f64; 3],
	pub route_id: u64,
	pub payload_kg: f64,
}

/// Plain-data recipe describing one agent to spawn (authoring -> sim).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct AgentSpawnRecipe {
	pub agent: LogiAgentId,
	pub route: LogiRouteId,
	pub tile: LogiTileId,
	pub position: Vec3,
	pub payload_kg: f64,
}
