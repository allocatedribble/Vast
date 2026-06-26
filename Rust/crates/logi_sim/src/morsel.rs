//! ADR-0014 — the Regime-H morsel engine (the flat-agent-loop CPU lane), spike form (Exp A).
//!
//! The gameplay hot loop is a flat, uniform, per-frame-barriered pass over the agent set. `par_iter` serves
//! it by spawning **one task per batch**; the morsel engine instead spawns **exactly `P` long-lived workers
//! that pull contiguous row ranges off ONE shared atomic cursor** (`fetch_add`). On the uniform path that is
//! zero per-morsel task objects and zero steals — the cursor itself is the schedule — while still
//! load-balancing (an idle worker just grabs the next morsel). Determinism is **by decomposition**: the
//! cursor tiles `[0, n)` into disjoint ranges, each row is processed exactly once, and each row's update is
//! independent — so the output is bit-identical regardless of worker count or completion order.
//!
//! **Spike status (Exp A):** operates on caller-owned scalar-column SoA buffers (the "split-AoS" layout
//! ADR-0014 calls for — `tx/ty/tz/vx/vy/vz` as separate stride-1 arrays). Wiring this over `bevy_ecs`
//! `Table` storage via `Table::get_data_slice_for` is the production follow-up; this spike validates the two
//! load-bearing claims first: (1) the cursor dispatch beats task-per-batch at fine granularity, and (2)
//! determinism survives morselization.

use crate::components::{Transform3d, Velocity3d};
use bevy_ecs::prelude::*;
use bevy_ecs::storage::TableId;
use bevy_tasks::ComputeTaskPool;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Apply `kernel` to every row in `[0, len)`, dispatched as morsels of `morsel` contiguous rows pulled by
/// `workers` persistent workers off one shared atomic cursor. `kernel(lo, hi)` processes the half-open range
/// `[lo, hi)` — and **must touch only those rows** (the disjointness the cursor guarantees is what makes the
/// concurrent writes sound and the result deterministic). Requires the global `ComputeTaskPool` to exist.
pub fn morsel_for_each<K>(len: usize, workers: usize, morsel: usize, kernel: K)
where
    K: Fn(usize, usize) + Sync,
{
    if len == 0 {
        return;
    }
    let workers = workers.max(1);
    let morsel = morsel.max(1);
    let cursor = AtomicUsize::new(0);
    let cursor_ref = &cursor;
    let kernel_ref = &kernel;
    // One scope; `P` persistent workers each drain the cursor to exhaustion. The calling thread also
    // participates (it drives the pool while awaiting), so the cursor is correct for any number of pullers.
    ComputeTaskPool::get().scope(|scope| {
        for _ in 0..workers {
            scope.spawn(async move {
                loop {
                    let lo = cursor_ref.fetch_add(morsel, Ordering::Relaxed);
                    if lo >= len {
                        break;
                    }
                    let hi = (lo + morsel).min(len);
                    (kernel_ref)(lo, hi);
                }
            });
        }
    });
}

/// Scalar-column SoA agent set — position in f64 cm, velocity in f32 cm/s, each a stride-1 array (the
/// layout ISPC/SIMD wants; ADR-0014's required refactor of today's AoS-in-column `Transform3d`).
#[derive(Clone)]
pub struct AgentSoa {
    pub tx: Vec<f64>,
    pub ty: Vec<f64>,
    pub tz: Vec<f64>,
    pub vx: Vec<f32>,
    pub vy: Vec<f32>,
    pub vz: Vec<f32>,
}

impl AgentSoa {
    #[inline]
    pub fn len(&self) -> usize {
        self.tx.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.tx.is_empty()
    }
}

/// Raw column bases, made `Send + Sync` so a `Fn` kernel can write disjoint ranges from many workers. Sound
/// only because the morsel cursor hands each invocation a non-overlapping `[lo, hi)`.
struct Cols {
    tx: *mut f64,
    ty: *mut f64,
    tz: *mut f64,
    vx: *const f32,
    vy: *const f32,
    vz: *const f32,
}
// SAFETY: the pointers are only ever dereferenced over disjoint row ranges (one worker per range, guaranteed
// by the atomic cursor), so there is no aliasing and no data race despite the shared `&Cols`.
unsafe impl Send for Cols {}
unsafe impl Sync for Cols {}

impl Cols {
    #[inline]
    fn of(soa: &mut AgentSoa) -> Self {
        Self {
            tx: soa.tx.as_mut_ptr(),
            ty: soa.ty.as_mut_ptr(),
            tz: soa.tz.as_mut_ptr(),
            vx: soa.vx.as_ptr(),
            vy: soa.vy.as_ptr(),
            vz: soa.vz.as_ptr(),
        }
    }
    /// `integrate_motion` (`systems.rs`) semantics for one row: skip stationary, else `pos += vel·dt`.
    #[inline]
    unsafe fn integrate_row(&self, i: usize, dt: f64) {
        let (vx, vy, vz) = (*self.vx.add(i), *self.vy.add(i), *self.vz.add(i));
        if vx != 0.0 || vy != 0.0 || vz != 0.0 {
            *self.tx.add(i) += vx as f64 * dt;
            *self.ty.add(i) += vy as f64 * dt;
            *self.tz.add(i) += vz as f64 * dt;
        }
    }
}

/// Serial reference — the golden order, identical per-row math to the parallel paths.
pub fn integrate_serial(soa: &mut AgentSoa, dt: f64) {
    let n = soa.len();
    let c = Cols::of(soa);
    // SAFETY: single-threaded; every index < n.
    unsafe {
        for i in 0..n {
            c.integrate_row(i, dt);
        }
    }
}

/// The morsel engine: `P` persistent workers + one cursor.
pub fn integrate_morsel(soa: &mut AgentSoa, dt: f64, workers: usize, morsel: usize) {
    let n = soa.len();
    let c = Cols::of(soa);
    let c = &c;
    morsel_for_each(n, workers, morsel, |lo, hi| {
        // SAFETY: `[lo, hi)` is disjoint across workers (the cursor never hands the same row twice) and all
        // indices are < n, so these writes neither alias nor race.
        unsafe {
            for i in lo..hi {
                c.integrate_row(i, dt);
            }
        }
    });
}

/// Task-per-batch baseline: mimics `par_iter`'s dispatch — **one spawned task per batch** rather than `P`
/// persistent cursor-workers. Same SoA, same kernel; isolates the dispatch model so the A/B measures exactly
/// "task object + queue push + join, per batch" vs "P workers + one fetch_add per morsel".
pub fn integrate_task_per_batch(soa: &mut AgentSoa, dt: f64, batch: usize) {
    let n = soa.len();
    if n == 0 {
        return;
    }
    let batch = batch.max(1);
    let c = Cols::of(soa);
    let c = &c;
    ComputeTaskPool::get().scope(|scope| {
        let mut lo = 0usize;
        while lo < n {
            let hi = (lo + batch).min(n);
            scope.spawn(async move {
                // SAFETY: each task owns a disjoint `[lo, hi)`; indices < n.
                unsafe {
                    for i in lo..hi {
                        c.integrate_row(i, dt);
                    }
                }
            });
            lo = hi;
        }
    });
}

/// Raw `bevy_ecs` column bases for one Table, made `Send + Sync` so a morsel kernel can mutate the
/// `Transform3d` column over disjoint row ranges from many workers. Sound under exclusive `&mut World`
/// access + the cursor's disjointness (see `integrate_motion_morsel_world`).
struct TableCols {
    trans: *const UnsafeCell<Transform3d>,
    vel: *const UnsafeCell<Velocity3d>,
}
// SAFETY: only ever dereferenced over disjoint row ranges (one worker per `[lo, hi)`), under a `&mut World`
// that guarantees no other reader/writer of these columns — so no aliasing, no data race.
unsafe impl Send for TableCols {}
unsafe impl Sync for TableCols {}

/// **The production-form morsel engine (Exp A, the `Table`-seam half):** `integrate_motion` over the REAL
/// `bevy_ecs` `Table` storage. It reads each matched table's `Transform3d`/`Velocity3d` columns directly via
/// the public `Table::get_data_slice_for` seam — no `par_iter`, no one-task-per-batch — and drives the
/// morsel cursor over the rows. Bit-identical to the `integrate_motion` system (proven by
/// `morsel_over_table_matches_par_iter_system`), so it is a drop-in for the flat-loop CPU lane (ADR-0014).
///
/// Takes `&mut World` (exclusive access is what makes the `UnsafeCell` column writes sound); a future
/// integration runs it as an exclusive system inside `FixedSim`.
pub fn integrate_motion_morsel_world(world: &mut World, dt: f64, workers: usize, morsel: usize) {
    let trans_id = world
        .component_id::<Transform3d>()
        .expect("Transform3d must be registered (spawn agents before integrating)");
    let vel_id = world
        .component_id::<Velocity3d>()
        .expect("Velocity3d must be registered (spawn agents before integrating)");

    // Find the tables holding both components (one archetype today, but handle many). The query state is used
    // ONLY to enumerate matched tables; the columns are read by component id directly.
    let mut qs = world.query::<(&Transform3d, &Velocity3d)>();
    qs.update_archetypes(world);
    let table_ids: Vec<TableId> = qs.matched_tables().collect();

    let storages = world.storages();
    for tid in table_ids {
        let Some(table) = storages.tables.get(tid) else {
            continue;
        };
        let n = table.entity_count() as usize;
        if n == 0 {
            continue;
        }
        // SAFETY: `Transform3d`/`Velocity3d` match `trans_id`/`vel_id`, and `n == entity_count()` rows exist.
        let trans = unsafe { table.get_data_slice_for::<Transform3d>(trans_id) }.expect("Transform3d column");
        let vel = unsafe { table.get_data_slice_for::<Velocity3d>(vel_id) }.expect("Velocity3d column");
        debug_assert_eq!(trans.len(), n);
        debug_assert_eq!(vel.len(), n);

        let cols = TableCols {
            trans: trans.as_ptr(),
            vel: vel.as_ptr(),
        };
        let cols = &cols;
        morsel_for_each(n, workers, morsel, |lo, hi| {
            // SAFETY: `[lo, hi)` is disjoint across workers (the cursor never hands a row twice); the
            // `Transform3d` and `Velocity3d` columns are distinct allocations (read vs write never alias);
            // exclusive `&mut World` rules out any other access; all indices are `< n`. The per-row math is
            // byte-identical to `integrate_motion` (`systems.rs`).
            unsafe {
                for i in lo..hi {
                    let v = &*(*cols.vel.add(i)).get();
                    if v.linear_cm_s != [0.0, 0.0, 0.0] {
                        let t = &mut *(*cols.trans.add(i)).get();
                        t.translation_cm[0] += f64::from(v.linear_cm_s[0]) * dt;
                        t.translation_cm[1] += f64::from(v.linear_cm_s[1]) * dt;
                        t.translation_cm[2] += f64::from(v.linear_cm_s[2]) * dt;
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_tasks::{ComputeTaskPool, TaskPoolBuilder};
    use std::time::Instant;

    /// Init a genuinely multi-threaded pool (best-effort: the singleton may already be set by another test in
    /// the binary — the bench reports the actual `thread_num()` so the numbers are self-documenting).
    fn pool_threads() -> usize {
        ComputeTaskPool::get_or_init(|| TaskPoolBuilder::new().num_threads(8).build());
        ComputeTaskPool::get().thread_num()
    }

    /// **Exp A — the `Table`-seam proof (the last uncertain claim).** The production-form morsel engine,
    /// reading REAL `bevy_ecs` `Table` columns via `get_data_slice_for`, must produce state **bit-identical**
    /// to the actual `integrate_motion` `par_iter` system on the same World — proving the SoA seam is usable
    /// from outside `bevy_ecs` on production storage, not just a caller-owned buffer.
    #[test]
    fn morsel_over_table_matches_par_iter_system() {
        use crate::components::{StableId, Transform3d, Velocity3d};
        use crate::resources::SimClock;
        use crate::schedules::LogiSchedule;
        use crate::systems::integrate_motion;
        use logi_schema::LogiEntityId;

        pool_threads();
        let n = 30_000u64;
        let clock = SimClock { tick: 0, fixed_hz: 30 };
        // Match integrate_motion's dt EXACTLY (it reads `SimClock::seconds_per_tick()` -> f32 -> f64).
        let dt = f64::from(clock.seconds_per_tick());

        let build = || -> World {
            let mut w = World::new();
            w.insert_resource(clock);
            for i in 0..n {
                let f = i as f32;
                let vel = if i % 7 != 0 {
                    [10.0 + f, -5.0 + 0.5 * f, 2.0]
                } else {
                    [0.0; 3]
                };
                w.spawn((
                    StableId(LogiEntityId {
                        index: i,
                        generation: 1,
                        shard: 0,
                        kind: 1,
                    }),
                    Transform3d {
                        translation_cm: [i as f64 * 137.0, -(i as f64), 3.0],
                        ..Default::default()
                    },
                    Velocity3d {
                        linear_cm_s: vel,
                        ..Default::default()
                    },
                ));
            }
            w
        };

        // Reference: the real `par_iter` `integrate_motion` via a schedule.
        let mut wa = build();
        let mut sched = Schedule::new(LogiSchedule::FixedSim);
        sched.add_systems(integrate_motion);
        sched.run(&mut wa);

        // Candidate: the morsel engine reading Table storage directly.
        let mut wb = build();
        integrate_motion_morsel_world(&mut wb, dt, 8, 512);

        // Compare per-entity translation bit-for-bit, sorted by stable id (order-independent).
        let collect = |w: &mut World| -> Vec<(u64, [u64; 3])> {
            let mut q = w.query::<(&StableId, &Transform3d)>();
            let mut v: Vec<(u64, [u64; 3])> = q
                .iter(w)
                .map(|(s, t)| {
                    let p = t.translation_cm;
                    (s.0.index, [p[0].to_bits(), p[1].to_bits(), p[2].to_bits()])
                })
                .collect();
            v.sort_by_key(|e| e.0);
            v
        };
        assert_eq!(
            collect(&mut wa),
            collect(&mut wb),
            "morsel-over-Table diverged from the integrate_motion par_iter system"
        );
    }

    /// Deterministic agent set: distinct non-zero velocities + spread positions (mirrors the golden test).
    fn make_agents(n: usize) -> AgentSoa {
        let mut s = AgentSoa {
            tx: vec![0.0; n],
            ty: vec![0.0; n],
            tz: vec![0.0; n],
            vx: vec![0.0; n],
            vy: vec![0.0; n],
            vz: vec![0.0; n],
        };
        for i in 0..n {
            let f = i as f32;
            s.tx[i] = i as f64 * 137.0;
            s.ty[i] = -(i as f64);
            s.tz[i] = 3.0;
            // Every 7th agent stationary, to exercise the skip branch deterministically.
            if i % 7 != 0 {
                s.vx[i] = 10.0 + f;
                s.vy[i] = -5.0 + 0.5 * f;
                s.vz[i] = 2.0;
            }
        }
        s
    }

    /// Bit-equality over the f64 position columns (compare bit patterns, not `==`, to catch any divergence).
    fn assert_bits_eq(a: &AgentSoa, b: &AgentSoa, ctx: &str) {
        assert_eq!(a.len(), b.len());
        for i in 0..a.len() {
            assert_eq!(a.tx[i].to_bits(), b.tx[i].to_bits(), "tx[{i}] differs ({ctx})");
            assert_eq!(a.ty[i].to_bits(), b.ty[i].to_bits(), "ty[{i}] differs ({ctx})");
            assert_eq!(a.tz[i].to_bits(), b.tz[i].to_bits(), "tz[{i}] differs ({ctx})");
        }
    }

    /// **Exp A — the correctness gate.** The morsel engine must produce state **bit-identical** to the serial
    /// golden reference, for ANY worker count and morsel size — and identical to the task-per-batch path. This
    /// is the morsel analogue of `parallel_execution_matches_single_threaded_bit_for_bit`.
    #[test]
    fn morsel_is_bit_identical_to_serial() {
        pool_threads();
        let n = 50_000usize;
        let dt = 1.0 / 30.0;

        let base = make_agents(n);
        let mut golden = base.clone();
        integrate_serial(&mut golden, dt);

        // Across worker counts AND morsel sizes — the result must not move a single bit.
        for &workers in &[1usize, 2, 3, 4, 8] {
            for &morsel in &[1usize, 17, 256, 4096] {
                let mut s = base.clone();
                integrate_morsel(&mut s, dt, workers, morsel);
                assert_bits_eq(&golden, &s, &format!("morsel workers={workers} morsel={morsel}"));
            }
        }
        // Task-per-batch path agrees too.
        for &batch in &[1usize, 256, 4096] {
            let mut s = base.clone();
            integrate_task_per_batch(&mut s, dt, batch);
            assert_bits_eq(&golden, &s, &format!("task-per-batch batch={batch}"));
        }
    }

    /// Multi-step determinism: 20 fixed steps (like the golden test's loop) stays bit-identical serial vs
    /// morsel — accumulated error would surface here if any step diverged.
    #[test]
    fn morsel_multistep_is_deterministic() {
        pool_threads();
        let n = 20_000usize;
        let dt = 0.0625;
        let base = make_agents(n);

        let mut golden = base.clone();
        let mut par = base.clone();
        for _ in 0..20 {
            integrate_serial(&mut golden, dt);
            integrate_morsel(&mut par, dt, 8, 512);
        }
        assert_bits_eq(&golden, &par, "20-step morsel vs serial");
    }

    /// **Exp A — the dispatch A/B.** Time the morsel cursor vs task-per-batch (same data, same kernel) across
    /// granularities, plus serial. The morsel engine's task count is fixed at `P`; task-per-batch's grows as
    /// `N/batch`, so its overhead should explode at fine granularity while morsel stays flat. Run with
    /// `--nocapture`.
    #[test]
    fn bench_morsel_vs_task_per_batch() {
        let threads = pool_threads();
        let n = 1_000_000usize;
        let dt = 1.0 / 30.0;
        let iters = 50usize;
        let warmup = 5usize;

        let base = make_agents(n);

        let time = |label: &str, mut f: Box<dyn FnMut(&mut AgentSoa)>| -> f64 {
            let mut samples = Vec::with_capacity(iters);
            for it in 0..(iters + warmup) {
                let mut s = base.clone();
                let t0 = Instant::now();
                f(&mut s);
                let e = t0.elapsed();
                std::hint::black_box(&s.tx[n - 1]);
                if it >= warmup {
                    samples.push(e.as_secs_f64() * 1e9 / n as f64); // ns / agent
                }
            }
            samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let med = samples[samples.len() / 2];
            let _ = label;
            med
        };

        eprintln!("\n=== Exp A: morsel cursor vs task-per-batch dispatch ({n} agents, pool threads = {threads}) ===");
        let serial = time("serial", Box::new(move |s| integrate_serial(s, dt)));
        eprintln!("  {:<32} {:>10.3} ns/agent", "serial (1 thread)", serial);

        for &m in &[256usize, 1024, 16384, 65536] {
            let morsel = time("morsel", Box::new(move |s| integrate_morsel(s, dt, threads, m)));
            let tpb = time("tpb", Box::new(move |s| integrate_task_per_batch(s, dt, m)));
            let tasks = n.div_ceil(m);
            eprintln!(
                "  granule={m:>6}: morsel(P workers) {morsel:>8.3} | task-per-batch({tasks:>5} tasks) {tpb:>8.3} ns/agent  | morsel {:>4.1}x vs tpb",
                tpb / morsel
            );
        }
        eprintln!("  → morsel ≈ flat in granule (P tasks always); task-per-batch worsens as N/batch tasks grow.\n");
    }
}
