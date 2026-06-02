//! Headless harnesses for the terrain kernel — chiefly the **brutal streaming stress test**
//! (`terrain-doctrine.md` §1): 100k+ logical tiles, network jitter, a hard memory budget, camera
//! teleportation, high-speed flight, predictive prefetch, stable parent fallback, and telemetry.

use logi_terrain_core::{DVec3, Telemetry};
use logi_terrain_stream::{StreamConfig, StreamEngine, SyntheticProvider, World};

/// Scenario parameters.
pub struct StressParams {
    pub world_size: f64,
    pub max_level: u8,
    pub budget_bytes: usize,
    pub max_in_flight: usize,
    pub window_radius: u32,
    pub frames_per_leg: u64,
}

impl Default for StressParams {
    fn default() -> Self {
        Self {
            world_size: 4_000_000.0, // 4,000 km square
            max_level: 9,            // 512 x 512 = 262,144 leaf tiles (>= 100k)
            budget_bytes: 8 * 1024 * 1024, // 8 MiB HARD budget
            max_in_flight: 256,
            window_radius: 6,        // 13x13 window
            frames_per_leg: 400,
        }
    }
}

/// Aggregated results + invariant counters.
pub struct StressReport {
    pub logical_tiles: u64,
    pub budget_bytes: usize,
    pub frames: u64,
    pub peak_bytes: u64,
    pub max_in_flight_seen: u32,
    pub max_resident_seen: u32,
    pub total_requests: u64,
    pub total_completed: u64,
    pub fallback_frames: u64,
    pub empty_render_frames: u64,
    pub budget_exceeded_frames: u64,
    pub final_telemetry: Telemetry,
}

impl StressReport {
    fn new(logical_tiles: u64, budget_bytes: usize) -> Self {
        Self {
            logical_tiles,
            budget_bytes,
            frames: 0,
            peak_bytes: 0,
            max_in_flight_seen: 0,
            max_resident_seen: 0,
            total_requests: 0,
            total_completed: 0,
            fallback_frames: 0,
            empty_render_frames: 0,
            budget_exceeded_frames: 0,
            final_telemetry: Telemetry::default(),
        }
    }
}

fn step(
    engine: &mut StreamEngine<SyntheticProvider>,
    world: &World,
    report: &mut StressReport,
    prev_fallbacks: &mut u64,
    pos: DVec3,
    motion: DVec3,
) {
    let empty = engine.update(world, pos, motion).is_empty();
    let t = engine.telemetry();

    report.frames += 1;
    if (t.bytes_resident as usize) > report.budget_bytes {
        report.budget_exceeded_frames += 1;
    }
    if empty {
        report.empty_render_frames += 1;
    }
    if t.fallbacks_used > *prev_fallbacks {
        report.fallback_frames += 1;
    }
    *prev_fallbacks = t.fallbacks_used;

    report.peak_bytes = report.peak_bytes.max(t.bytes_resident);
    report.max_in_flight_seen = report.max_in_flight_seen.max(t.in_flight);
    report.max_resident_seen = report.max_resident_seen.max(t.resident_tiles);
    report.total_requests = t.requests_issued;
    report.total_completed = t.requests_completed;
    report.final_telemetry = t;
}

/// Run the brutal streaming scenario: diagonal flight → teleport → high-speed flight.
pub fn run_stress(p: &StressParams) -> StressReport {
    let world = World::new(p.world_size, p.max_level, p.world_size / 4.0);
    let cfg = StreamConfig {
        budget_bytes: p.budget_bytes,
        max_in_flight: p.max_in_flight,
        window_radius: p.window_radius,
        sse_target_pixels: 2.0,
        fov_y_radians: 60.0_f64.to_radians(),
        viewport_height: 1080.0,
        prefetch_lead: 8.0,
    };
    let mut engine = StreamEngine::new(cfg, SyntheticProvider::new(6, 40)); // latency 6..46 ticks
    let mut report = StressReport::new(world.logical_tile_count(), p.budget_bytes);
    let mut prev_fallbacks = 0u64;

    let alt = 1500.0;
    let half = p.world_size * 0.5;
    let leg = p.frames_per_leg;
    let speed = (p.world_size * 0.8) / leg as f64;

    // Leg 1 — diagonal flight across most of the world.
    let mut pos = DVec3::new(-half * 0.9, alt, -half * 0.9);
    let m1 = DVec3::new(speed, 0.0, speed);
    for _ in 0..leg {
        step(&mut engine, &world, &mut report, &mut prev_fallbacks, pos, m1);
        pos.x += m1.x;
        pos.z += m1.z;
    }

    // Teleport to a far corner (motion ~ 0) — exercises stable parent fallback.
    pos = DVec3::new(half * 0.85, alt, -half * 0.85);
    let m0 = DVec3::default();
    for _ in 0..(leg / 4) {
        step(&mut engine, &world, &mut report, &mut prev_fallbacks, pos, m0);
    }

    // Leg 2 — high-speed flight (3x) to stress predictive prefetch.
    let m2 = DVec3::new(-speed * 3.0, 0.0, speed * 3.0);
    for _ in 0..leg {
        step(&mut engine, &world, &mut report, &mut prev_fallbacks, pos, m2);
        pos.x += m2.x;
        pos.z += m2.z;
    }

    report
}
