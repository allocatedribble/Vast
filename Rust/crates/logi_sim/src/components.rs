//! Gameplay Components — the atomic units of authoritative runtime state (§7).

use bevy_ecs::prelude::*;
use logi_schema::{
    LogiAgentId, LogiAssetHandle, LogiEntityId, LogiRouteId, LogiShipmentId, LogiTileId, RenderFlags,
    RepresentationTier,
};

/// The durable simulation identity, mirrored to Unreal. Bridges Unreal's
/// `LogiEntityId` references to the internal `bevy_ecs::Entity`.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct StableId(pub LogiEntityId);

/// Authoritative transform. Translation is f64 (centimeters) for planetary /
/// large-world-coordinate scale; rotation (quaternion xyzw) and scale are f32.
#[derive(Component, Clone, Copy, Debug)]
pub struct Transform3d {
    pub translation_cm: [f64; 3],
    pub rotation_xyzw: [f32; 4],
    pub scale: [f32; 3],
}

impl Default for Transform3d {
    fn default() -> Self {
        Self {
            translation_cm: [0.0; 3],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0], // identity quaternion
            scale: [1.0, 1.0, 1.0],
        }
    }
}

/// Linear (cm/s) and angular (rad/s) velocity.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Velocity3d {
    pub linear_cm_s: [f32; 3],
    pub angular_rad_s: [f32; 3],
}

/// Render asset references + flags. Handles are resolved Unreal-side.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Renderable {
    pub mesh: LogiAssetHandle,
    pub material: LogiAssetHandle,
    pub skeleton: LogiAssetHandle,
    pub flags: RenderFlags,
}

/// Representation state (ADR-0003). Bevy computes `desired_tier` top-down by
/// `significance` against a budget; `current_tier` follows with hysteresis;
/// Unreal executes whatever `current_tier` says.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Representation {
    pub desired_tier: RepresentationTier,
    pub current_tier: RepresentationTier,
    pub significance: f32,
    pub hysteresis_key: u8,
}

/// Inputs that drive an Unreal animation graph for a represented agent.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct AnimationDrive {
    pub anim_graph: LogiAssetHandle,
    pub locomotion_state: u16,
    pub speed_cm_s: f32,
    pub turn_rate: f32,
    pub aim_yaw_pitch: [f32; 2],
}

/// Marker: this entity's transform changed this step (for incremental extract).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct DirtyTransform;

/// Visibility/culling state. **Reserved**: no culling/frustum system populates it
/// yet, so `compute_significance` does not read it (audit Low — significance is
/// camera-proximity only for now). It rides on spawned entities so the future
/// culling system can fill it and feed a screen-area significance term.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct CullState {
    pub visible_to_any_camera: bool,
    pub projected_screen_area: f32,
}

// --- Domain components -------------------------------------------------------

/// Identity + assignment of a logistics agent.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Agent {
    pub id: LogiAgentId,
    pub route: LogiRouteId,
    pub tile: LogiTileId,
}

/// The shipment an agent is carrying.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Shipment {
    pub id: LogiShipmentId,
    pub payload_kg: f64,
}

/// Marker: this entity participates in the fixed-step simulation.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Simulated;
