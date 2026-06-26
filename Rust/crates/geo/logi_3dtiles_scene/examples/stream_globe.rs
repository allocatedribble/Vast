//! Live streaming of Google Photorealistic 3D Tiles around a city, through the full Rust stack driven by
//! the **`bevy_ecs` Geo World** (ion → io → tileset → ECS select/decode/publish → ENU rebase). Token from
//! the environment:
//! `$env:CESIUM_ION_TOKEN="<token>"; cargo run -p logi_3dtiles_scene --example stream_globe --release`

use glam::DVec3;
use logi_3dtiles_scene::{FetchBackend, GeoView, GeoWorld};
use logi_geospatial::{Cartographic, Ellipsoid, EnuFrame};

fn main() {
    let token = std::env::var("CESIUM_ION_TOKEN").expect("set CESIUM_ION_TOKEN");

    // Target: the Empire State Building, Manhattan (dense P3DT coverage).
    let (lon, lat) = (-73.9857, 40.7484);
    let origin = Cartographic::from_degrees(lon, lat, 0.0);
    let enu = EnuFrame::from_cartographic(&Ellipsoid::wgs84(), origin);
    let camera = enu.local_to_ecef(DVec3::new(0.0, -800.0, 1500.0)); // 800 m south, 1.5 km up

    // Blocking: ion resolve + root fetch, then a streaming Geo World, 6 km cull. `FetchBackend::Auto` selects
    // the HTTP/2 CyperSource for this Google P3DT asset when built with the default `cyper-h2` feature (else the
    // 8-worker ureq pool). Force `FetchBackend::Ureq` here to A/B the blocking pool.
    let mut world = GeoWorld::from_ion(
        2275207,
        &token,
        lon,
        lat,
        0.0,
        8,
        FetchBackend::Auto,
        6000.0,
        None,
        5120,
        4096,
    )
    .expect("build Geo World from ion");
    println!("streaming P3DT over Manhattan via the bevy_ecs Geo World (Auto fetch backend, 6 km cull)…");

    let view = |frame: u64| GeoView {
        camera_ecef: camera,
        fov_y_radians: 60f64.to_radians(),
        viewport_height: 1080.0,
        max_sse: 16.0,
        cull_distance_m: 6000.0,
        frame,
    };

    for step in 0..80u64 {
        // Stamp the wall-clock each step (H5), exactly as the production stream loop does before
        // `advance_from_ue`: the retry-backoff deadlines are `Instant`-based now, so without this the clock
        // would be frozen and a transiently-failed tile (a real 429/5xx/timeout) would never retry this run.
        world.set_now(std::time::Instant::now());
        world.advance_streaming(view(step + 1));
        if step % 10 == 0 {
            let frame = world.latest_frame();
            let verts: usize = frame.tiles.iter().map(|t| t.geom.vertices.len() / 32).sum();
            println!(
                "  step {:2}: resident={:3} in_flight={:3} render={:3} verts={}",
                step,
                world.resident_tiles(),
                world.in_flight(),
                frame.tiles.len(),
                verts
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(70));
    }

    let frame = world.latest_frame();
    let verts: usize = frame.tiles.iter().map(|t| t.geom.vertices.len() / 32).sum();
    println!(
        "FINAL: {} render tiles, {} total verts, {} resident decoded tiles",
        frame.tiles.len(),
        verts,
        world.resident_tiles()
    );
    // DIAGNOSTIC: where does the decoded geometry land in UE space (cm)? Tiles within the 6 km cull of a
    // near-origin camera MUST have bounds centres within a few hundred thousand cm of the origin; geocenter-
    // scale values (~6.4e8 cm) would prove a dropped RTC / node transform.
    for t in frame.tiles.iter().take(8) {
        let g = &t.geom;
        println!(
            "  tile key={:016x} bounds_center_cm=({:>12.0},{:>12.0},{:>12.0}) radius_cm={:>10.0} verts={}",
            t.tile_key,
            g.bounds_center[0],
            g.bounds_center[1],
            g.bounds_center[2],
            g.bounds_radius,
            g.vertices.len() / 32
        );
    }
    // Also dump the first vertex of the first tile (UE cm) to see absolute placement.
    if let Some(t) = frame.tiles.first() {
        let v = &t.geom.vertices;
        let rd = |o: usize| f32::from_le_bytes([v[o], v[o + 1], v[o + 2], v[o + 3]]);
        println!(
            "  first tile v0_cm=({:.0},{:.0},{:.0}) texture_bytes={}",
            rd(0),
            rd(4),
            rd(8),
            t.geom.texture.len()
        );
    }
}
