//! The brutal streaming test as an assertion: bounded memory, parent fallback, bounded in-flight.

use logi_terrain_tools::{run_stress, StressParams};

#[test]
fn brutal_streaming_stays_bounded_with_fallback() {
    let p = StressParams::default();
    let r = run_stress(&p);

    // A planet-scale logical world.
    assert!(
        r.logical_tiles >= 100_000,
        "logical world >= 100k tiles (got {})",
        r.logical_tiles
    );

    // The core invariant: the hard memory budget is NEVER exceeded.
    assert_eq!(r.budget_exceeded_frames, 0, "memory budget exceeded on some frame");
    assert!(
        r.peak_bytes <= p.budget_bytes as u64,
        "peak {} > budget {}",
        r.peak_bytes,
        p.budget_bytes
    );

    // Eviction must actually be exercised (the world visited far exceeds the budget).
    assert!(
        r.final_telemetry.evictions > 0,
        "eviction not exercised — test isn't brutal enough"
    );

    // In-flight requests respect the cap.
    assert!(
        r.max_in_flight_seen as usize <= p.max_in_flight,
        "in-flight cap violated: {}",
        r.max_in_flight_seen
    );

    // Stable parent fallback was used (notably right after the teleport).
    assert!(r.fallback_frames > 0, "parent fallback never used");

    // Streaming made progress.
    assert!(r.total_completed > 0, "no requests completed");

    // There is always something correct to render except during the initial warmup
    // (root page latency is at most ~46 ticks).
    assert!(
        r.empty_render_frames <= 64,
        "too many empty-render frames: {}",
        r.empty_render_frames
    );
}
