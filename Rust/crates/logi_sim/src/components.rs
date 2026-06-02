//! Gameplay Components — the atomic units of authoritative runtime state.

use bevy_ecs::prelude::*;
use logi_schema::{LogiAgentId, LogiEntityId, LogiRouteId, LogiShipmentId, LogiTileId, Vec3};

/// The durable simulation identity of an entity, mirrored to Unreal. The bridge
/// between Unreal's `LogiEntityId` references and the internal `bevy_ecs::Entity`.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct LogiId(pub LogiEntityId);

/// Identity + assignment of a logistics agent.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Agent {
	pub id: LogiAgentId,
	pub route: LogiRouteId,
	pub tile: LogiTileId,
}

/// Authoritative world-space position (cm).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Position(pub Vec3);

/// Linear velocity (cm per second).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Velocity(pub Vec3);

/// The shipment an agent is carrying.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Shipment {
	pub id: LogiShipmentId,
	pub payload_kg: f64,
}

/// Marker: this entity participates in the fixed-step simulation.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Simulated;
