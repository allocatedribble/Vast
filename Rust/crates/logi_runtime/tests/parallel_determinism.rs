//! The CROSS-PROCESS determinism gate (re-eval 2026-06-09): parallel `par_iter` output must be
//! **bit-identical** to the strict-serial reference — the property that licenses every `par_iter_mut`
//! in the sim (AGENTS.md §7 safety net, ADR-0016 near-set determinism).
//!
//! Why a separate process: the global `ComputeTaskPool` is a process-wide `OnceLock`, so inside ONE test
//! process both arms inevitably share one pool size (first initializer wins — `cargo test` ordering decides
//! which), and since the Exp-C executor decision both scheduler modes also share the SingleThreaded system
//! executor. The in-crate `parallel_execution_matches_single_threaded_bit_for_bit` therefore CANNOT compare
//! serial-fold `par_iter` against parallel-fan `par_iter` — it silently degrades to comparing two identical
//! configurations. Here the SERIAL arm runs in a CHILD process whose fresh `OnceLock` is initialized to a
//! 1-thread pool (hard-asserted), the PARALLEL arm runs in this process on a 4-thread pool (hard-asserted),
//! and the two snapshot-stream digests must match exactly.

use logi_runtime::{LogiWorld, WorldConfig};
use logi_schema::{LogiCameraView, LogiEntityId, LogiSpawnCommand, LOGI_FLAG_FORCE_SINGLE_THREADED};

const CHILD_ENV: &str = "LOGI_DET_GATE_CHILD";
const AGENTS: u64 = 500;
const TICKS: u32 = 20;
const PARALLEL_POOL_THREADS: usize = 4;

/// Mirrors the in-crate test helper: a valid requested id (generation 1) at a per-agent x offset.
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

#[inline]
fn fold(digest: u64, v: u64) -> u64 {
    // FNV-1a fold over 8-byte words: cheap, dependency-free, and any single-bit divergence in any tick's
    // snapshot propagates to the final value.
    (digest ^ v).wrapping_mul(0x0000_0100_0000_01B3)
}

/// Run the standard moving-agents-plus-camera scenario and digest the FULL per-tick snapshot stream
/// (instances, actors, despawns) — exactly what replay equality means to the renderer host.
fn run_and_digest(flags: u32) -> u64 {
    let cfg = WorldConfig {
        fixed_step_hz: 16,
        deterministic_seed: 99,
        worker_thread_count: 8,
        flags,
        high_res_actor_budget: 8,
        low_res_actor_budget: 32,
        instanced_mesh_budget: 1024,
    };
    let mut w = LogiWorld::new(cfg);
    let cmds: Vec<_> = (0..AGENTS).map(|i| spawn_cmd(i, i as f64 * 137.0)).collect();
    let spawned = w.submit_spawn_commands(&cmds);
    assert_eq!(spawned as u64, AGENTS, "all spawn commands must apply");
    // Distinct non-zero velocities so `integrate_motion` does varying work per agent.
    w.debug_seed_velocities(|i| {
        let f = i as f32;
        [10.0 + f, -5.0 + 0.5 * f, 2.0]
    });
    let cam = [LogiCameraView {
        position: [25_000.0, 0.0, 0.0],
        forward: [0.0, 1.0, 0.0],
        fov_degrees: 60.0,
        screen_height_px: 1080.0,
    }];

    let mut digest: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for _ in 0..TICKS {
        w.tick(0.0625, 1, &cam, &[]);
        let (id, tick_digest) = {
            let view = w.render_snapshot(0).expect("tick must publish a snapshot");
            let mut d = digest;
            d = fold(d, view.sim_tick);
            for c in view.world_origin_cm {
                d = fold(d, c.to_bits());
            }
            for b in view.static_batches {
                d = fold(d, (u64::from(b.lod) << 32) | u64::from(b.instance_count));
                // SAFETY: `instances` points into the live ring slot's buffer with exactly
                // `instance_count` entries, valid until the slot is released/reused (the FFI contract
                // this view mirrors); we hold the borrow for the whole read.
                let inst = unsafe { std::slice::from_raw_parts(b.instances, b.instance_count as usize) };
                for i in inst {
                    d = fold(d, i.entity.index);
                    d = fold(d, (u64::from(i.stable_instance_slot) << 32) | u64::from(i.flags));
                    for f in i.transform_3x4 {
                        d = fold(d, u64::from(f.to_bits()));
                    }
                }
            }
            for a in view.actor_updates {
                d = fold(d, a.entity.index);
                d = fold(d, u64::from(a.significance.to_bits()));
                for f in a.transform_3x4 {
                    d = fold(d, u64::from(f.to_bits()));
                }
            }
            for dsp in view.despawns {
                d = fold(d, dsp.index);
            }
            (view.id, d)
        };
        digest = tick_digest;
        // Retire the slot so the ring never jams (3 unreleased publishes would start dropping frames and
        // the digest would silently stop covering later ticks).
        assert!(w.release_snapshot(id), "snapshot id must be live to release");
    }
    digest
}

#[test]
fn parallel_matches_serial_bit_for_bit_cross_process() {
    if std::env::var(CHILD_ENV).is_ok() {
        // CHILD = the strict-serial reference arm. Fresh process => fresh OnceLock: initialize the pool to
        // ONE thread before any world exists, so every `par_iter` provably takes its serial-fold path.
        bevy_tasks::ComputeTaskPool::get_or_init(|| bevy_tasks::TaskPoolBuilder::new().num_threads(1).build());
        assert_eq!(
            bevy_tasks::ComputeTaskPool::get().thread_num(),
            1,
            "serial arm: the 1-thread pool must have won the fresh OnceLock"
        );
        let digest = run_and_digest(LOGI_FLAG_FORCE_SINGLE_THREADED);
        // Parsed by the parent from captured stdout.
        println!("LOGI_DET_DIGEST={digest:016x}");
        return;
    }

    // PARENT = the parallel arm. Take the pool at a real multi-thread width and HARD-assert it stuck —
    // if another test in this binary ever wins the OnceLock with 1 thread, this gate must fail loudly,
    // not pass vacuously.
    bevy_tasks::ComputeTaskPool::get_or_init(|| {
        bevy_tasks::TaskPoolBuilder::new()
            .num_threads(PARALLEL_POOL_THREADS)
            .build()
    });
    assert!(
        bevy_tasks::ComputeTaskPool::get().thread_num() > 1,
        "parallel arm: the multi-thread pool must have won the OnceLock (got 1 — gate would be vacuous)"
    );
    let parallel_digest = run_and_digest(0);

    // Spawn the serial reference in a CHILD process (fresh OnceLock => genuine 1-thread pool).
    let exe = std::env::current_exe().expect("test binary path");
    let out = std::process::Command::new(exe)
        .args([
            "--exact",
            "parallel_matches_serial_bit_for_bit_cross_process",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .output()
        .expect("spawn serial-reference child process");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "serial-reference child failed:\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let serial_digest = stdout
        .lines()
        .find_map(|l| l.strip_prefix("LOGI_DET_DIGEST="))
        .map(|h| u64::from_str_radix(h.trim(), 16).expect("digest hex"))
        .expect("child must print LOGI_DET_DIGEST=");

    assert_eq!(
        parallel_digest, serial_digest,
        "parallel snapshot stream diverged from the strict-serial reference — replay determinism is broken"
    );
}
