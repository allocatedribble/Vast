use logi_terrain_core::DVec3;
use logi_terrain_stream::{StreamConfig, StreamEngine, SyntheticProvider, World};

/// The per-frame promote budget must cap how many requests the engine issues in a frame.
#[test]
fn promote_budget_caps_requests_per_frame() {
    let world = World::new(16_384.0, 8, 4096.0);
    let cfg = StreamConfig {
        budget_bytes: 64 * 1024 * 1024,
        max_in_flight: 4096,
        window_radius: 6,
        sse_target_pixels: 2.0,
        fov_y_radians: 1.0,
        viewport_height: 1080.0,
        prefetch_lead: 0.0,
    };
    // Long latency so nothing completes during the test — isolates the promote cap.
    let mut engine = StreamEngine::new(cfg, SyntheticProvider::new(1000, 0));

    let mut budgets = engine.budgets();
    budgets.max_pages_promoted_per_frame = 4;
    engine.set_budgets(budgets);

    let before = engine.telemetry().requests_issued;
    engine.update(&world, DVec3::new(0.0, 300.0, 0.0), DVec3::default());
    let issued = engine.telemetry().requests_issued - before;

    assert!(issued > 0, "some requests should issue");
    assert!(issued <= 4, "promote budget must cap per-frame requests, got {issued}");

    // queue_counts reflects requests *issued* (counted after the budget gate), so it sums to the
    // promoted count and stays within the per-frame cap — never the larger desired-candidate set.
    let counts = engine.queue_counts();
    let total_issued: u32 = counts.iter().sum();
    assert_eq!(u64::from(total_issued), issued, "queue_counts must sum to issued requests");
    assert!(total_issued <= 4, "queue_counts must respect the promote cap (got {total_issued})");
}
