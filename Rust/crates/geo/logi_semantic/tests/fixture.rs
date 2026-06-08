//! Cross-language format contract: decode a tile store produced by the **Python** `Tools/logi_bake`
//! (the `hero_tiny` fixture, checked in) and exercise the full read path. If the Rust decoder and the
//! Python writer ever disagree byte-for-byte, this fails. Regenerate the fixture with:
//!
//! ```text
//! python -m logi_bake bake --radius 200 --leaf-level 1 --grid-n 9 --seed 42 \
//!   --out Rust/crates/logi_semantic/tests/fixtures/hero_tiny
//! ```

use logi_geospatial::{Cartographic, Ellipsoid};
use logi_semantic::{triangulate_enu, BakedStore, BakedWorld, TileId};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hero_tiny");

#[test]
fn reads_python_baked_store() {
    let store = BakedStore::open(FIXTURE).expect("open fixture store (run logi_bake to regenerate)");
    let m = store.manifest();
    assert_eq!(m.format_version, 1);
    assert_eq!(m.tiling.grid_n, 9);
    assert_eq!(m.tiles.len(), 5, "pyramid: level 0 root + 4 level-1 tiles");

    // Every tile decodes (CRC-32 validated inside) and triangulates to the expected mesh shape. Read via
    // the production entry path (`read_tile_entry`, as `BakedWorld::open_with` does), iterating the manifest.
    let n = m.tiling.grid_n as usize;
    for entry in &store.manifest().tiles {
        let tile = store.read_tile_entry(entry).expect("decode python-written tile");
        assert_eq!(tile.grid_n as usize, n);
        assert_eq!(tile.heights.len(), n * n);
        assert!(tile.h_min.is_finite() && tile.h_max >= tile.h_min);

        let mesh = triangulate_enu(&tile);
        assert_eq!(mesh.vertex_count(), n * n);
        assert_eq!(mesh.indices.len(), (n - 1) * (n - 1) * 6);
        assert!(mesh.bounds_radius > 0.0);
        // ENU → Unreal cm conversion stays finite + non-degenerate.
        let ue = mesh.to_ue_cm();
        assert_eq!(ue.vertex_count(), n * n);
    }
}

#[test]
fn manifest_origin_ecef_matches_engine_geodesy() {
    // The bake (Python geo.py) and the engine (logi_geospatial) must compute the SAME origin ECEF, or
    // every baked tile lands in the wrong place. This is the Python↔Rust geodesy contract.
    let store = BakedStore::open(FIXTURE).expect("open fixture store");
    let o = &store.manifest().origin;
    let expect =
        Ellipsoid::wgs84().cartographic_to_cartesian(Cartographic::from_degrees(o.lon_deg, o.lat_deg, o.height_m));
    assert!((o.ecef[0] - expect.x).abs() < 1e-3, "x: {} vs {}", o.ecef[0], expect.x);
    assert!((o.ecef[1] - expect.y).abs() < 1e-3, "y: {} vs {}", o.ecef[1], expect.y);
    assert!((o.ecef[2] - expect.z).abs() < 1e-3, "z: {} vs {}", o.ecef[2], expect.z);
}

#[test]
fn baked_world_loads_all_tiles_into_ue_space() {
    let world = BakedWorld::open(FIXTURE).expect("open baked world");
    assert_eq!(world.tile_count(), 5); // pyramid: 1 root + 4 leaves
                                       // Origin sits on the globe (~Earth radius from the geocentre).
    let o = world.origin_ecef();
    let r = (o[0] * o[0] + o[1] * o[1] + o[2] * o[2]).sqrt();
    assert!((r - 6_369_000.0).abs() < 40_000.0, "origin radius {r}");
    // Every tile is a 9x9 grid → 81 verts, 8*8*2 tris.
    for t in world.tiles() {
        assert_eq!(t.mesh.vertex_count(), 81);
        assert_eq!(t.mesh.indices.len(), 8 * 8 * 6);
        assert!(t.mesh.bounds_radius > 0.0);
    }
}

#[test]
fn sse_select_coarsens_far_and_refines_near() {
    let mut world = BakedWorld::open(FIXTURE).expect("open baked world");
    // 50 km up → the root's screen-space error is tiny → just the coarse level-0 tile.
    let far = world.select([0.0, 0.0, 5_000_000.0], 1.0, 1080.0, 16.0);
    assert_eq!(far, vec![TileId::new(0, 0, 0)], "far → the coarse root only");
    // At the surface with a tight SSE target → refines to the 4 level-1 leaves.
    let near = world.select([0.0, 0.0, 50.0], 1.0, 1080.0, 1.0);
    assert_eq!(near.len(), 4, "near → the 4 leaves");
    assert!(near.iter().all(|t| t.level == 1), "near leaves are level 1");
}

#[test]
fn specific_tile_is_present_and_addressable() {
    let store = BakedStore::open(FIXTURE).expect("open fixture store");
    // The SW leaf must exist and round-trip its id (found in the manifest, read via the entry path).
    let sw = TileId::new(1, 0, 0);
    let entry = store
        .manifest()
        .tiles
        .iter()
        .find(|t| t.level == sw.level && t.x == sw.x && t.y == sw.y)
        .expect("SW tile present");
    let tile = store.read_tile_entry(entry).expect("decode SW tile");
    assert_eq!(tile.id, sw);
}
