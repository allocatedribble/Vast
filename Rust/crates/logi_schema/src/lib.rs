//! `logi_schema` — foundational, dependency-free types shared across the LogiUE
//! Rust workspace: deterministic IDs, a minimal POD vector, and spawn recipes.
//! No `bevy`, no Unreal. These are the Rust home of the former C++ `LogiCore` IDs.

/// Minimal POD 3-vector. `repr(C)` so it crosses the FFI verbatim; `f32` for
/// parity with Unreal's render-side instance data.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
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

/// Deterministic, stable 64-bit identity types. A value of 0 means "invalid".
/// Values are produced deterministically upstream (e.g. tile identity from
/// `logi_terrain`) to support replay/snapshot equality.
macro_rules! logi_id {
    ($name:ident) => {
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
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

/// Plain-data recipe describing one agent to spawn (authoring -> sim).
#[derive(Clone, Copy, Debug, Default)]
pub struct AgentSpawnRecipe {
    pub agent: LogiAgentId,
    pub route: LogiRouteId,
    pub tile: LogiTileId,
    pub position: Vec3,
    pub payload_kg: f64,
}
