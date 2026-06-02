//! `logi_runtime` — owns the Bevy `World`, the phased schedules, the stable
//! identity map, and the marshalled FFI batch array. The fixed-step accumulator
//! lives HERE; the frame flow follows the scheduling treaty (ADR-0005):
//! ingest -> [pre/sim/post]* -> representation plan -> render extract.
//! No Unreal types.

use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::*;
use std::collections::HashMap;

use logi_render_extract::{
	build_ffi_batches, extract_render_snapshot, LogiActorUpdate, LogiStaticBatch, RenderSnapshotResource,
};
use logi_schema::{
	LogiCameraView, LogiEntityId, LogiPlayerIntent, LogiSpawnCommand, LOGI_FLAG_BOUNDED_PARALLEL,
};
use logi_sim::components::{
	Agent, CullState, Renderable, Representation, Shipment, Simulated, StableId, Transform3d, Velocity3d,
};
use logi_sim::resources::{RepresentationBudget, SimClock, SimRng, TerrainResource, UnrealFrameInput};
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
	/// Pointer-bearing C batch array, rebuilt each tick from RenderSnapshotResource.
	ffi_batches: Vec<LogiStaticBatch>,
	id_map: HashMap<LogiEntityId, Entity>,
	next_index: u64,
	shard: u16,
	accumulator: f64,
	fixed_dt: f64,
}

impl LogiWorld {
	pub fn new(config: WorldConfig) -> Self {
		let hz = if config.fixed_step_hz == 0 { 30 } else { config.fixed_step_hz };

		let mut world = World::new();
		world.insert_resource(SimClock { tick: 0, fixed_hz: hz });
		world.insert_resource(SimRng::new(config.deterministic_seed));
		world.insert_resource(TerrainResource::default());
		world.insert_resource(UnrealFrameInput::default());
		world.init_resource::<RepresentationBudget>();
		world.insert_resource(RenderSnapshotResource::default());

		let mut schedules = LogiSchedules::new(config.scheduler_mode(), config.worker_thread_count);
		// The render-extract system lives in logi_render_extract (which depends on
		// logi_sim), so it is wired in here where both crates are available.
		schedules.render_extract.add_systems(extract_render_snapshot);

		Self {
			world,
			schedules,
			ffi_batches: Vec::new(),
			id_map: HashMap::new(),
			next_index: 1,
			shard: 0,
			accumulator: 0.0,
			fixed_dt: 1.0 / hz as f64,
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
		self.schedules.render_extract.run(&mut self.world); // fills RenderSnapshotResource
		self.schedules.maintenance.run(&mut self.world);

		// Marshal the pointer-bearing batch array for the FFI from the resource.
		build_ffi_batches(self.world.resource::<RenderSnapshotResource>(), &mut self.ffi_batches);

		let render_snapshot_id = self.world.resource::<RenderSnapshotResource>().snapshot_id;
		let tick = self.sim_tick();

		// Per-frame tracker cleanup (clears removal buffers; App-equivalent).
		self.world.clear_trackers();

		TickOutcome {
			fixed_steps_run: steps_run,
			dropped_steps: dropped,
			sim_tick: tick,
			render_snapshot_id,
		}
	}

	/// Current fixed-step tick.
	pub fn sim_tick(&self) -> u64 {
		self.world.resource::<SimClock>().tick
	}

	// --- Render snapshot accessors (read by the FFI; valid until next tick) ---

	pub fn snapshot_id(&self) -> u64 {
		self.world.resource::<RenderSnapshotResource>().snapshot_id
	}

	pub fn snapshot_sim_tick(&self) -> u64 {
		self.world.resource::<RenderSnapshotResource>().sim_tick
	}

	pub fn static_batches(&self) -> &[LogiStaticBatch] {
		&self.ffi_batches
	}

	pub fn actor_updates(&self) -> &[LogiActorUpdate] {
		&self.world.resource::<RenderSnapshotResource>().actor_updates
	}

	pub fn despawns(&self) -> &[LogiEntityId] {
		&self.world.resource::<RenderSnapshotResource>().despawns
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

	fn spawn_cmd(index: u64, x: f64) -> LogiSpawnCommand {
		LogiSpawnCommand {
			kind: 1,
			requested_id: LogiEntityId { index, generation: 1, shard: 0, kind: 1 },
			position: [x, 0.0, 0.0],
			route_id: 1,
			payload_kg: 1.0,
		}
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
	fn spawn_assigns_valid_ids_and_advances_snapshot() {
		let mut w = LogiWorld::new(cfg(1));
		let applied = w.submit_spawn_commands(&[spawn_cmd(10, 1.0), spawn_cmd(11, 2.0)]);
		assert_eq!(applied, 2);
		let id = w.spawn(&LogiSpawnCommand { requested_id: LogiEntityId::default(), ..spawn_cmd(0, 3.0) });
		assert!(id.is_valid());
		// A render snapshot id is produced each tick.
		let a = w.tick(0.0625, 8, &[], &[]).render_snapshot_id;
		let b = w.tick(0.0625, 8, &[], &[]).render_snapshot_id;
		assert!(b > a);
	}

	/// Determinism rule 7: the fixed-step count is identical regardless of how the
	/// same total time is chunked into frames (30 / 60 / 120 fps analogue).
	#[test]
	fn sim_tick_equal_across_frame_rates() {
		fn run(delta: f32, frames: u32) -> u64 {
			let mut w = LogiWorld::new(cfg(123));
			w.spawn(&spawn_cmd(1, 5.0));
			for _ in 0..frames {
				w.tick(delta, 64, &[], &[]);
			}
			w.sim_tick()
		}
		assert_eq!(run(1.0 / 16.0, 16), 16);
		assert_eq!(run(1.0 / 16.0, 16), run(1.0 / 32.0, 32));
		assert_eq!(run(1.0 / 32.0, 32), run(1.0 / 64.0, 64));
	}
}
