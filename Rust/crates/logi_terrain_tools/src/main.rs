//! `cargo run -p logi_terrain_tools --bin streaming_stress` — the brutal streaming stress harness.

use logi_terrain_tools::{run_stress, StressParams};

fn main() {
    let p = StressParams::default();
    let r = run_stress(&p);

    let mib = 1024 * 1024;
    println!("=== Logi terrain — streaming stress ===");
    println!("logical tiles:         {}", r.logical_tiles);
    println!("frames:                {}", r.frames);
    println!("memory budget:         {} MiB", r.budget_bytes / mib);
    println!(
        "peak resident:         {} MiB  ({} tiles)",
        r.peak_bytes as usize / mib,
        r.max_resident_seen
    );
    println!("budget exceeded:       {} frames", r.budget_exceeded_frames);
    println!("empty-render frames:   {}  (warmup only)", r.empty_render_frames);
    println!("fallback frames:       {}", r.fallback_frames);
    println!(
        "max in-flight:         {}  (cap {})",
        r.max_in_flight_seen, p.max_in_flight
    );
    println!("requests issued/done:  {} / {}", r.total_requests, r.total_completed);
    println!("evictions:             {}", r.final_telemetry.evictions);

    let bounded = r.budget_exceeded_frames == 0 && r.peak_bytes <= r.budget_bytes as u64;
    println!("BOUNDED MEMORY:        {}", if bounded { "PASS" } else { "FAIL" });
}
