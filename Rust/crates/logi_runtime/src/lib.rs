//! `logi_runtime` — owns the Bevy `World`, the fixed-step `Schedule`, and the
//! reusable render snapshot. This is the orchestrator the FFI wraps. No Unreal
//! types here; this crate is engine-agnostic and headless-testable.

use bevy_ecs::prelude::*;
use logi_render_extract::{extract_agents, RenderSnapshot};
use logi_schema::{AgentSpawnRecipe, LogiShipmentId, Vec3};
use logi_sim::components::{Agent, Position, Shipment, Simulated, Velocity};
use logi_sim::resources::{SimClock, SimRng, TerrainResource};
use logi_sim::schedules::build_fixed_step_schedule;

/// The whole simulation runtime: `World` + fixed-step `Schedule` + snapshot buffer.
pub struct LogiRuntime {
    world: World,
    schedule: Schedule,
    snapshot: RenderSnapshot,
}

impl LogiRuntime {
    /// Create a runtime seeded deterministically.
    pub fn new(seed: u64) -> Self {
        let mut world = World::new();
        world.insert_resource(SimClock::default());
        world.insert_resource(SimRng::new(seed));
        world.insert_resource(TerrainResource::default());

        Self {
            world,
            schedule: build_fixed_step_schedule(),
            snapshot: RenderSnapshot::default(),
        }
    }

    /// Spawn one agent from a plain-data recipe; returns its entity bits.
    pub fn spawn_agent(&mut self, recipe: AgentSpawnRecipe) -> u64 {
        self.world
            .spawn((
                Agent { id: recipe.agent, route: recipe.route, tile: recipe.tile },
                Position(recipe.position),
                Velocity(Vec3::ZERO),
                Shipment { id: LogiShipmentId::default(), payload_kg: recipe.payload_kg },
                Simulated,
            ))
            .id()
            .to_bits()
    }

    /// Advance the deterministic sim by `steps` fixed ticks.
    pub fn tick(&mut self, steps: u32) {
        for _ in 0..steps {
            self.schedule.run(&mut self.world);
        }
    }

    /// Re-extract the render snapshot and return a reference to it.
    pub fn extract(&mut self) -> &RenderSnapshot {
        extract_agents(&mut self.world, &mut self.snapshot);
        &self.snapshot
    }

    /// Current fixed-step tick.
    pub fn tick_count(&self) -> u64 {
        self.world.resource::<SimClock>().tick
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_are_deterministic_and_monotonic() {
        let mut rt = LogiRuntime::new(42);
        assert_eq!(rt.tick_count(), 0);
        rt.tick(10);
        assert_eq!(rt.tick_count(), 10);
    }

    #[test]
    fn spawn_then_extract_sees_the_agent() {
        let mut rt = LogiRuntime::new(1);
        rt.spawn_agent(AgentSpawnRecipe::default());
        let snap = rt.extract();
        assert_eq!(snap.agents.len(), 1);
    }
}
