//! `logi_runtime` — owns the Bevy `World`, the phased schedules, the stable
//! identity map, and the **triple-buffered snapshot ring**. Thread-safety policy
//! (§11): a single mutable world owner. The owning thread is captured at create;
//! every mutating FFI call must come from it. Representation reads the published,
//! immutable snapshot slot. We NEVER hand out pointers into Bevy component
//! storage — the snapshot is always copied into ABI-stable ring buffers, so
//! Unreal can't hold stale pointers after archetype moves / command application.

use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::*;
use hashbrown::{HashMap, HashSet};
use std::thread::ThreadId;

use logi_render_extract::{
    build_ffi_batches, extract_render_snapshot, LogiActorUpdate, LogiInstanceUpdate, LogiStaticBatch,
    RenderSnapshotResource,
};
#[cfg(feature = "editor_inspector")]
use logi_schema::LogiEntitySummary;
use logi_schema::{LogiCameraView, LogiEntityId, LogiPlayerIntent, LogiSpawnCommand, LOGI_FLAG_FORCE_SINGLE_THREADED};
use logi_sim::components::{
    Agent, AnimationDrive, CullState, Renderable, Representation, Shipment, Simulated, StableId, Transform3d,
    Velocity3d,
};
use logi_sim::resources::{
    RepresentationBudget, SignificanceIndex, SimClock, SimRng, TerrainResource, UnrealFrameInput,
};
use logi_sim::schedules::{run_if_populated, LogiSchedulerMode, LogiSchedules};
use logi_sim::terrain::Terrain;

/// Captured at creation (from the FFI `LogiInitDesc`).
#[derive(Clone, Copy, Debug)]
pub struct WorldConfig {
    pub fixed_step_hz: u32,
    pub deterministic_seed: u64,
    pub worker_thread_count: u32,
    pub flags: u32,
    pub high_res_actor_budget: u32,
    pub low_res_actor_budget: u32,
    pub instanced_mesh_budget: u32,
}

impl WorldConfig {
    fn scheduler_mode(&self) -> LogiSchedulerMode {
        // Multi-threaded by default; single-threaded only when explicitly forced
        // (strict replay / tests). Determinism comes from system ordering.
        if self.flags & LOGI_FLAG_FORCE_SINGLE_THREADED != 0 {
            LogiSchedulerMode::SingleThreadedDeterministic
        } else {
            LogiSchedulerMode::BoundedParallel
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

// --- Triple-buffered snapshot ring (§11) ------------------------------------

/// One published snapshot buffer. ABI-stable, owned copies (no Bevy pointers).
/// `ffi_batches` hold raw pointers into THIS slot's `instances` (valid until the
/// slot is reused, which round-robin guarantees is at least two ticks away).
struct SnapshotSlot {
    id: u64,
    sim_tick: u64,
    world_origin_cm: [f64; 3],
    instances: Vec<LogiInstanceUpdate>,
    ffi_batches: Vec<LogiStaticBatch>,
    actor_updates: Vec<LogiActorUpdate>,
    despawns: Vec<LogiEntityId>,
    retired: bool,
}

impl Default for SnapshotSlot {
    fn default() -> Self {
        Self {
            id: 0,
            sim_tick: 0,
            world_origin_cm: [0.0; 3],
            instances: Vec::new(),
            ffi_batches: Vec::new(),
            actor_updates: Vec::new(),
            despawns: Vec::new(),
            retired: true,
        }
    }
}

/// Triple buffer with an explicit reader fence (§11 + audit #9): writing /
/// published / retired. `publish` only writes into a slot the consumer has
/// retired AND that is not the currently-published "latest", so a buffer Unreal
/// may still be reading is never overwritten. If no slot is free the frame is
/// dropped (the prior published snapshot stays latest) and counted.
struct SnapshotRing {
    slots: [SnapshotSlot; 3],
    published: Option<usize>,
    dropped_frames: u64,
}

impl SnapshotRing {
    fn new() -> Self {
        Self {
            // All slots start `retired` (the `SnapshotSlot::default()` invariant) and `published` is
            // `None`, so the very first `publish` always finds a free slot and can never drop a frame
            // or overwrite a buffer Unreal is reading.
            slots: [
                SnapshotSlot::default(),
                SnapshotSlot::default(),
                SnapshotSlot::default(),
            ],
            published: None,
            dropped_frames: 0,
        }
    }

    /// A slot is safe to overwrite iff it is retired (consumer released it) AND it
    /// is not the slot currently served as "latest".
    fn acquire_free_slot(&self) -> Option<usize> {
        (0..self.slots.len()).find(|&i| self.slots[i].retired && Some(i) != self.published)
    }

    /// Copy the scratch snapshot into a free slot and publish it. Returns false
    /// (dropping + counting the frame) when the fence finds no free slot.
    fn publish(&mut self, id: u64, scratch: &RenderSnapshotResource) -> bool {
        let idx = match self.acquire_free_slot() {
            Some(i) => i,
            None => {
                self.dropped_frames = self.dropped_frames.wrapping_add(1);
                return false;
            }
        };
        {
            let slot = &mut self.slots[idx];
            slot.id = id;
            slot.sim_tick = scratch.sim_tick;
            slot.world_origin_cm = scratch.world_origin_cm;
            slot.instances.clear();
            slot.instances.extend_from_slice(&scratch.instances);
            slot.actor_updates.clear();
            slot.actor_updates.extend_from_slice(&scratch.actor_updates);
            slot.despawns.clear();
            slot.despawns.extend_from_slice(&scratch.despawns);
            slot.retired = false;

            // Build the pointer-bearing C batch array into THIS slot's instances via the single
            // shared marshaller (audit M24: no duplicated pointer-baking `unsafe` here). The metas
            // came from `scratch`, but the pointers must base off the SLOT's own copy, which is
            // stable until the slot is reused.
            let base = slot.instances.as_ptr();
            // SAFETY: `base` is this slot's instance buffer; every `scratch.batch_metas` offset/count
            // indexes the instances just copied from the same scratch (`extend_from_slice` above), so
            // the bounds match; the slot's `instances` outlives `ffi_batches` and is not reallocated
            // until this slot is reused (≥ 2 ticks away, guarded by the drop fence).
            unsafe { build_ffi_batches(base, &scratch.batch_metas, &mut slot.ffi_batches) };
        }
        self.published = Some(idx);
        true
    }

    fn published(&self) -> Option<&SnapshotSlot> {
        self.published.map(|i| &self.slots[i])
    }

    /// Look up a still-live snapshot slot by id (`0` ⇒ the currently-published "latest").
    /// A non-zero id resolves across ALL ring slots — not just the latest — so a consumer
    /// can still read a snapshot it has not yet released even after a newer one published
    /// (the ring keeps up to three live; audit M28). A retired slot is never returned.
    fn find(&self, id: u64) -> Option<&SnapshotSlot> {
        if id == 0 {
            return self.published();
        }
        self.slots.iter().find(|s| s.id == id && !s.retired)
    }

    /// Retire (mark free) the slot with `id`. Returns true if found.
    fn retire(&mut self, id: u64) -> bool {
        for slot in &mut self.slots {
            if slot.id == id && !slot.retired {
                slot.retired = true;
                return true;
            }
        }
        false
    }
}

/// A borrowed view of one live snapshot ring slot, returned by
/// [`LogiWorld::render_snapshot`]. The slices borrow the slot's owned buffers and stay valid as long
/// as the slot is neither retired (`logi_buffer_release`) nor reused by a later `publish` — see the
/// FFI `LogiRenderSnapshot` contract. No Bevy pointers; ABI-stable owned copies.
pub struct SnapshotView<'a> {
    pub id: u64,
    pub sim_tick: u64,
    pub world_origin_cm: [f64; 3],
    pub static_batches: &'a [LogiStaticBatch],
    pub actor_updates: &'a [LogiActorUpdate],
    pub despawns: &'a [LogiEntityId],
}

/// The whole simulation world for one shard (the §11 `Runtime`).
pub struct LogiWorld {
    owner_thread: ThreadId,
    world: World,
    schedules: LogiSchedules,
    ring: SnapshotRing,
    id_map: HashMap<LogiEntityId, Entity>,
    /// Every `LogiEntityId.index` currently live (both auto-allocated and
    /// externally-requested). `allocate_id` skips occupied indices so an auto id
    /// can never collide with a hand-authored one (audit M3); maintained in lockstep
    /// with `id_map` on spawn/despawn.
    used_indices: HashSet<u64>,
    next_index: u64,
    shard: u16,
    accumulator: f64,
    fixed_dt: f64,
    #[cfg(feature = "editor_inspector")]
    pinned: HashSet<LogiEntityId>,
    #[cfg(feature = "editor_inspector")]
    editor_query_buf: Vec<LogiEntitySummary>,
}

impl LogiWorld {
    pub fn new(config: WorldConfig) -> Self {
        let hz = if config.fixed_step_hz == 0 {
            30
        } else {
            config.fixed_step_hz
        };

        let mut world = World::new();
        world.insert_resource(SimClock { tick: 0, fixed_hz: hz });
        world.insert_resource(SimRng::new(config.deterministic_seed));
        // Terrain is the CANONICAL shared field (logi_geo_procgen::canonical) — the SAME surface
        // the renderer draws, so agents sit on the terrain you see. The terrain
        // is a fixed designed world; deterministic_seed drives agent RNG, not terrain geography.
        // (Supersedes audit M1's per-seed terrain: the holistic fix is one shared field, not two.)
        world.insert_resource(TerrainResource(Terrain::canonical()));
        world.insert_resource(UnrealFrameInput::default());
        // Representation budgets from config; all-zero => library defaults.
        let budget = if config.high_res_actor_budget == 0
            && config.low_res_actor_budget == 0
            && config.instanced_mesh_budget == 0
        {
            RepresentationBudget::default()
        } else {
            RepresentationBudget {
                high_res_actors: config.high_res_actor_budget,
                low_res_actors: config.low_res_actor_budget,
                instanced_meshes: config.instanced_mesh_budget,
            }
        };
        world.insert_resource(budget);
        // The persistent significance index the budget system ranks against (ADR-0020 R1);
        // grows lazily with the entity index space.
        world.insert_resource(SignificanceIndex::default());
        world.insert_resource(RenderSnapshotResource::default());

        let mut schedules = LogiSchedules::new(config.scheduler_mode(), config.worker_thread_count);
        schedules.render_extract.add_systems(extract_render_snapshot);

        Self {
            owner_thread: std::thread::current().id(),
            world,
            schedules,
            ring: SnapshotRing::new(),
            id_map: HashMap::new(),
            used_indices: HashSet::new(),
            next_index: 1,
            shard: 0,
            accumulator: 0.0,
            fixed_dt: 1.0 / hz as f64,
            #[cfg(feature = "editor_inspector")]
            pinned: HashSet::new(),
            #[cfg(feature = "editor_inspector")]
            editor_query_buf: Vec::new(),
        }
    }

    /// §11: the thread that created this world is the only one allowed to mutate it.
    pub fn is_owner_thread(&self) -> bool {
        std::thread::current().id() == self.owner_thread
    }

    /// Allocate the next auto id, SKIPPING any index already live (audit M3) so an
    /// auto-allocated id can never collide with an externally-supplied `requested_id`
    /// (which would orphan the prior entity). Deterministic: the live-index set is a
    /// pure function of the spawn sequence, so the same sequence yields the same ids.
    ///
    /// Index advance uses `checked_add` (audit Low): a bare `+=` would debug-panic
    /// (a panic must not cross the FFI) and silently wrap in release. On the
    /// astronomically-unreachable `u64` overflow we wrap the search back to 1 (never
    /// 0 — index 0 is the invalid/default sentinel); a free slot is then guaranteed
    /// unless 2^64 indices are live, which cannot happen.
    fn allocate_id(&mut self, kind: u16) -> LogiEntityId {
        while self.used_indices.contains(&self.next_index) {
            self.next_index = self.next_index.checked_add(1).unwrap_or(1);
        }
        let index = self.next_index;
        let id = LogiEntityId {
            index,
            generation: 1,
            shard: self.shard,
            kind,
        };
        self.next_index = self.next_index.checked_add(1).unwrap_or(1);
        id
    }

    /// Spawn one entity from a plain-data command (a controlled mutation phase).
    /// A `requested_id` that is already live is rejected as a no-op (audit M3):
    /// the existing id is returned and the live entity is left untouched, rather than
    /// inserting a second `id_map` entry that orphans the first.
    pub fn spawn(&mut self, cmd: &LogiSpawnCommand) -> LogiEntityId {
        self.spawn_inner(cmd).0
    }

    /// Spawn worker. Returns `(id, created)` where `created` is false when a valid
    /// `requested_id` was already present (de-duplicated, no entity spawned).
    fn spawn_inner(&mut self, cmd: &LogiSpawnCommand) -> (LogiEntityId, bool) {
        let id = if cmd.requested_id.is_valid() {
            // Reject a duplicate: return the live id without overwriting it.
            if self.id_map.contains_key(&cmd.requested_id) {
                return (cmd.requested_id, false);
            }
            cmd.requested_id
        } else {
            self.allocate_id(cmd.kind)
        };

        let transform = Transform3d {
            translation_cm: cmd.position,
            ..Default::default()
        };
        let entity = self
            .world
            .spawn((
                StableId(id),
                transform,
                Velocity3d::default(),
                Renderable::default(),
                Representation::default(),
                CullState::default(),
                // Animation-drive lane (audit M26): every spawned entity carries this so the
                // render-extract actor path treats the lane as live (its `Option<AnimationDrive>`
                // resolves to `Some` and carries the entity's `anim_graph` + locomotion params)
                // rather than always emitting defaults. The component starts at its inert default;
                // a locomotion system (not yet present) will write it as agents move.
                AnimationDrive::default(),
                Agent::default(),
                Shipment {
                    id: Default::default(),
                    payload_kg: cmd.payload_kg,
                },
                Simulated,
            ))
            .id();

        self.id_map.insert(id, entity);
        self.used_indices.insert(id.index);
        (id, true)
    }

    /// Despawn a live entity by stable id (a controlled mutation phase). Removes it
    /// from the Bevy world AND from `id_map` / `used_indices` (audit M3 / H1): leaving
    /// stale id-map entries would leak identity and freeing the index for reuse keeps
    /// auto-allocation tight. The render-extract retirement diff (H1) emits the
    /// despawn to Unreal on the next extract because the id drops out of the query.
    /// Returns true if an entity was removed.
    pub fn despawn(&mut self, id: LogiEntityId) -> bool {
        if let Some(entity) = self.id_map.remove(&id) {
            self.used_indices.remove(&id.index);
            self.world.despawn(entity);
            true
        } else {
            false
        }
    }

    /// Submit a batch of spawn commands. Returns how many entities were actually
    /// spawned (duplicates that hit an existing `requested_id` are not counted).
    pub fn submit_spawn_commands(&mut self, commands: &[LogiSpawnCommand]) -> u32 {
        let mut applied = 0u32;
        for cmd in commands {
            if self.spawn_inner(cmd).1 {
                applied += 1;
            }
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

    /// Advance one frame following the scheduling treaty. Publishes a new snapshot
    /// ring slot. Caller must be the owner thread (checked at the FFI boundary).
    ///
    /// `frame_delta_seconds` is wall-clock frame time and stays `f32` (L5): it only
    /// feeds the variable-rate accumulator that decides HOW MANY fixed steps to run.
    /// Per-step determinism is unaffected because every fixed step integrates with
    /// the constant tick dt (`self.fixed_dt`, derived from `fixed_step_hz`), never
    /// the frame delta — so 30/60/120 fps produce identical step counts and state.
    pub fn tick(
        &mut self,
        frame_delta_seconds: f32,
        max_fixed_steps: u32,
        cameras: &[LogiCameraView],
        intents: &[LogiPlayerIntent],
    ) -> TickOutcome {
        self.ingest_input(cameras, intents);
        // Reserved/empty phases are skipped (audit M25): `run_if_populated` runs a schedule only when
        // it has systems, so empty phases cost nothing per step but still run automatically once a
        // system is added. `ingest_input` already applied the FFI input above (the InputIngest
        // schedule is reserved for systems that derive from it).
        run_if_populated(&mut self.schedules.input_ingest, &mut self.world);

        // f64 accumulator; the f32 frame delta is widened once here. Determinism
        // comes from the constant `fixed_dt` per step, not this wall-clock input.
        self.accumulator += f64::from(frame_delta_seconds.max(0.0));
        let mut steps_run = 0u32;
        while self.accumulator >= self.fixed_dt && steps_run < max_fixed_steps {
            run_if_populated(&mut self.schedules.fixed_pre_sim, &mut self.world);
            run_if_populated(&mut self.schedules.fixed_sim, &mut self.world);
            run_if_populated(&mut self.schedules.fixed_post_sim, &mut self.world);
            increment_sim_tick(&mut self.world);
            self.accumulator -= self.fixed_dt;
            steps_run += 1;
        }

        let mut dropped = 0u32;
        while self.accumulator >= self.fixed_dt {
            self.accumulator -= self.fixed_dt;
            dropped += 1;
        }

        run_if_populated(&mut self.schedules.representation_plan, &mut self.world);
        run_if_populated(&mut self.schedules.render_extract, &mut self.world); // fills the scratch resource
        run_if_populated(&mut self.schedules.maintenance, &mut self.world);

        // Publish the scratch into a fresh ring slot (ABI-stable copy).
        let scratch_id = self.world.resource::<RenderSnapshotResource>().snapshot_id;
        let published = self
            .ring
            .publish(scratch_id, self.world.resource::<RenderSnapshotResource>());
        // Report the actually-published id (the prior one if the fence dropped this frame).
        let render_snapshot_id = self.snapshot_id();

        let tick = self.sim_tick();
        // Only retire change-detection trackers and advance the extract's cross-frame
        // bookkeeping when the snapshot actually published (audit H2). On a dropped
        // frame we keep the prior committed rendered-set / origin so the same
        // retirements + origin re-bake retry next tick, and we flag the extract to do
        // a FULL re-emit next frame to recover the lost instance/actor delta (Bevy's
        // per-system change detection has already consumed the Changed flags, so they
        // will NOT fire again — the full re-emit is what actually recovers the delta).
        if published {
            self.world.resource_mut::<RenderSnapshotResource>().commit();
            self.world.clear_trackers();
        } else {
            self.world
                .resource_mut::<RenderSnapshotResource>()
                .mark_publish_dropped();
        }

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

    /// Retire a published snapshot buffer (after Unreal has consumed it). §11.
    pub fn release_snapshot(&mut self, snapshot_id: u64) -> bool {
        self.ring.retire(snapshot_id)
    }

    /// Test-support seam for the CROSS-PROCESS determinism gate (`tests/parallel_determinism.rs`): seed
    /// every agent's velocity from its stable index so `integrate_motion` does varying real work. Hidden —
    /// not part of the runtime contract (the FFI spawn command carries no velocity), and an integration
    /// test cannot reach the private `world` the in-crate tests use for the same seeding.
    #[doc(hidden)]
    pub fn debug_seed_velocities(&mut self, f: impl Fn(u64) -> [f32; 3]) {
        use logi_sim::components::{StableId, Velocity3d};
        let mut q = self.world.query::<(&StableId, &mut Velocity3d)>();
        for (sid, mut vel) in q.iter_mut(&mut self.world) {
            vel.linear_cm_s = f(sid.0.index);
        }
    }

    // --- Published snapshot accessors (read by the FFI) ---

    pub fn snapshot_id(&self) -> u64 {
        self.ring.published().map(|s| s.id).unwrap_or(0)
    }

    pub fn snapshot_sim_tick(&self) -> u64 {
        self.ring.published().map(|s| s.sim_tick).unwrap_or(0)
    }

    /// Local-frame origin of the published snapshot (world cm); Unreal adds this
    /// back to the rebased f32 transforms (P0 #6).
    pub fn snapshot_world_origin(&self) -> [f64; 3] {
        self.ring.published().map(|s| s.world_origin_cm).unwrap_or([0.0; 3])
    }

    /// Count of frames the snapshot fence dropped (no free ring slot).
    pub fn dropped_snapshot_frames(&self) -> u64 {
        self.ring.dropped_frames
    }

    pub fn static_batches(&self) -> &[LogiStaticBatch] {
        self.ring.published().map(|s| s.ffi_batches.as_slice()).unwrap_or(&[])
    }

    pub fn actor_updates(&self) -> &[LogiActorUpdate] {
        self.ring.published().map(|s| s.actor_updates.as_slice()).unwrap_or(&[])
    }

    pub fn despawns(&self) -> &[LogiEntityId] {
        self.ring.published().map(|s| s.despawns.as_slice()).unwrap_or(&[])
    }

    /// Borrow a still-live snapshot by id (`0` ⇒ latest), resolved across ALL ring slots so a
    /// consumer can read a snapshot it has not yet released even after a newer one published
    /// (audit M28). Returns `None` if the id is unknown or already retired.
    pub fn render_snapshot(&self, id: u64) -> Option<SnapshotView<'_>> {
        let slot = self.ring.find(id)?;
        Some(SnapshotView {
            id: slot.id,
            sim_tick: slot.sim_tick,
            world_origin_cm: slot.world_origin_cm,
            static_batches: slot.ffi_batches.as_slice(),
            actor_updates: slot.actor_updates.as_slice(),
            despawns: slot.despawns.as_slice(),
        })
    }

    // --- Editor introspection (ADR-0006; `editor_inspector` feature only) ---

    /// Pin/unpin an entity for editor debug focus.
    #[cfg(feature = "editor_inspector")]
    pub fn editor_set_pin(&mut self, id: LogiEntityId, pinned: bool) {
        if pinned {
            self.pinned.insert(id);
        } else {
            self.pinned.remove(&id);
        }
    }

    /// Fill the editor query buffer with a page of entity summaries; returns the
    /// total entity count (for the editor's paging). Sampled (5-10 Hz), not hot.
    ///
    /// Single pass (audit M27): the page is pushed DIRECTLY into the reused
    /// `editor_query_buf` while the query is iterated once, and `total` is summed in
    /// the same walk — no separate count scan and no throwaway intermediate `Vec`.
    #[cfg(feature = "editor_inspector")]
    pub fn editor_query_entities(&mut self, offset: u32, limit: u32) -> u32 {
        use logi_sim::components::{Representation, StableId, Transform3d};
        // Build the QueryState first (needs `&mut self.world`; that borrow ends here), then split the
        // remaining borrows so the immutable iteration over `self.world` and the mutable push into the
        // disjoint `editor_query_buf` (reading the disjoint `pinned`) can coexist in one pass.
        let mut state = self.world.query::<(&StableId, &Transform3d, &Representation)>();
        let world = &self.world;
        let pinned = &self.pinned;
        let buf = &mut self.editor_query_buf;
        buf.clear();
        let (offset, limit) = (offset as usize, limit as usize);
        let mut total: u32 = 0;
        for (i, (sid, t, rep)) in state.iter(world).enumerate() {
            total = total.saturating_add(1);
            // Keep only the requested page; still count every row for paging.
            if i < offset || buf.len() >= limit {
                continue;
            }
            buf.push(LogiEntitySummary {
                id: sid.0,
                position_cm: t.translation_cm,
                significance: rep.significance,
                representation_tier: rep.current_tier as u8,
                pinned: u8::from(pinned.contains(&sid.0)),
                _pad: [0; 2],
            });
        }
        total
    }

    /// The page filled by the last [`editor_query_entities`] (borrowed view).
    #[cfg(feature = "editor_inspector")]
    pub fn editor_entities(&self) -> &[LogiEntitySummary] {
        &self.editor_query_buf
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

    // hz = 16 -> fixed_dt = 0.0625, exactly representable in f32/f64. Force the
    // single-threaded scheduler so determinism assertions are reproducible
    // (production defaults to the parallel executor).
    fn cfg(seed: u64) -> WorldConfig {
        WorldConfig {
            fixed_step_hz: 16,
            deterministic_seed: seed,
            worker_thread_count: 0,
            flags: LOGI_FLAG_FORCE_SINGLE_THREADED,
            high_res_actor_budget: 64,
            low_res_actor_budget: 256,
            instanced_mesh_budget: 100_000,
        }
    }

    fn spawn_cmd(index: u64, x: f64) -> LogiSpawnCommand {
        LogiSpawnCommand {
            kind: 1,
            requested_id: LogiEntityId {
                index,
                generation: 1,
                shard: 0,
                kind: 1,
            },
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
    fn owner_thread_is_creator() {
        let w = LogiWorld::new(cfg(1));
        assert!(w.is_owner_thread());
    }

    #[test]
    fn snapshot_ring_publishes_and_releases() {
        let mut w = LogiWorld::new(cfg(1));
        w.submit_spawn_commands(&[spawn_cmd(10, 1.0), spawn_cmd(11, 2.0)]);
        let a = w.tick(0.0625, 8, &[], &[]).render_snapshot_id;
        assert_eq!(w.snapshot_id(), a);
        assert!(w.release_snapshot(a)); // retire the published buffer
        assert!(!w.release_snapshot(a)); // already retired -> false
        let b = w.tick(0.0625, 8, &[], &[]).render_snapshot_id;
        assert!(b > a);
        assert_eq!(w.snapshot_id(), b);
    }

    /// M28: a still-live, non-latest snapshot id resolves across the ring (not just the
    /// published "latest"); id 0 means latest; a retired/unknown id resolves to None.
    #[test]
    fn render_snapshot_resolves_nonlatest_live_ids() {
        let mut w = LogiWorld::new(cfg(1));
        w.submit_spawn_commands(&[spawn_cmd(10, 1.0)]);
        // Publish A and B without releasing either — both slots stay live.
        let a = w.tick(0.0625, 8, &[], &[]).render_snapshot_id;
        let b = w.tick(0.0625, 8, &[], &[]).render_snapshot_id;
        assert_ne!(a, b);
        // id 0 == latest (B); the older A still resolves while live.
        assert_eq!(w.render_snapshot(0).map(|v| v.id), Some(b));
        assert_eq!(w.render_snapshot(a).map(|v| v.id), Some(a));
        assert_eq!(w.render_snapshot(b).map(|v| v.id), Some(b));
        // An unknown id resolves to None...
        assert!(w.render_snapshot(a.wrapping_add(b).wrapping_add(999)).is_none());
        // ...and a retired id no longer resolves.
        assert!(w.release_snapshot(a));
        assert!(w.render_snapshot(a).is_none());
    }

    #[test]
    fn snapshot_ring_fence_drops_when_unreleased() {
        let mut w = LogiWorld::new(cfg(2));
        w.submit_spawn_commands(&[spawn_cmd(20, 1.0)]);
        // Never release: once the 3 slots fill, the fence must drop further frames
        // and keep the last good snapshot available (never overwrite an unreleased
        // or currently-published buffer).
        let mut last_id = 0;
        for _ in 0..6 {
            last_id = w.tick(0.0625, 8, &[], &[]).render_snapshot_id;
        }
        assert!(w.dropped_snapshot_frames() >= 1);
        assert_ne!(last_id, 0);
        assert_eq!(w.snapshot_id(), last_id);
    }

    #[cfg(feature = "editor_inspector")]
    #[test]
    fn editor_query_pages_and_pins() {
        let mut w = LogiWorld::new(cfg(1));
        for i in 0..5 {
            w.spawn(&spawn_cmd(100 + i, i as f64));
        }
        // total = 5, page capped at 3.
        assert_eq!(w.editor_query_entities(0, 3), 5);
        assert_eq!(w.editor_entities().len(), 3);
        // Pin one entity; it reads back pinned on the next query.
        let id = w.editor_entities()[0].id;
        w.editor_set_pin(id, true);
        w.editor_query_entities(0, 10);
        assert_eq!(w.editor_entities().iter().filter(|e| e.pinned != 0).count(), 1);
    }

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

    /// A camera looking at the world origin, so an entity there earns positive
    /// significance and the representation plan promotes it to a rendered tier.
    fn cam_at_origin() -> LogiCameraView {
        LogiCameraView {
            position: [0.0, 0.0, 0.0],
            forward: [1.0, 0.0, 0.0],
            fov_degrees: 90.0,
            screen_height_px: 1080.0,
        }
    }

    /// H2: when the snapshot fence drops a frame, the render delta for an entity that
    /// changed during the drop is NOT lost — it is carried by the next published
    /// snapshot (because change-detection trackers are only cleared on a successful
    /// publish). We use the actor lane: a camera keeps the entity at a rendered tier
    /// across the whole test (the representation plan re-derives tiers every tick).
    #[test]
    fn dropped_publish_retries_render_delta() {
        let mut w = LogiWorld::new(cfg(7));
        let id = LogiEntityId {
            index: 50,
            generation: 1,
            shard: 0,
            kind: 1,
        };
        w.spawn(&spawn_cmd(50, 0.0));
        let cams = [cam_at_origin()];

        // Warm up (releasing every snapshot) until the hysteresis ramp settles the
        // entity at HighResActor and significance stabilizes — so later ticks produce
        // EMPTY deltas unless we deliberately mutate the entity.
        for _ in 0..8 {
            let sid = w.tick(0.0625, 8, &cams, &[]).render_snapshot_id;
            w.release_snapshot(sid);
        }
        assert_eq!(
            w.actor_updates().len(),
            0,
            "warm-up should have reached a stable empty delta"
        );

        // Fill the 3-slot ring WITHOUT releasing: A, B, C. (Stable -> empty deltas.)
        let a = w.tick(0.0625, 8, &cams, &[]).render_snapshot_id;
        let _b = w.tick(0.0625, 8, &cams, &[]).render_snapshot_id;
        let _c = w.tick(0.0625, 8, &cams, &[]).render_snapshot_id; // ring now full

        // Mutate the entity (nudge its transform), then tick: no free slot -> DROP.
        let entity = w.id_map[&id];
        w.world.get_mut::<Transform3d>(entity).unwrap().translation_cm[2] += 10.0;
        let dropped_before = w.dropped_snapshot_frames();
        w.tick(0.0625, 8, &cams, &[]);
        assert_eq!(w.dropped_snapshot_frames(), dropped_before + 1, "this frame must drop");

        // Free a non-published slot (release A) and tick again: the delta must survive
        // the drop and appear now. Without the H2 fix, trackers would have been cleared
        // on the dropped frame and this snapshot would be empty.
        assert!(w.release_snapshot(a));
        w.tick(0.0625, 8, &cams, &[]);
        let published = w.ring.published().unwrap();
        assert_eq!(
            published.actor_updates.len(),
            1,
            "changed entity must be re-emitted after the drop"
        );
        assert_eq!(published.actor_updates[0].entity.index, 50);
    }

    /// The terrain is the CANONICAL shared field — the same surface the renderer
    /// draws — so it deliberately does NOT vary with the world seed (a fixed designed world; the seed
    /// drives agent RNG, not terrain geography). Supersedes M1's per-seed terrain.
    #[test]
    fn terrain_is_the_canonical_shared_field() {
        use logi_schema::LogiTileId;
        let sample_world = |seed: u64| -> f64 {
            LogiWorld::new(cfg(seed))
                .world
                .resource::<TerrainResource>()
                .0
                .sample_surface(LogiTileId::new(1), 1234.0, 5678.0)
                .height_cm
        };
        // Different world seeds -> IDENTICAL ground (terrain is the shared canonical field, not seeded).
        assert_eq!(
            sample_world(1),
            sample_world(2),
            "terrain must be the canonical shared field, independent of seed"
        );
        // ...and it equals Terrain::canonical (the exact field the renderer renders).
        let canonical = Terrain::canonical()
            .sample_surface(LogiTileId::new(1), 1234.0, 5678.0)
            .height_cm;
        assert_eq!(
            sample_world(1),
            canonical,
            "sim terrain must equal the canonical shared field"
        );
    }

    /// M3: a duplicate `requested_id` is de-duplicated — the live entity is left
    /// intact, no second id_map entry is created, and the submit count excludes it.
    #[test]
    fn duplicate_requested_id_is_deduped() {
        let mut w = LogiWorld::new(cfg(1));
        let applied = w.submit_spawn_commands(&[spawn_cmd(30, 1.0), spawn_cmd(30, 9.0)]);
        assert_eq!(applied, 1, "the duplicate must not count as applied");
        assert_eq!(w.id_map.len(), 1, "no orphaned id_map entry");
        // The original entity (position x=1.0) is untouched by the duplicate (x=9.0).
        let entity = w.id_map[&LogiEntityId {
            index: 30,
            generation: 1,
            shard: 0,
            kind: 1,
        }];
        assert_eq!(w.world.get::<Transform3d>(entity).unwrap().translation_cm[0], 1.0);
    }

    /// M3: auto-allocated ids never collide with an externally-supplied index — the
    /// allocator skips occupied indices.
    #[test]
    fn auto_allocation_skips_occupied_indices() {
        let mut w = LogiWorld::new(cfg(1));
        // Externally claim index 1 (the value `next_index` starts at).
        let claimed = w.spawn(&spawn_cmd(1, 0.0));
        // Auto-allocate (invalid requested_id) -> must NOT reuse index 1.
        let auto = w.spawn(&LogiSpawnCommand {
            kind: 1,
            requested_id: LogiEntityId::default(), // invalid => auto
            position: [0.0; 3],
            route_id: 0,
            payload_kg: 0.0,
        });
        assert_ne!(auto.index, claimed.index, "auto id must skip the occupied index");
        assert_eq!(w.id_map.len(), 2, "both entities live, none orphaned");
    }

    /// M3 / H1: despawn removes the entity from id_map AND frees its index for reuse.
    #[test]
    fn despawn_clears_id_map_and_frees_index() {
        let mut w = LogiWorld::new(cfg(1));
        let id = w.spawn(&spawn_cmd(40, 1.0));
        assert_eq!(w.id_map.len(), 1);
        assert!(w.despawn(id), "despawn of a live id returns true");
        assert!(w.id_map.is_empty(), "id_map entry removed");
        assert!(!w.despawn(id), "despawning again is a no-op");
        // The index is free again: re-spawning the same requested id now succeeds.
        let again = w.spawn(&spawn_cmd(40, 2.0));
        assert_eq!(again.index, 40);
        assert_eq!(w.id_map.len(), 1);
    }

    /// Phase 4 safety net (§7): the parallel executor + `par_iter_mut` agent systems must produce state
    /// **bit-identical** to the single-threaded golden reference. The hot per-entity systems
    /// (`integrate_motion`, `compute_significance`) write only each entity's own components — no reduction,
    /// no cross-entity reads — so the result is independent of thread count / batch order. This runs the SAME
    /// scenario (moving agents + a camera, exercising motion, significance, and budgeting) under both modes
    /// and asserts equality; a regression that smuggles order-dependence into a `par_iter` system fails here.
    #[test]
    fn parallel_execution_matches_single_threaded_bit_for_bit() {
        use logi_sim::components::{Representation, StableId, Transform3d, Velocity3d};

        fn run(flags: u32) -> Vec<(u64, [f64; 3], f32, u8)> {
            let cfg = WorldConfig {
                fixed_step_hz: 16,
                deterministic_seed: 99,
                worker_thread_count: 8, // request a real multi-thread pool for the BoundedParallel run
                flags,
                high_res_actor_budget: 8,
                low_res_actor_budget: 32,
                instanced_mesh_budget: 1024,
            };
            let mut w = LogiWorld::new(cfg);
            let cmds: Vec<_> = (0..500u64).map(|i| spawn_cmd(i, i as f64 * 137.0)).collect();
            w.submit_spawn_commands(&cmds);
            // Give each agent a distinct non-zero velocity so `integrate_motion` does varying work.
            {
                let mut q = w.world.query::<(&StableId, &mut Velocity3d)>();
                for (sid, mut vel) in q.iter_mut(&mut w.world) {
                    let i = sid.0.index as f32;
                    vel.linear_cm_s = [10.0 + i, -5.0 + 0.5 * i, 2.0];
                }
            }
            // A camera off to the side so per-agent distance (→ significance → tier) varies.
            let cam = [LogiCameraView {
                position: [25_000.0, 0.0, 0.0],
                forward: [0.0, 1.0, 0.0],
                fov_degrees: 60.0,
                screen_height_px: 1080.0,
            }];
            for _ in 0..20 {
                w.tick(0.0625, 1, &cam, &[]);
            }
            // Final per-agent state, keyed + sorted by stable index (order-independent comparison).
            let mut state = w.world.query::<(&StableId, &Transform3d, &Representation)>();
            let mut out: Vec<(u64, [f64; 3], f32, u8)> = state
                .iter(&w.world)
                .map(|(s, t, r)| (s.0.index, t.translation_cm, r.significance, r.current_tier as u8))
                .collect();
            out.sort_by_key(|e| e.0);
            out
        }

        // The global `ComputeTaskPool` is a one-shot singleton: whichever test runs first fixes its size for
        // the whole process, so within THIS binary both arms inevitably share one pool (and, since the Exp-C
        // executor decision, one SingleThreaded system executor too) — making this an in-process SMOKE test,
        // not the real serial-vs-parallel comparison. The rigorous gate is the CROSS-PROCESS
        // `tests/parallel_determinism.rs` (re-eval 2026-06-09): its child process pins a genuine 1-thread
        // pool for the serial-fold arm, the parent hard-asserts a multi-thread pool, and the full per-tick
        // snapshot stream is digested. Keep this one as fast in-binary coverage of the scenario itself.
        bevy_tasks::ComputeTaskPool::get_or_init(|| bevy_tasks::TaskPoolBuilder::new().num_threads(4).build());

        let single = run(LOGI_FLAG_FORCE_SINGLE_THREADED);
        let parallel = run(0); // default BoundedParallel: MultiThreaded executor + pooled par_iter
        assert_eq!(single.len(), 500, "all agents present");
        assert_eq!(
            single, parallel,
            "parallel execution diverged from the single-threaded reference — replay determinism is broken"
        );
    }
}
