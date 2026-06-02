//! `logi_runtime` — owns the Bevy `World`, the phased schedules, the stable
//! identity map, and the reusable render snapshot. The fixed-step accumulator
//! lives HERE; the frame flow follows the scheduling treaty (ADR-0005):
//! ingest -> [pre/sim/post]* -> representation plan -> render extract.
//! No Unreal types.

use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::*;
use std::collections::HashMap;

use logi_render_extract::{extract_agents, RenderSnapshot};
use logi_schema::{
	LogiCameraView, LogiEntityId, LogiPlayerIntent, LogiSpawnCommand, LOGI_FLAG_BOUNDED_PARALLEL,
};
use logi_sim::components::{
	Agent, CullState, Renderable, Representation, Shipment, Simulated, StableId, Transform3d, Velocity3d,
};
use logi_sim::resources::{SimClock, SimRng, TerrainResource, UnrealFrameInput};
use logi_sim::schedules::{LogiSchedulerMode, LogiSchedules};

/// Captured at creation (from the FFI `LogiInitDesc`).
#[derive(Clone, Copy, Debug)]
pub struct WorldConfig {
	pub fixed_step_hz: u32,
	pub deterministic_seed: u64,
	pub worker_thread_count: u32,
	pub flags: u32,
}

impl WorldConfig {
	fn scheduler_mode(&self) -> LogiSchedulerMode {
		if self.flags & LOGI_FLAG_BOUNDED_PARALLEL != 0 {
			LogiSchedulerMode::BoundedParallel
		} else {
			LogiSchedulerMode::SingleThreadedDeterministic
		}
	}
}

/// Outcome of a tick (maps to the FFI `LogiTickResult`).
#[derive(Clone, Copy, Debug, Default)]
pub struct TickOutcome {
	pub fixed_steps_run: u32,
	pub dropped_steps: u32,
	pub sim_tick: u64,
	pub render_snapshot_id: u64,
}

/// The whole simulation world for one shard.
pub struct LogiWorld {
	world: World,
	schedules: LogiSchedules,
	snapshot: RenderSnapshot,
	id_map: HashMap<LogiEntityId, Entity>,
	next_index: u64,
	shard: u16,
	accumulator: f64,
	fixed_dt: f64,
	snapshot_counter: u64,
}

impl LogiWorld {
	pub fn new(config: WorldConfig) -> Self {
		let hz = if config.fixed_step_hz == 0 { 30 } else { config.fixed_step_hz };

		let mut world = World::new();
		world.insert_resource(SimClock { tick: 0, fixed_hz: hz });
		world.insert_resource(SimRng::new(config.deterministic_seed));
		world.insert_resource(TerrainResource::default());
		world.insert_resource(UnrealFrameInput::default());
		world.init_resource::<logi_sim::resources::RepresentationBudget>();

		Self {
			world,
			schedules: LogiSchedules::new(config.scheduler_mode(), config.worker_thread_count),
			snapshot: RenderSnapshot::default(),
			id_map: HashMap::new(),
			next_index: 1,
			shard: 0,
			accumulator: 0.0,
			fixed_dt: 1.0 / hz as f64,
			snapshot_counter: 0,
		}
	}

	fn allocate_id(&mut self, kind: u16) -> LogiEntityId {
		let id = LogiEntityId { index: self.next_index, generation: 1, shard: self.shard, kind };
		self.next_index += 1;
		id
	}

	/// Spawn one entity from a plain-data command (a controlled mutation phase).
	pub fn spawn(&mut self, cmd: &LogiSpawnCommand) -> LogiEntityId {
		let id = if cmd.requested_id.is_valid() {
			cmd.requested_id
		} else {
			self.allocate_id(cmd.kind)
		};

		let transform = Transform3d { translation_cm: cmd.position, ..Default::default() };
		let entity = self
			.world
			.spawn((
				StableId(id),
				transform,
				Velocity3d::default(),
				Renderable::default(),
				Representation::default(),
				CullState::default(),
				Agent::default(),
				Shipment { id: Default::default(), payload_kg: cmd.payload_kg },
				Simulated,
			))
			.id();

		self.id_map.insert(id, entity);
		id
	}

	/// Submit a batch of spawn commands. Returns how many were applied.
	pub fn submit_spawn_commands(&mut self, commands: &[LogiSpawnCommand]) -> u32 {
		let mut applied = 0u32;
		for cmd in commands {
			self.spawn(cmd);
			applied += 1;
		}
		applied
	}

	fn ingest_input(&mut self, cameras: &[LogiCameraView], intents: &[LogiPlayerIntent]) {
		let mut input = self.world.resource_mut::<UnrealFrameInput>();
		input.cameras.clear();
		input.cameras.extend_from_slice(cameras);
		input.intents.clear();
		input.intents.extend_from_slice(intents);
	}

	/// Advance one frame following the scheduling treaty. The fixed-step
	/// accumulator is internal; `max_fixed_steps` clamps catch-up.
	pub fn tick(
		&mut self,
		frame_delta_seconds: f32,
		max_fixed_steps: u32,
		cameras: &[LogiCameraView],
		intents: &[LogiPlayerIntent],
	) -> TickOutcome {
		self.ingest_input(cameras, intents);
		self.schedules.input_ingest.run(&mut self.world);

		self.accumulator += f64::from(frame_delta_seconds.max(0.0));
		let mut steps_run = 0u32;
		while self.accumulator >= self.fixed_dt && steps_run < max_fixed_steps {
			self.schedules.fixed_pre_sim.run(&mut self.world);
			self.schedules.fixed_sim.run(&mut self.world);
			self.schedules.fixed_post_sim.run(&mut self.world);
			increment_sim_tick(&mut self.world);
			self.accumulator -= self.fixed_dt;
			steps_run += 1;
		}

		let mut dropped = 0u32;
		while self.accumulator >= self.fixed_dt {
			self.accumulator -= self.fixed_dt;
			dropped += 1;
		}

		self.schedules.representation_plan.run(&mut self.world);
		self.schedules.render_extract.run(&mut self.world);
		self.schedules.maintenance.run(&mut self.world);

		self.snapshot_counter += 1;
		extract_agents(&mut self.world, &mut self.snapshot);
		let tick = self.sim_tick();
		self.snapshot.snapshot_id = self.snapshot_counter;
		self.snapshot.sim_tick = tick;

		TickOutcome {
			fixed_steps_run: steps_run,
			dropped_steps: dropped,
			sim_tick: tick,
			render_snapshot_id: self.snapshot_counter,
		}
	}

	/// Current fixed-step tick.
	pub fn sim_tick(&self) -> u64 {
		self.world.resource::<SimClock>().tick
	}

	/// The latest snapshot if `requested_id` is 0 (latest) or matches it.
	/// Borrowed view, valid only until the next `tick`.
	pub fn snapshot_view(&self, requested_id: u64) -> Option<&RenderSnapshot> {
		if requested_id == 0 || requested_id == self.snapshot.snapshot_id {
			Some(&self.snapshot)
		} else {
			None
		}
	}
}

/// Advance the authoritative tick counter (one explicit step between phases).
fn increment_sim_tick(world: &mut World) {
	let mut clock = world.resource_mut::<SimClock>();
	clock.tick = clock.tick.wrapping_add(1);
}

#[cfg(test)]
mod tests {
	use super::*;

	// hz = 16 -> fixed_dt = 0.0625, exactly representable in f32/f64.
	fn cfg(seed: u64) -> WorldConfig {
		WorldConfig { fixed_step_hz: 16, deterministic_seed: seed, worker_thread_count: 0, flags: 0 }
	}

	#[test]
	fn one_second_runs_sixteen_fixed_steps() {
		let mut w = LogiWorld::new(cfg(42));
		let out = w.tick(1.0, 64, &[], &[]);
		assert_eq!(out.fixed_steps_run, 16);
		assert_eq!(out.dropped_steps, 0);
		assert_eq!(w.sim_tick(), 16);
	}

	#[test]
	fn max_fixed_steps_clamps_and_reports_dropped() {
		let mut w = LogiWorld::new(cfg(7));
		let out = w.tick(1.0, 10, &[], &[]);
		assert_eq!(out.fixed_steps_run, 10);
		assert_eq!(out.dropped_steps, 6);
	}

	#[test]
	fn spawn_appears_in_snapshot_with_stable_id() {
		let mut w = LogiWorld::new(cfg(1));
		let cmd = LogiSpawnCommand {
			kind: 1,
			requested_id: LogiEntityId::default(),
			position: [10.0, 0.0, 0.0],
			route_id: 1,
			payload_kg: 5.0,
		};
		let id = w.spawn(&cmd);
		assert!(id.is_valid());
		w.tick(0.0625, 8, &[], &[]); // exactly one fixed step
		let snap = w.snapshot_view(0).expect("latest snapshot");
		assert_eq!(snap.agents.len(), 1);
		assert_eq!(snap.agents[0].id, id);
		assert_eq!(snap.agents[0].translation_cm, [10.0, 0.0, 0.0]);
	}

	/// Determinism rule 7: the fixed-step result is identical regardless of how
	/// the same total time is chunked into frames (30 / 60 / 120 fps analogue).
	#[test]
	fn snapshot_equal_across_frame_rates() {
		fn run(delta: f32, frames: u32) -> (u64, Vec<(u64, [f64; 3])>) {
			let mut w = LogiWorld::new(cfg(123));
			w.spawn(&LogiSpawnCommand {
				kind: 1,
				requested_id: LogiEntityId::default(),
				position: [1.0, 2.0, 3.0],
				route_id: 1,
				payload_kg: 1.0,
			});
			for _ in 0..frames {
				w.tick(delta, 64, &[], &[]);
			}
			let snap = w.snapshot_view(0).expect("snapshot");
			let agents = snap.agents.iter().map(|a| (a.id.index, a.translation_cm)).collect();
			(w.sim_tick(), agents)
		}

		let a = run(1.0 / 16.0, 16);
		let b = run(1.0 / 32.0, 32);
		let c = run(1.0 / 64.0, 64);
		assert_eq!(a.0, 16);
		assert_eq!(a.0, b.0);
		assert_eq!(b.0, c.0);
		assert_eq!(a.1, b.1);
		assert_eq!(b.1, c.1);
	}
}
