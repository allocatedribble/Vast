//! Exp C (spike) — the `bevy_ecs` scheduler overhead at our scale.
//!
//! ADR-0014's Regime-S claim is that a **precomputed-stage executor** (compile the conflict graph into
//! barrier-separated stages once, then walk them) beats the stock `MultiThreadedExecutor` — whose per-tick
//! cost is a single `Mutex<ExecutorState>` decide-loop + a per-system OS-thread wakeup (bevy issue #11378).
//! That executor must be vendored (its conflict bitsets are `pub(crate)`), so before paying that cost we
//! measure, with public APIs only, **(a)** how the stock executor's per-tick overhead grows with system
//! count, vs `SingleThreaded`, and **(b)** a "manual single-stage scope" lower bound — `P` tasks + one
//! barrier, which is what a precomputed-stage executor approaches for a wave of conflict-free systems. The
//! gap (b)↔stock-MultiThreaded is the headroom the vendored executor would capture.
//!
//! Pure `bevy_ecs` + `bevy_tasks`; no fork, no new deps. Run:
//!   cargo test -p logi_sim --release --test sched_overhead -- --nocapture

use bevy_ecs::prelude::*;
use bevy_ecs::schedule::{MultiThreadedExecutor, ScheduleLabel, SingleThreadedExecutor};
use bevy_tasks::{ComputeTaskPool, TaskPoolBuilder};
use std::time::Instant;

#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
struct Bench;

/// A small fixed amount of CPU work shared by every system instance.
#[derive(Resource, Clone, Copy)]
struct Spin {
    iters: u64,
    seed: u64,
}

/// A **read-only** system: every instance is mutually access-compatible, so the `MultiThreaded` executor is
/// free to run them all concurrently (the best case) — which isolates its per-system *dispatch* overhead
/// (spawn + the decide-loop + completion wakeup), the thing #11378 measures.
fn spin(load: Res<Spin>) {
    std::hint::black_box(do_spin(load.iters, load.seed));
}

#[inline]
fn do_spin(iters: u64, seed: u64) -> u64 {
    let mut acc = seed;
    for k in 0..iters {
        acc = acc
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(k ^ 1_442_695_040_888_963_407);
    }
    acc
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// Time `frames` runs of `f` (drop `warmup`), returning the median per-tick µs.
fn time_per_tick(frames: usize, warmup: usize, mut f: impl FnMut()) -> f64 {
    let mut s = Vec::with_capacity(frames);
    for i in 0..(frames + warmup) {
        let t0 = Instant::now();
        f();
        let dt = t0.elapsed();
        if i >= warmup {
            s.push(dt.as_secs_f64() * 1e6);
        }
    }
    median(s)
}

#[test]
fn bench_scheduler_overhead_vs_system_count() {
    let threads = {
        ComputeTaskPool::get_or_init(|| TaskPoolBuilder::new().num_threads(8).build());
        ComputeTaskPool::get().thread_num()
    };
    const ITERS: u64 = 300; // per-system work: cheap, so executor overhead is visible but not dominant
    const FRAMES: usize = 200;
    const WARMUP: usize = 20;

    eprintln!("\n=== Exp C: bevy_ecs scheduler per-tick overhead vs system count (pool threads = {threads}, {ITERS} iters/sys) ===");
    eprintln!(
        "  {:>5} | {:>12} | {:>12} | {:>14} | {:>14}",
        "N", "single (µs)", "multi (µs)", "manual-stage", "multi/stage"
    );

    for &n in &[4usize, 16, 64, 256, 1024] {
        // --- stock SingleThreaded executor ---
        let single = {
            let mut world = World::new();
            world.insert_resource(Spin {
                iters: ITERS,
                seed: 0x1234,
            });
            let mut sched = Schedule::new(Bench);
            sched.set_executor(SingleThreadedExecutor::new());
            for _ in 0..n {
                sched.add_systems(spin);
            }
            sched.run(&mut world); // build/init once
            time_per_tick(FRAMES, WARMUP, || sched.run(&mut world))
        };

        // --- stock MultiThreaded executor (the single-Mutex decide-loop) ---
        let multi = {
            let mut world = World::new();
            world.insert_resource(Spin {
                iters: ITERS,
                seed: 0x1234,
            });
            let mut sched = Schedule::new(Bench);
            sched.set_executor(MultiThreadedExecutor::new());
            for _ in 0..n {
                sched.add_systems(spin);
            }
            sched.run(&mut world);
            time_per_tick(FRAMES, WARMUP, || sched.run(&mut world))
        };

        // --- manual single-stage scope: P tasks + one barrier (the precomputed-stage lower bound for a
        //     wave of conflict-free systems) — no executor, no decide-loop, no per-system wakeup. The n
        //     "systems" are run as n closures over the pool via one `scope`. ---
        let stage = {
            let pool = ComputeTaskPool::get();
            time_per_tick(FRAMES, WARMUP, || {
                pool.scope(|s| {
                    for i in 0..n {
                        s.spawn(async move {
                            std::hint::black_box(do_spin(ITERS, 0x1234 ^ i as u64));
                        });
                    }
                });
            })
        };

        eprintln!(
            "  {n:>5} | {single:>12.2} | {multi:>12.2} | {stage:>14.2} | {:>13.1}x",
            multi / stage
        );
    }
    eprintln!("  → multi-vs-stage gap = the per-tick overhead (decide-loop + per-system wakeups) a precomputed-stage executor would remove.\n");
}
