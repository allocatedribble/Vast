//! `logi_schema` — foundational, dependency-light POD types shared across the
//! LogiUE Rust workspace and across the FFI boundary: the stable simulation
//! identity, IDs, vectors, asset handles, representation tiers, camera/intent/
//! spawn inputs, and spawn recipes. No `bevy`, no Unreal.
//! (`bevy_ecs::Entity` is NEVER the durable identity.)

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

/// `LogiInitDesc::flags` bit: FORCE the single-threaded deterministic scheduler.
/// Default (bit unset) is the bounded-parallel **multi-threaded** scheduler --
/// Bevy systems run in parallel by default and parallelism is encouraged;
/// determinism is preserved by explicit system ordering + commutative writes,
/// not by serializing. Set this bit only for strict replay equality / tests.
pub const LOGI_FLAG_FORCE_SINGLE_THREADED: u32 = 1 << 0;

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

/// Handle to an Unreal-side asset (§9). Rust stores handles, never `UObject*`;
/// Unreal owns the mapping. `kind` selects the asset category and `stable_hash`
/// keys it. `stable_hash == 0` means "none". Used for meshes, materials,
/// skeletons, actor classes, and anim instances alike.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LogiAssetHandle {
	pub stable_hash: u64,
	pub kind: u32,
}

impl LogiAssetHandle {
	pub const NONE: LogiAssetHandle = LogiAssetHandle { stable_hash: 0, kind: asset_kind::NONE };
	#[inline]
	pub const fn new(stable_hash: u64, kind: u32) -> Self {
		Self { stable_hash, kind }
	}
	#[inline]
	pub const fn is_valid(self) -> bool {
		self.stable_hash != 0
	}
}

/// Asset categories for `LogiAssetHandle::kind`. Unreal maps each to a concrete
/// asset/class type (§9).
pub mod asset_kind {
	pub const NONE: u32 = 0;
	pub const STATIC_MESH: u32 = 1;
	pub const SKELETAL_MESH: u32 = 2;
	pub const MATERIAL: u32 = 3;
	pub const ACTOR_CLASS: u32 = 4;
	pub const ANIM_INSTANCE: u32 = 5;
}

/// Animation parameters Unreal feeds to an AnimInstance for a represented agent.
/// Bevy computes these; Unreal presents them. `repr(C)`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LogiAnimationParams {
	pub locomotion_state: u16,
	pub speed_cm_s: f32,
	pub turn_rate: f32,
	pub aim_yaw_pitch: [f32; 2],
}

/// Representation tier (ADR-0003). **Bevy decides; Unreal executes.** `repr(u8)`
/// so it can cross the FFI. Ordered low→high fidelity.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RepresentationTier {
	#[default]
	None = 0,
	InstancedStaticMesh = 1,
	LowResActor = 2,
	HighResActor = 3,
}

/// Render option bitflags carried on `Renderable`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderFlags(pub u32);

impl RenderFlags {
	pub const NONE: RenderFlags = RenderFlags(0);
	pub const CAST_SHADOW: RenderFlags = RenderFlags(1 << 0);
	pub const RECEIVE_DECALS: RenderFlags = RenderFlags(1 << 1);
	pub const VISIBLE: RenderFlags = RenderFlags(1 << 2);

	#[inline]
	pub const fn contains(self, other: RenderFlags) -> bool {
		(self.0 & other.0) == other.0
	}
}

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

/// Editor-only inspection summary of one entity (ADR-0006). POD + paged. Excluded
/// from the runtime header (cbindgen `exclude`); emitted into the separate
/// `logi_bevy_editor_ffi.h`. Mirrors a row in the Bevy Entity Outliner.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LogiEntitySummary {
	pub id: LogiEntityId,
	pub position_cm: [f64; 3],
	pub significance: f32,
	pub representation_tier: u8,
	pub pinned: u8,
	pub _pad: [u8; 2],
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
