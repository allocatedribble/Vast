//! Gameplay Components — the atomic units of authoritative runtime state.

use bevy_ecs::prelude::*;
use logi_schema::{LogiAgentId, LogiRouteId, LogiShipmentId, LogiTileId, Vec3};

/// Identity + assignment of a logistics agent.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Agent {
    pub id: LogiAgentId,
    pub route: LogiRouteId,
    /// Terrain tile the agent is currently over.
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
