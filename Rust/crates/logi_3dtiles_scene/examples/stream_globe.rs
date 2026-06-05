//! Live streaming of Google Photorealistic 3D Tiles around a city, through the full Rust stack
//! (ion → io → tileset → select → gltf-decode → ENU rebase). Token from the environment:
//! `$env:CESIUM_ION_TOKEN="<token>"; cargo run -p logi_3dtiles_scene --example stream_globe --release`

use glam::DVec3;
use logi_3dtiles_scene::TilesetWorld;
use logi_geospatial::{Cartographic, Ellipsoid, EnuFrame};

fn main() {
    let token = std::env::var("CESIUM_ION_TOKEN").expect("set CESIUM_ION_TOKEN");
    let asset = logi_3dtiles_ion::resolve_asset(2275207, &token).expect("ion resolve");
    let root_bytes = logi_3dtiles_io::http_get(&asset.tileset_url).expect("fetch root.json");
    println!("root.json: {} bytes", root_bytes.len());

    // Target: the Empire State Building, Manhattan (dense P3DT coverage).
    let origin = Cartographic::from_degrees(-73.9857, 40.7484, 0.0);
    let ellipsoid = Ellipsoid::wgs84();
    let enu = EnuFrame::from_cartographic(&ellipsoid, origin);
    let camera = enu.local_to_ecef(DVec3::new(0.0, -800.0, 1500.0)); // 800 m south, 1.5 km up

    let mut world =
        TilesetWorld::new(asset.tileset_url, &root_bytes, origin, 8, 6000.0, None).expect("world");
    println!("streaming P3DT over Manhattan (8 workers, 6 km cull)…");

    for step in 0..80 {
        let frame = world.step(camera, 60f64.to_radians(), 1080.0, 16.0);
        if step % 10 == 0 {
            let verts: u32 = frame.tiles.iter().map(|t| t.vertex_count).sum();
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

    let frame = world.step(camera, 60f64.to_radians(), 1080.0, 16.0);
    let verts: u32 = frame.tiles.iter().map(|t| t.vertex_count).sum();
    println!(
        "FINAL: {} render tiles, {} total verts, {} resident decoded tiles",
        frame.tiles.len(),
        verts,
        world.resident_tiles()
    );
    // DIAGNOSTIC: where does the decoded geometry actually land in UE space (cm)? Tiles selected within
    // the 6 km cull of a near-origin camera MUST have bounds centres within a few hundred thousand cm of
    // the origin. Geocenter-scale values (~6.4e8 cm) would prove a dropped RTC / node transform.
    for t in frame.tiles.iter().take(8) {
        println!(
            "  tile bounds_center_cm=({:>12.0},{:>12.0},{:>12.0}) radius_cm={:>10.0} verts={}",
            t.bounds_center[0], t.bounds_center[1], t.bounds_center[2], t.bounds_radius, t.vertex_count
        );
    }
    // Also dump the first vertex of the first tile (UE cm) to see absolute placement.
    if let Some(t) = frame.tiles.first() {
        if let Some(m) = world.tile_mesh(&t.path) {
            let v = &m.vertices;
            let rd = |o: usize| f32::from_le_bytes([v[o], v[o + 1], v[o + 2], v[o + 3]]);
            println!(
                "  first tile v0_cm=({:.0},{:.0},{:.0}) texture_bytes={}",
                rd(0), rd(4), rd(8), m.texture.len()
            );
        }
    }
}
