//! 3D Tiles **tile selection** — the screen-space-error traversal that turns a camera view over a
//! tileset BVH into the tiles to render + the tiles to load, for the actorless 3D-Tiles stack
//! (ADR-0009, Milestone F). This is the Rust generalisation of the kernel's regular-quadtree
//! `select_hierarchical` to cesium-native's pointer-tree.
//!
//! v1 implements: per-volume distance (region/box/sphere → an ECEF bounding sphere, plan §9.4 globe
//! geometry), the SSE refine decision (the same model as `logi_geo_lod` / 3D Tiles), `REPLACE`
//! refinement, and **parent-fallback** — when a selected (SSE-leaf) tile isn't loaded yet, render the
//! nearest loaded ancestor while the finer tile streams (the existing kernel's proven anti-pop pattern).
//! Frustum/horizon culling and the per-tile load state are injected as predicates, so the streaming loop
//! (`logi_3dtiles_scene`) owns them. The full cesium-native cross-frame `kick`/`ancestorMeetsSse` state
//! and `ADD` refinement layer on top of this (see the plan §9.2 — the highest-risk follow-up).

#![forbid(unsafe_code)]

use glam::{DMat4, DVec3};
use logi_3dtiles_tileset::{BoundingVolume, Tile, Tileset};
use logi_geospatial::{Cartographic, Ellipsoid};
use std::collections::HashMap;
use std::rc::Rc;

// One screen-space-error model for the whole engine: reuse the kernel's canonical implementation
// (`logi_geo_lod`) instead of a second copy here. Same formula, with its degenerate-FOV/zero-distance
// clamps. The 3D-Tiles selection traversal below calls it exactly as the quadtree streamer does.
pub use logi_geo_lod::screen_space_error;

/// The per-frame view + LOD parameters that drive selection (camera in ECEF, plan §9.4).
#[derive(Clone, Copy, Debug)]
pub struct SelectParams {
    pub camera_position: DVec3,
    pub fov_y_radians: f64,
    pub viewport_height: f64,
    /// Refine while a tile's screen-space error exceeds this many pixels (3D Tiles `maxScreenSpaceError`).
    pub max_screen_space_error: f64,
}

/// A tile chosen to render or load. `path` is the child-index path from the root — a stable identity for
/// dedup + cross-frame state.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectedTile {
    pub path: Vec<u32>,
    pub world_transform: DMat4,
    pub content_uris: Vec<String>,
    pub geometric_error: f64,
    pub distance: f64,
}

/// The result of one selection pass.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    /// Tiles to draw this frame (deduped — an ancestor used as fallback for several descendants appears once).
    pub render: Vec<SelectedTile>,
    /// Selected tiles whose content isn't loaded yet — request these.
    pub load: Vec<SelectedTile>,
}

/// Conservatively bound a tile's bounding volume by an ECEF sphere `(center, radius)`. `box`/`sphere`
/// volumes are in the tile's coordinate system and ride `world` (the cumulative tile transform);
/// `region` is geodetic and (per spec) transform-independent — bounded from its 8 corners on the ellipsoid.
pub fn bounding_sphere_ecef(bv: &BoundingVolume, world: &DMat4, ellipsoid: &Ellipsoid) -> (DVec3, f64) {
    match bv {
        BoundingVolume::Sphere(s) => {
            let center = world.transform_point3(DVec3::new(s[0], s[1], s[2]));
            // `max_axis_scale` (largest column norm) is EXACT for the rotation + uniform-scale transforms
            // 3D Tiles uses in practice (it then equals the operator 2-norm); it can slightly UNDER-bound
            // only under shear / anisotropic scale (rare). v1 simplification.
            (center, s[3] * max_axis_scale(world))
        }
        BoundingVolume::Box(b) => {
            let center = world.transform_point3(DVec3::new(b[0], b[1], b[2]));
            let u = world.transform_vector3(DVec3::new(b[3], b[4], b[5]));
            let v = world.transform_vector3(DVec3::new(b[6], b[7], b[8]));
            let w = world.transform_vector3(DVec3::new(b[9], b[10], b[11]));
            (center, u.length() + v.length() + w.length()) // OBB → enclosing sphere (conservative)
        }
        BoundingVolume::Region(r) => {
            // 8 ellipsoid corners → enclosing sphere: tight for small leaf regions (the real case); for a
            // very wide region the mid-arc surface bulge can sit just outside it (under-bound) — cesium
            // uses an oriented box there. v1 simplification (regions are normally tiny leaves).
            let (west, south, mut east, north, min_h, max_h) = (r[0], r[1], r[2], r[3], r[4], r[5]);
            // Antimeridian wrap: a region with east < west crosses ±π. We unwrap east by +2π so the four
            // corner longitudes span the *short* way round, giving a correct (if conservative) sphere over
            // the seam instead of one swept across the whole globe. A wide wrapped region still over-refines
            // (same v1 limitation as a wide non-wrapped one); it is never under-bounded unsafely.
            if east < west {
                east += std::f64::consts::TAU;
            }
            // Stack array — no per-call heap allocation (this runs per region tile per frame).
            let corners: [DVec3; 8] = [
                (west, south, min_h),
                (east, south, min_h),
                (west, north, min_h),
                (east, north, min_h),
                (west, south, max_h),
                (east, south, max_h),
                (west, north, max_h),
                (east, north, max_h),
            ]
            .map(|(lon, lat, h)| ellipsoid.cartographic_to_cartesian(Cartographic::new(lon, lat, h)));
            let center = corners.iter().fold(DVec3::ZERO, |a, p| a + *p) / 8.0;
            let radius = corners.iter().map(|p| (*p - center).length()).fold(0.0, f64::max);
            (center, radius)
        }
    }
}

fn max_axis_scale(m: &DMat4) -> f64 {
    m.x_axis
        .truncate()
        .length()
        .max(m.y_axis.truncate().length())
        .max(m.z_axis.truncate().length())
}

/// Select the tiles to render + load for `params`, given pluggable predicates:
/// - `is_loaded(path)` — is that tile's content resident?
/// - `is_visible(center, radius)` — does that bounding sphere pass frustum/horizon culling? (`|_, _| true`
///   for no culling.)
pub fn select(
    tileset: &Tileset,
    ellipsoid: &Ellipsoid,
    params: &SelectParams,
    is_loaded: &mut dyn FnMut(&[u32]) -> bool,
    is_visible: &mut dyn FnMut(DVec3, f64) -> bool,
) -> Selection {
    select_tree(&tileset.root, ellipsoid, params, is_loaded, is_visible)
}

/// Like [`select`] but driven from a root [`Tile`] directly — for callers that own a growing tree (the
/// streaming scene grafts external `.json` tilesets into it as it descends) rather than a parsed [`Tileset`].
pub fn select_tree(
    root: &Tile,
    ellipsoid: &Ellipsoid,
    params: &SelectParams,
    is_loaded: &mut dyn FnMut(&[u32]) -> bool,
    is_visible: &mut dyn FnMut(DVec3, f64) -> bool,
) -> Selection {
    let mut render: HashMap<Vec<u32>, Rc<SelectedTile>> = HashMap::new();
    let mut load: Vec<SelectedTile> = Vec::new();
    let mut ctx = Ctx {
        ellipsoid,
        params,
        is_loaded,
        is_visible,
        render: &mut render,
        load: &mut load,
        warned_add: false,
    };
    let mut path = Vec::new();
    visit(root, &mut path, root.matrix(), None, &mut ctx);
    // By here each render entry is the sole owner of its Rc (the recursion's clones are dropped), so
    // `try_unwrap` moves the value out without copying; it would only clone on the (unreachable) shared case.
    // Order is intentionally left as-is (NOT sorted): the Geo engine is render-only / non-authoritative
    // (ADR-0007), so tile draw order never affects gameplay replay or snapshot equality. Rendering needs no
    // determinism — sorting here would be wasted work. Do not "fix" this to a deterministic order.
    let render: Vec<SelectedTile> = render
        .into_values()
        .map(|rc| Rc::try_unwrap(rc).unwrap_or_else(|rc| (*rc).clone()))
        .collect();
    Selection { render, load }
}

struct Ctx<'a> {
    ellipsoid: &'a Ellipsoid,
    params: &'a SelectParams,
    is_loaded: &'a mut dyn FnMut(&[u32]) -> bool,
    is_visible: &'a mut dyn FnMut(DVec3, f64) -> bool,
    render: &'a mut HashMap<Vec<u32>, Rc<SelectedTile>>,
    load: &'a mut Vec<SelectedTile>,
    /// Latches once this pass has warned about an unsupported `refine=ADD` tile, so we log it at most once
    /// per selection rather than once per ADD node per frame.
    warned_add: bool,
}

/// Build a `SelectedTile`. This allocates a `Vec<String>` of content URIs (+ the path), so it is called
/// ONLY when the tile is actually kept — a resident fallback or an SSE-leaf to request — never for the
/// transient interior nodes that just pass an inherited fallback down.
fn make_selected(path: &[u32], world: DMat4, tile: &Tile, distance: f64) -> SelectedTile {
    SelectedTile {
        path: path.to_vec(),
        world_transform: world,
        content_uris: tile.content_uris().map(String::from).collect(),
        geometric_error: tile.geometric_error,
        distance,
    }
}

fn visit(tile: &Tile, path: &mut Vec<u32>, world: DMat4, nearest_loaded: Option<Rc<SelectedTile>>, ctx: &mut Ctx) {
    let (center, radius) = bounding_sphere_ecef(&tile.bounding_volume, &world, ctx.ellipsoid);
    if !(ctx.is_visible)(center, radius) {
        return; // culled — drop the whole subtree
    }
    let distance = ((ctx.params.camera_position - center).length() - radius).max(0.0);
    let loaded = (ctx.is_loaded)(path);

    // The stand-in to draw for this subtree until finer tiles load: this tile if it's resident (built now,
    // shared via Rc), else the nearest resident ancestor handed down (a cheap refcount bump). Crucially we
    // do NOT build a SelectedTile for unloaded interior nodes — that is the per-node allocation we avoid.
    let fallback: Option<Rc<SelectedTile>> = if loaded {
        Some(Rc::new(make_selected(path, world, tile, distance)))
    } else {
        nearest_loaded
    };

    // `tile.refine` ADD is not yet implemented — ADD means children render *in addition to* the parent;
    // we only do REPLACE. An ADD-root tileset would otherwise render holes silently, so warn (at most once
    // per pass; rare — production tilesets are overwhelmingly REPLACE; see ADR-0009 §9.2 follow-up).
    if !ctx.warned_add && tile.refine == Some(logi_3dtiles_tileset::Refine::Add) {
        report_unsupported_add(path);
        ctx.warned_add = true;
    }

    // Refine criterion single-homed in `logi_geo_lod::should_refine` (audit M22). This selector is
    // stateless, so it opts out of the hysteresis dead-band (`hysteresis: 0.0`, `was_refined: false`) —
    // a plain `SSE > target` threshold, identical to the previous inline check.
    let refine = logi_geo_lod::should_refine(
        tile.geometric_error,
        distance,
        &logi_geo_lod::RefineParams {
            fov_y_radians: ctx.params.fov_y_radians,
            viewport_height: ctx.params.viewport_height,
            sse_target: ctx.params.max_screen_space_error,
            hysteresis: 0.0,
        },
        false,
    );
    if refine && !tile.children.is_empty() {
        // One shared path stack: push this child's index, recurse, pop — instead of cloning `path` per
        // child (the O(nodes·depth) allocation the header promises to avoid). `make_selected` still
        // snapshots the path with `.to_vec()` at the (rare) keep-points.
        for (i, child) in tile.children.iter().enumerate() {
            path.push(i as u32);
            visit(child, path, world * child.matrix(), fallback.clone(), ctx);
            path.pop();
        }
    } else {
        // SSE-leaf: render the nearest resident tile (deduped), and request this tile if it isn't resident.
        if let Some(sel) = &fallback {
            ctx.render.entry(sel.path.clone()).or_insert_with(|| Rc::clone(sel));
        }
        if !loaded {
            ctx.load.push(make_selected(path, world, tile, distance));
        }
    }
}

/// Warn (rate-unlimited but only on actual ADD nodes, which are rare) that a tile requests `ADD`
/// refinement, which the v1 selector treats as `REPLACE`. Pulled out so the hot path stays branch-light.
#[cold]
fn report_unsupported_add(path: &[u32]) {
    tracing::warn!(
        ?path,
        "3D Tiles refine=ADD is unsupported (treated as REPLACE); subtree may render holes"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use logi_3dtiles_tileset::parse_tileset;
    use std::collections::BTreeSet;

    // A 3-level tileset; every tile's bounding sphere sits at the same equator point (radius 1) so the
    // distance is just the camera offset, and SSE is driven purely by each level's geometric error.
    const C: f64 = 6_378_137.0;
    fn nested_tileset() -> Tileset {
        let json = format!(
            r#"{{ "asset": {{"version":"1.1"}}, "geometricError": 2000,
              "root": {{ "boundingVolume": {{"sphere":[{C},0,0,1]}}, "geometricError": 1000, "refine":"REPLACE",
                "content": {{"uri":"r.glb"}}, "children": [
                  {{ "boundingVolume": {{"sphere":[{C},0,0,1]}}, "geometricError": 100, "content": {{"uri":"0.glb"}},
                     "children": [
                       {{ "boundingVolume": {{"sphere":[{C},0,0,1]}}, "geometricError": 10, "content": {{"uri":"00.glb"}} }},
                       {{ "boundingVolume": {{"sphere":[{C},0,0,1]}}, "geometricError": 10, "content": {{"uri":"01.glb"}} }} ] }},
                  {{ "boundingVolume": {{"sphere":[{C},0,0,1]}}, "geometricError": 100, "content": {{"uri":"1.glb"}},
                     "children": [
                       {{ "boundingVolume": {{"sphere":[{C},0,0,1]}}, "geometricError": 10, "content": {{"uri":"10.glb"}} }},
                       {{ "boundingVolume": {{"sphere":[{C},0,0,1]}}, "geometricError": 10, "content": {{"uri":"11.glb"}} }} ] }} ] }} }}"#
        );
        parse_tileset(json.as_bytes()).unwrap()
    }

    fn params_at(offset: f64) -> SelectParams {
        SelectParams {
            camera_position: DVec3::new(C + offset, 0.0, 0.0),
            fov_y_radians: std::f64::consts::FRAC_PI_3, // 60°
            viewport_height: 1080.0,
            max_screen_space_error: 16.0,
        }
    }

    fn paths(tiles: &[SelectedTile]) -> BTreeSet<Vec<u32>> {
        tiles.iter().map(|t| t.path.clone()).collect()
    }

    #[test]
    fn far_camera_renders_only_the_root() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        let sel = select(&ts, &e, &params_at(200_000.0), &mut |_| true, &mut |_, _| true);
        assert_eq!(paths(&sel.render), BTreeSet::from([vec![]])); // just the root
        assert!(sel.load.is_empty());
    }

    #[test]
    fn near_camera_refines_to_leaves() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        let sel = select(&ts, &e, &params_at(2000.0), &mut |_| true, &mut |_, _| true);
        assert_eq!(
            paths(&sel.render),
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]])
        );
        assert!(sel.load.is_empty());
    }

    #[test]
    fn unloaded_leaf_falls_back_to_nearest_loaded_ancestor() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        // grandchild [0,0] isn't resident; everything else is.
        let mut is_loaded = |p: &[u32]| p != [0, 0];
        let sel = select(&ts, &e, &params_at(2000.0), &mut is_loaded, &mut |_, _| true);
        // [0,0] falls back to its loaded parent [0]; the other three leaves render themselves.
        assert_eq!(
            paths(&sel.render),
            BTreeSet::from([vec![0], vec![0, 1], vec![1, 0], vec![1, 1]])
        );
        assert_eq!(paths(&sel.load), BTreeSet::from([vec![0, 0]]));
    }

    #[test]
    fn unloaded_leaf_with_unloaded_ancestors_loads_but_renders_root() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        // only the root is resident; the whole left subtree falls back to it.
        let mut is_loaded = |p: &[u32]| p.is_empty();
        let sel = select(&ts, &e, &params_at(2000.0), &mut is_loaded, &mut |_, _| true);
        assert_eq!(paths(&sel.render), BTreeSet::from([vec![]])); // root is the only resident fallback
                                                                  // all four leaves are requested.
        assert_eq!(
            paths(&sel.load),
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]])
        );
    }

    #[test]
    fn culling_drops_a_subtree() {
        // Place child [1]'s subtree far away (along +Z) so a visibility predicate can cull it.
        let far = C; // a point on the equator 90° around — clearly distinct from (C,0,0)
        let json = format!(
            r#"{{ "asset": {{"version":"1.1"}}, "geometricError": 2000,
              "root": {{ "boundingVolume": {{"sphere":[{C},0,0,1]}}, "geometricError": 1000, "refine":"REPLACE",
                "content": {{"uri":"r.glb"}}, "children": [
                  {{ "boundingVolume": {{"sphere":[{C},0,0,1]}}, "geometricError": 10, "content": {{"uri":"near.glb"}} }},
                  {{ "boundingVolume": {{"sphere":[0,0,{far},1]}}, "geometricError": 10, "content": {{"uri":"far.glb"}} }} ] }} }}"#
        );
        let ts = parse_tileset(json.as_bytes()).unwrap();
        let e = Ellipsoid::wgs84();
        // cull anything whose center is near the +Z pole point (the far child).
        let mut is_visible = |center: DVec3, _r: f64| (center - DVec3::new(0.0, 0.0, C)).length() > 1.0;
        let sel = select(&ts, &e, &params_at(2000.0), &mut |_| true, &mut is_visible);
        assert_eq!(paths(&sel.render), BTreeSet::from([vec![0]])); // only the near child; far one culled
    }

    #[test]
    fn bounding_sphere_for_each_volume_type() {
        let e = Ellipsoid::wgs84();
        let id = DMat4::IDENTITY;
        // sphere: center + radius pass through identity.
        let (c, r) = bounding_sphere_ecef(&BoundingVolume::Sphere([1.0, 2.0, 3.0, 9.0]), &id, &e);
        assert_eq!(c, DVec3::new(1.0, 2.0, 3.0));
        assert!((r - 9.0).abs() < 1e-9);
        // box: radius = sum of half-axis lengths.
        let (_, rb) = bounding_sphere_ecef(
            &BoundingVolume::Box([0.0, 0.0, 0.0, 3.0, 0.0, 0.0, 0.0, 4.0, 0.0, 0.0, 0.0, 12.0]),
            &id,
            &e,
        );
        assert!((rb - 19.0).abs() < 1e-9);
        // a translation+scale transform moves the sphere center and scales its radius.
        let m = DMat4::from_scale_rotation_translation(
            glam::DVec3::splat(2.0),
            glam::DQuat::IDENTITY,
            DVec3::new(100.0, 0.0, 0.0),
        );
        let (c2, r2) = bounding_sphere_ecef(&BoundingVolume::Sphere([1.0, 0.0, 0.0, 5.0]), &m, &e);
        assert_eq!(c2, DVec3::new(102.0, 0.0, 0.0)); // 100 + 1*2
        assert!((r2 - 10.0).abs() < 1e-9); // 5 * scale 2
                                           // region: a small patch near the equator bounds to a sphere on the globe.
        let patch = BoundingVolume::Region([-0.01, -0.01, 0.01, 0.01, 0.0, 100.0]);
        let (cr, rr) = bounding_sphere_ecef(&patch, &id, &e);
        assert!((cr.length() - WGS84_ISH).abs() < 70_000.0); // center is ~on the globe
        assert!(rr > 0.0 && rr < 200_000.0);
    }

    #[test]
    fn antimeridian_region_bounds_the_seam_not_the_globe() {
        let e = Ellipsoid::wgs84();
        let id = DMat4::IDENTITY;
        let pi = std::f64::consts::PI;
        // A small patch straddling the antimeridian: west just below +π, east just above -π (east < west).
        let wrapped = BoundingVolume::Region([pi - 0.01, -0.01, -pi + 0.01, 0.01, 0.0, 100.0]);
        let (cw, rw) = bounding_sphere_ecef(&wrapped, &id, &e);
        // Without unwrap the 4 corners span ~360° and the radius would be a full Earth diameter (~1.2e7 m);
        // with unwrap it is a small seam patch comparable to the equivalent non-wrapped equator patch.
        assert!(
            rw < 200_000.0,
            "wrapped region radius should be a small patch, got {rw}"
        );
        // Its center sits near the -X axis (the antimeridian), ~on the globe.
        assert!((cw.length() - WGS84_ISH).abs() < 70_000.0, "center ~on globe: {cw:?}");
        assert!(cw.x < 0.0, "antimeridian patch is on the -X side: {cw:?}");
    }

    const WGS84_ISH: f64 = 6_378_137.0;
}
