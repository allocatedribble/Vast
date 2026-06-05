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
    assert_eq!(m.tiles.len(), 4, "leaf_level 1 → 2x2 tiles");
    assert_eq!(store.tile_ids().len(), 4);

    // Every tile decodes (CRC-32 validated inside) and triangulates to the expected mesh shape.
    let n = m.tiling.grid_n as usize;
    for id in store.tile_ids() {
        let tile = store.read_tile(id).expect("decode python-written tile");
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
    assert_eq!(world.tile_count(), 4);
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
fn specific_tile_is_present_and_addressable() {
    let store = BakedStore::open(FIXTURE).expect("open fixture store");
    // The SW leaf must exist and round-trip its id.
    let sw = TileId::new(1, 0, 0);
    let tile = store.read_tile(sw).expect("SW tile present");
    assert_eq!(tile.id, sw);
}
