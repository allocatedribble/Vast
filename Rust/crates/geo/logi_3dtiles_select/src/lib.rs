//! 3D Tiles **tile selection** — the screen-space-error traversal that turns a camera view over a
//! tileset BVH into the tiles to render + the tiles to load, for the actorless 3D-Tiles stack
//! (ADR-0009, Milestone F). This is the Rust generalisation of the kernel's regular-quadtree
//! `select_hierarchical` to cesium-native's pointer-tree.
//!
//! Implements: per-volume distance (region/box/sphere → an ECEF bounding sphere, plan §9.4 globe geometry),
//! the SSE refine decision (the same model as `logi_geo_lod` / 3D Tiles) with a **hysteresis dead-band**,
//! `REPLACE` refinement, and the cesium-native **cross-frame kick** (plan §9.2): a per-tile
//! [`SelectionResult`] is folded up as [`TraversalDetails`] and persisted as a [`SelectionState`] so a fresh,
//! not-yet-loaded refinement renders the coarse ancestor instead of a hole-y partial split, then switches to
//! the fine LOD **atomically** once the subtree fully loads (and never pops backward once it's been shown).
//! The per-tile load state + a [`Visibility`] classification are injected as predicates, so the streaming
//! loop (`logi_3dtiles_scene`) owns frustum/distance culling. **Frustum-aware LOD** is wired (§9.2 culled-SSE):
//! in-view tiles refine to `max_screen_space_error`, off-view-but-in-range tiles to the coarser
//! `culled_screen_space_error` (a resident coarse shell — shadow-safe, since the engine casts shadows from a
//! larger-than-view frustum — never hard-culled by direction; only distance hard-culls). Also done: the
//! `ancestor_meets_sse` coarse-fallback load (§9.2 step 6) and, in the Geo World, the view-direction fetch
//! priority (§9.2 step 7) + the ECEF view frustum. Only `ADD` refinement is still deferred (warned + treated
//! as REPLACE — no ADD tileset in the pipeline to validate against).

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
    /// The (looser) SSE target for tiles the visibility predicate reports [`Visibility::Offscreen`] — outside
    /// the view frustum but kept resident (for shadows the engine casts from a larger-than-view frustum, and
    /// so a camera turn finds them already coarse-loaded). Typically several× `max_screen_space_error` so
    /// off-view tiles refine to a coarse shell, not full detail. Set it equal to `max_screen_space_error` to
    /// disable the distinction (off-view tiles then load at full detail).
    pub culled_screen_space_error: f64,
    /// Hysteresis dead-band fraction for the refine decision (`0.0` = stateless plain threshold). A tile that
    /// was `Refined` last pass holds its split until SSE drops below `max_screen_space_error / (1 + hysteresis)`,
    /// so a camera hovering on the threshold can't flip parent↔children every frame (anti-pop). Only takes
    /// effect when `prev_state` carries the previous pass's [`SelectionResult`]s.
    pub hysteresis: f64,
}

/// How a tile's bounding sphere relates to the view this frame — the streaming visibility classification
/// (the predicate the [`select`] caller supplies). **Frustum-aware LOD, not a hard cull:** `Offscreen` tiles
/// are still streamed (so the engine can cast their shadows from its larger-than-view shadow frustum, and so
/// camera turns find them ready), just at the coarser `culled_screen_space_error`. Only `Culled` drops a
/// subtree — reserve it for genuinely out-of-range tiles (e.g. beyond the streaming distance).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Visibility {
    /// Inside the view frustum (+ any edge margin) — refine to the full `max_screen_space_error`.
    Visible,
    /// Outside the view frustum but kept resident — refine only to the coarse `culled_screen_space_error`.
    Offscreen,
    /// Out of range entirely — drop the whole subtree (no fetch, no render).
    Culled,
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
    /// The tile's bounding sphere in ECEF (metres) — already computed during the walk, surfaced so a
    /// consumer (the ECS Geo World) can populate per-tile bounds without recomputing it.
    pub bounds_center: DVec3,
    pub bounds_radius: f64,
}

/// What the traversal decided for a tile this pass — the **cross-frame selection state** (cesium-native's
/// `TileSelectionState`). Persisting it lets the next pass answer "was this tile refined / rendered last
/// frame?", which drives both the hysteresis dead-band and the anti-pop **kick**. Tiles that were culled or
/// merely loading (shown via an ancestor) are simply absent from the map.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelectionResult {
    /// The tile's own content was drawn at this LOD (it met SSE / is a leaf, and is loaded).
    Rendered,
    /// The tile refined: its children were drawn, not the tile itself.
    Refined,
    /// The tile *wanted* to refine but its descendants weren't ready and nothing finer was on screen, so the
    /// coarse tile (or its nearest loaded ancestor) was drawn instead — the **kick** (anti-hole fallback).
    Kicked,
}

/// Per-tile [`SelectionResult`] from one pass, keyed by child-index path — the state a stateful caller feeds
/// back next frame (cesium-native's per-tile `TileSelectionState`, minus the frame number: the *whole map*
/// is "last frame"). An empty map ⇒ a cold / stateless pass (no hysteresis, no cross-frame kick).
pub type SelectionState = std::collections::HashMap<Vec<u32>, SelectionResult>;

/// The result of one selection pass.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    /// Tiles to draw this frame (deduped — an ancestor used as fallback for several descendants appears once).
    pub render: Vec<SelectedTile>,
    /// Selected tiles whose content isn't loaded yet — request these.
    pub load: Vec<SelectedTile>,
    /// This pass's per-tile [`SelectionResult`] — feed it back next frame as `prev_state` to enable the
    /// hysteresis dead-band + the cross-frame kick. Empty for a cold/stateless pass.
    pub state: SelectionState,
}

/// Folded up the tree during a [`visit`] (cesium-native's `TraversalDetails`) to drive the **kick**: does a
/// refined subtree have everything it needs, did it show anything last frame, and how much is still loading?
#[derive(Clone, Copy, Debug)]
struct TraversalDetails {
    /// Every visited tile in the subtree has its content loaded (so the fine LOD can be shown without holes).
    all_renderable: bool,
    /// Some tile in the subtree drew its own content **last** frame — if so, don't kick back to coarse (that
    /// would be a backward pop); let the in-progress refinement keep showing.
    any_rendered_last_frame: bool,
    /// How many visited tiles in the subtree aren't loaded yet — the kick's "too many holes" threshold.
    not_yet_renderable: u32,
}

impl TraversalDetails {
    /// A subtree that imposes no hole/kick pressure on its ancestors (a culled or fully-absent branch).
    const NEUTRAL: Self = Self {
        all_renderable: true,
        any_rendered_last_frame: false,
        not_yet_renderable: 0,
    };
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
/// - `visibility(center, radius)` — the bounding sphere's [`Visibility`] (frustum-aware LOD; `|_, _|
///   Visibility::Visible` disables culling — everything at full detail).
pub fn select(
    tileset: &Tileset,
    ellipsoid: &Ellipsoid,
    params: &SelectParams,
    is_loaded: &mut dyn FnMut(&[u32]) -> bool,
    visibility: &mut dyn FnMut(DVec3, f64) -> Visibility,
    prev_state: &SelectionState,
) -> Selection {
    select_tree(&tileset.root, ellipsoid, params, is_loaded, visibility, prev_state)
}

/// Like [`select`] but driven from a root [`Tile`] directly — for callers that own a growing tree (the
/// streaming scene grafts external `.json` tilesets into it as it descends) rather than a parsed [`Tileset`].
/// `prev_state` is the [`SelectionState`] returned by the **previous** pass (pass an empty map for a cold /
/// stateless pass); it drives the hysteresis dead-band (`params.hysteresis`) AND the cross-frame anti-pop
/// **kick**. This pass's state is returned in [`Selection::state`] for the caller to feed back next frame.
pub fn select_tree(
    root: &Tile,
    ellipsoid: &Ellipsoid,
    params: &SelectParams,
    is_loaded: &mut dyn FnMut(&[u32]) -> bool,
    visibility: &mut dyn FnMut(DVec3, f64) -> Visibility,
    prev_state: &SelectionState,
) -> Selection {
    // Render AND state are built as **append logs** (not maps) so a kick can `truncate` a subtree's tentative
    // entries off in O(1) — a subtree's contributions are always a contiguous suffix. This is what keeps a
    // kicked descendant from leaking a stale `Rendered` result into next frame's state. Both collapse to
    // their final (deduped) forms once, after the walk.
    let mut render: Vec<Rc<SelectedTile>> = Vec::new();
    let mut load: Vec<SelectedTile> = Vec::new();
    let mut state_log: Vec<(Vec<u32>, SelectionResult)> = Vec::new();
    let mut ctx = Ctx {
        ellipsoid,
        params,
        is_loaded,
        visibility,
        prev_state,
        render: &mut render,
        load: &mut load,
        state: &mut state_log,
        warned_add: false,
    };
    let mut path = Vec::new();
    visit(root, &mut path, root.matrix(), None, &mut ctx);
    // Collapse the state log into the keyed map (each path appears once — a kicked subtree's entries were
    // truncated off, never duplicated). This is next frame's `prev_state`.
    let state: SelectionState = state_log.into_iter().collect();
    // Dedup by path (an ancestor used as fallback for several descendants, or pushed before a sibling kicked,
    // appears once). Collapsing into a map first drops the duplicate `Rc`s eagerly, so the survivor is the
    // sole owner and `try_unwrap` *moves* it out — only a genuinely-shared tile would clone. Draw order is
    // intentionally NOT sorted: the Geo engine is render-only / non-authoritative (ADR-0007).
    let mut dedup: HashMap<Vec<u32>, Rc<SelectedTile>> = HashMap::with_capacity(render.len());
    for rc in render {
        dedup.entry(rc.path.clone()).or_insert(rc);
    }
    let render: Vec<SelectedTile> = dedup
        .into_values()
        .map(|rc| Rc::try_unwrap(rc).unwrap_or_else(|rc| (*rc).clone()))
        .collect();
    Selection { render, load, state }
}

/// cesium-native's `loadingDescendantLimit`: when a fresh (nothing-shown-last-frame) refinement has more than
/// this many still-loading descendants, render the coarse ancestor instead of waiting (the kick) — bounds how
/// many holes a not-yet-ready subtree may show. Cesium's default; tune if pop-vs-coarse balance needs it.
const LOADING_DESCENDANT_LIMIT: u32 = 20;

struct Ctx<'a> {
    ellipsoid: &'a Ellipsoid,
    params: &'a SelectParams,
    is_loaded: &'a mut dyn FnMut(&[u32]) -> bool,
    visibility: &'a mut dyn FnMut(DVec3, f64) -> Visibility,
    /// Last pass's per-tile results — read for the hysteresis dead-band + the cross-frame kick.
    prev_state: &'a SelectionState,
    /// This pass's render **append log** (deduped after the walk); the kick `truncate`s a subtree off it.
    render: &'a mut Vec<Rc<SelectedTile>>,
    load: &'a mut Vec<SelectedTile>,
    /// This pass's per-tile results as an **append log**, parallel to `render` so the kick truncates both in
    /// lockstep (a kicked descendant leaves no `Rendered` entry). Collapsed to [`SelectionState`] after the walk.
    state: &'a mut Vec<(Vec<u32>, SelectionResult)>,
    /// Latches once this pass has warned about an unsupported `refine=ADD` tile, so we log it at most once
    /// per selection rather than once per ADD node per frame.
    warned_add: bool,
}

/// Build a `SelectedTile`. This allocates a `Vec<String>` of content URIs (+ the path), so it is called
/// ONLY when the tile is actually kept — a resident fallback or an SSE-leaf to request — never for the
/// transient interior nodes that just pass an inherited fallback down.
fn make_selected(path: &[u32], world: DMat4, tile: &Tile, distance: f64, center: DVec3, radius: f64) -> SelectedTile {
    SelectedTile {
        path: path.to_vec(),
        world_transform: world,
        content_uris: tile.content_uris().map(String::from).collect(),
        geometric_error: tile.geometric_error,
        distance,
        bounds_center: center,
        bounds_radius: radius,
    }
}

/// Visit a tile (cesium-native's `visitTile`), returning the [`TraversalDetails`] its subtree folds up. Side
/// effects land in `ctx`: tiles to draw appended to `render`, tiles to fetch pushed to `load`, each tile's
/// [`SelectionResult`] recorded in `state`. `nearest_loaded` is the nearest loaded **ancestor** — the coarse
/// stand-in to draw while finer tiles stream (`None` until some ancestor is resident).
fn visit(
    tile: &Tile,
    path: &mut Vec<u32>,
    world: DMat4,
    nearest_loaded: Option<Rc<SelectedTile>>,
    ctx: &mut Ctx,
) -> TraversalDetails {
    let (center, radius) = bounding_sphere_ecef(&tile.bounding_volume, &world, ctx.ellipsoid);
    // Frustum-aware LOD: out-of-range drops the subtree; off-view tiles are KEPT (shadow casters + camera-turn
    // readiness) but refine only to the coarser `culled_screen_space_error`; in-view tiles get full detail.
    let sse_target = match (ctx.visibility)(center, radius) {
        Visibility::Culled => return TraversalDetails::NEUTRAL, // out of range — no fetch/render, no kick pressure
        Visibility::Offscreen => ctx.params.culled_screen_space_error,
        Visibility::Visible => ctx.params.max_screen_space_error,
    };
    let distance = ((ctx.params.camera_position - center).length() - radius).max(0.0);
    let loaded = (ctx.is_loaded)(path);
    let prev = ctx.prev_state.get(path.as_slice()).copied();
    // "Drew on screen last frame" = its own content (`Rendered`) or its coarse fallback (`Kicked`).
    let rendered_last = matches!(prev, Some(SelectionResult::Rendered | SelectionResult::Kicked));

    // The coarse stand-in for this region: this tile if it's resident (built now, shared via Rc), else the
    // nearest resident ancestor handed down. We never build a SelectedTile for an unloaded interior node.
    let coverage: Option<Rc<SelectedTile>> = if loaded {
        Some(Rc::new(make_selected(path, world, tile, distance, center, radius)))
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

    // Refine criterion single-homed in `logi_geo_lod::should_refine` (audit M22), with the hysteresis
    // dead-band: a tile `Refined` last frame holds its split until SSE drops below `target/(1+hysteresis)`.
    let meets = !logi_geo_lod::should_refine(
        tile.geometric_error,
        distance,
        &logi_geo_lod::RefineParams {
            fov_y_radians: ctx.params.fov_y_radians,
            viewport_height: ctx.params.viewport_height,
            sse_target, // full detail in view; coarser when off-view (Offscreen)
            hysteresis: ctx.params.hysteresis,
        },
        matches!(prev, Some(SelectionResult::Refined)),
    );

    // RENDER at this level: this tile meets SSE, or is a leaf (nothing finer to refine into).
    if meets || tile.children.is_empty() {
        if loaded {
            ctx.render.push(coverage.expect("loaded ⇒ coverage is self"));
            ctx.state.push((path.clone(), SelectionResult::Rendered));
            return TraversalDetails {
                all_renderable: true,
                any_rendered_last_frame: rendered_last,
                not_yet_renderable: 0,
            };
        }
        // Not loaded → request it; meanwhile draw the coarse ancestor (if any) so the region isn't a hole.
        // Its OWN content wasn't drawn, so leave it absent from `state` (≠ Rendered for next frame's kick).
        ctx.load
            .push(make_selected(path, world, tile, distance, center, radius));
        if let Some(anc) = &coverage {
            ctx.render.push(Rc::clone(anc));
        }
        return TraversalDetails {
            all_renderable: false,
            any_rendered_last_frame: rendered_last,
            not_yet_renderable: 1,
        };
    }

    // REFINE: visit the children, folding their details. Remember the render+state log lengths first so a
    // kick can `truncate` the children's tentative entries back off (they're a contiguous suffix in both).
    let render_mark = ctx.render.len();
    let state_mark = ctx.state.len();
    let mut sub = TraversalDetails {
        all_renderable: true,
        any_rendered_last_frame: false,
        not_yet_renderable: 0,
    };
    for (i, child) in tile.children.iter().enumerate() {
        path.push(i as u32);
        let cd = visit(child, path, world * child.matrix(), coverage.clone(), ctx);
        path.pop();
        sub.all_renderable &= cd.all_renderable;
        sub.any_rendered_last_frame |= cd.any_rendered_last_frame;
        sub.not_yet_renderable = sub.not_yet_renderable.saturating_add(cd.not_yet_renderable);
    }

    // KICK: a fresh refinement (showed nothing last frame) that isn't fully loaded would draw holes — fall
    // back to the coarse tile when there'd be too many holes, or this tile is itself a ready stand-in.
    // `sub.any_rendered_last_frame` is DESCENDANTS-only, so a kicked coarse tile keeps kicking each frame
    // until ALL its descendants load, then switches atomically (no partial-LOD pop-in).
    let kick = !sub.all_renderable && !sub.any_rendered_last_frame;
    let will_kick = kick && (sub.not_yet_renderable > LOADING_DESCENDANT_LIMIT || loaded);
    if will_kick {
        // Drop the descendants' tentative renders AND state entries ("kick" them) — leaving no stale
        // `Rendered` behind — then draw the coarse stand-in and record this tile as `Kicked`.
        ctx.render.truncate(render_mark);
        ctx.state.truncate(state_mark);
        if let Some(cov) = &coverage {
            ctx.render.push(Rc::clone(cov)); // draw the coarse stand-in instead
        }
        if !loaded {
            ctx.load
                .push(make_selected(path, world, tile, distance, center, radius));
            // and keep loading it
        }
        ctx.state.push((path.clone(), SelectionResult::Kicked));
        return TraversalDetails {
            all_renderable: loaded,
            any_rendered_last_frame: sub.any_rendered_last_frame || rendered_last,
            not_yet_renderable: if loaded { 0 } else { 1 },
        };
    }
    // Refine accepted — the children render (already appended). Record `Refined` for the hysteresis dead-band.
    //
    // `ancestor_meets_sse` fallback load (plan §9.2 step 6): if this refining subtree is **active** (drew
    // something last frame) but isn't fully loaded AND has no loaded ancestor to fall back on (`coverage` is
    // `None` ⇒ when this tile is unloaded, `coverage == nearest_loaded`), enqueue THIS tile's own content so
    // a coarse stand-in becomes available for its still-loading descendants instead of holes. The
    // `any_rendered_last_frame` gate keeps it from eagerly loading the whole chain on a cold first frame; the
    // coarser ancestor's higher geometric error sorts it ahead of the fine leaves in the load queue.
    if !loaded && coverage.is_none() && sub.any_rendered_last_frame && !sub.all_renderable {
        ctx.load
            .push(make_selected(path, world, tile, distance, center, radius));
    }
    ctx.state.push((path.clone(), SelectionResult::Refined));
    TraversalDetails {
        any_rendered_last_frame: sub.any_rendered_last_frame || rendered_last,
        ..sub
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
            culled_screen_space_error: 64.0, // 4× — off-view tiles refine to a coarse shell
            hysteresis: 0.0,
        }
    }

    fn paths(tiles: &[SelectedTile]) -> BTreeSet<Vec<u32>> {
        tiles.iter().map(|t| t.path.clone()).collect()
    }

    /// An empty prior state ⇒ a cold / stateless pass (no hysteresis, no cross-frame kick).
    fn cold() -> SelectionState {
        SelectionState::new()
    }

    #[test]
    fn far_camera_renders_only_the_root() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        let sel = select(
            &ts,
            &e,
            &params_at(200_000.0),
            &mut |_| true,
            &mut |_, _| Visibility::Visible,
            &cold(),
        );
        assert_eq!(paths(&sel.render), BTreeSet::from([vec![]])); // just the root
        assert!(sel.load.is_empty());
    }

    #[test]
    fn near_camera_refines_to_leaves() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        // Everything loaded ⇒ never a kick (all_renderable everywhere) ⇒ plain SSE refinement to the leaves.
        let sel = select(
            &ts,
            &e,
            &params_at(2000.0),
            &mut |_| true,
            &mut |_, _| Visibility::Visible,
            &cold(),
        );
        assert_eq!(
            paths(&sel.render),
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]])
        );
        assert!(sel.load.is_empty());
    }

    #[test]
    fn one_unloaded_grandchild_kicks_its_subtree_to_the_loaded_parent() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        // Grandchild [0,0] isn't resident; everything else is. A cold pass (nothing shown last frame).
        let mut is_loaded = |p: &[u32]| p != [0, 0];
        let sel = select(
            &ts,
            &e,
            &params_at(2000.0),
            &mut is_loaded,
            &mut |_, _| Visibility::Visible,
            &cold(),
        );
        // The KICK: [0]'s subtree isn't fully loaded and showed nothing last frame, so rather than a partial
        // refinement (fine [0,1] beside a coarse stand-in for [0,0]), the loaded parent [0] renders coarse
        // for its whole region — [0,1]'s tentative render is kicked. [1]'s subtree is fully loaded → it
        // refines to its grandchildren. Only [0,0] is queued to load.
        assert_eq!(paths(&sel.render), BTreeSet::from([vec![0], vec![1, 0], vec![1, 1]]));
        assert_eq!(paths(&sel.load), BTreeSet::from([vec![0, 0]]));
    }

    #[test]
    fn fresh_unloaded_subtree_kicks_to_root_and_loads_all_leaves() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        // Only the root is resident; the whole tree is a fresh, unloaded refinement → the root kicks (it is a
        // ready coarse stand-in) and renders alone, while all four leaves are queued.
        let mut is_loaded = |p: &[u32]| p.is_empty();
        let sel = select(
            &ts,
            &e,
            &params_at(2000.0),
            &mut is_loaded,
            &mut |_, _| Visibility::Visible,
            &cold(),
        );
        assert_eq!(paths(&sel.render), BTreeSet::from([vec![]])); // root only — no holes
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
        // Cull anything whose center is near the +Z pole point (the far child) — a hard `Culled` drop.
        let mut visibility = |center: DVec3, _r: f64| {
            if (center - DVec3::new(0.0, 0.0, C)).length() > 1.0 {
                Visibility::Visible
            } else {
                Visibility::Culled
            }
        };
        let sel = select(&ts, &e, &params_at(2000.0), &mut |_| true, &mut visibility, &cold());
        assert_eq!(paths(&sel.render), BTreeSet::from([vec![0]])); // only the near child; far one culled
    }

    #[test]
    fn offscreen_tiles_refine_to_the_coarser_culled_sse() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        // Offset 3000 m → the mid-children (gE 100) sit at SSE ≈ 31 px: ABOVE the 16 px view target (so an
        // in-view tile refines past them) but BELOW the 64 px culled target (so an off-view tile stops there).
        // The visibility class is the only thing that flips the children's refinement.
        let p = params_at(3000.0);
        let visible = select(&ts, &e, &p, &mut |_| true, &mut |_, _| Visibility::Visible, &cold());
        assert_eq!(
            paths(&visible.render),
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]]),
            "in view ⇒ full detail (refine to grandchildren)"
        );
        let offscreen = select(&ts, &e, &p, &mut |_| true, &mut |_, _| Visibility::Offscreen, &cold());
        assert_eq!(
            paths(&offscreen.render),
            BTreeSet::from([vec![0], vec![1]]),
            "off view ⇒ coarse shell (stop at the children, the culled SSE target)"
        );
    }

    #[test]
    fn kick_keeps_coarse_until_a_subtree_fully_loads_then_refines() {
        // Golden cross-frame trace (§9.5): grandchildren stream in over three frames; the selection state
        // persists between them. The kick must show coarse (no holes), refine each subtree atomically the
        // moment it's fully loaded, and never pop backward.
        use std::collections::HashSet;
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        let p = params_at(2000.0); // near → wants the gE-10 grandchildren
        let mut loaded: HashSet<Vec<u32>> = HashSet::from([vec![], vec![0], vec![1]]); // mids loaded, leaves not
        let mut state = cold();

        // Frame 1: no grandchild is loaded → each loaded mid-tile KICKS to its own coarse content (no holes);
        // all four grandchildren are queued.
        let s1 = select(
            &ts,
            &e,
            &p,
            &mut |q: &[u32]| loaded.contains(q),
            &mut |_, _| Visibility::Visible,
            &state,
        );
        assert_eq!(
            paths(&s1.render),
            BTreeSet::from([vec![0], vec![1]]),
            "frame 1: coarse mid-tiles while grandchildren load"
        );
        assert_eq!(
            paths(&s1.load),
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]])
        );
        state = s1.state;

        // Frame 2: [0]'s grandchildren arrive (not [1]'s) → [0] refines atomically to its grandchildren while
        // [1] still kicks to coarse. Per-subtree atomic: never a half-refined [0].
        loaded.extend([vec![0, 0], vec![0, 1]]);
        let s2 = select(
            &ts,
            &e,
            &p,
            &mut |q: &[u32]| loaded.contains(q),
            &mut |_, _| Visibility::Visible,
            &state,
        );
        assert_eq!(
            paths(&s2.render),
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1]]),
            "frame 2: [0] fully loaded → fine; [1] still coarse"
        );
        state = s2.state;

        // Frame 3: [1]'s grandchildren arrive → full detail everywhere; the traversal has converged.
        loaded.extend([vec![1, 0], vec![1, 1]]);
        let s3 = select(
            &ts,
            &e,
            &p,
            &mut |q: &[u32]| loaded.contains(q),
            &mut |_, _| Visibility::Visible,
            &state,
        );
        assert_eq!(
            paths(&s3.render),
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]]),
            "frame 3: converged to full detail"
        );
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

    #[test]
    fn active_subtree_with_no_loaded_ancestor_loads_a_coarse_fallback() {
        // §9.2 step 6 (`ancestor_meets_sse`): once a refinement is *active* (a descendant drew last frame)
        // but isn't fully loaded and has no loaded ancestor, the traversal loads the interior tile as a coarse
        // stand-in — so its still-loading siblings show coarse instead of holes. The gate is "active": this
        // must NOT fire on a cold frame (else it eagerly loads the whole chain).
        use std::collections::HashSet;
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        let p = params_at(2000.0); // near → wants the grandchildren
                                   // Only one grandchild is resident; crucially neither the root nor the mid-tile [0] is loaded.
        let loaded: HashSet<Vec<u32>> = HashSet::from([vec![0, 0]]);

        // Frame 1 (cold): [0,0] renders; [0,1]/[1,*] queue. The active-subtree fallback must NOT fire yet —
        // nothing rendered last frame — so the interior [0] is NOT in the load set.
        let s1 = select(
            &ts,
            &e,
            &p,
            &mut |q: &[u32]| loaded.contains(q),
            &mut |_, _| Visibility::Visible,
            &cold(),
        );
        assert!(
            paths(&s1.render).contains(&vec![0, 0]),
            "the one loaded grandchild renders"
        );
        assert!(
            !paths(&s1.load).contains(&vec![0]),
            "cold frame: no eager ancestor load — only the unloaded leaves queue"
        );
        assert_eq!(paths(&s1.load), BTreeSet::from([vec![0, 1], vec![1, 0], vec![1, 1]]));

        // Frame 2: [0]'s subtree is now active ([0,0] rendered last frame) but still incomplete ([0,1] not
        // loaded) and has no loaded ancestor → [0] is loaded as the coarse fallback. [1]'s subtree drew
        // nothing last frame, so its interior is NOT loaded — the load stays bounded to active subtrees.
        let s2 = select(
            &ts,
            &e,
            &p,
            &mut |q: &[u32]| loaded.contains(q),
            &mut |_, _| Visibility::Visible,
            &s1.state,
        );
        assert!(
            paths(&s2.load).contains(&vec![0]),
            "active, ancestor-less, incomplete subtree [0] loads its coarse fallback"
        );
        assert!(
            !paths(&s2.load).contains(&vec![1]),
            "inactive subtree [1] does not load an interior fallback"
        );
    }

    #[test]
    fn state_records_the_interior_splits_and_leaf_renders() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        // Near camera, all loaded → root + both mid-children Refined; the four grandchildren Rendered.
        let sel = select(
            &ts,
            &e,
            &params_at(2000.0),
            &mut |_| true,
            &mut |_, _| Visibility::Visible,
            &cold(),
        );
        let refined: BTreeSet<Vec<u32>> = sel
            .state
            .iter()
            .filter(|(_, &r)| r == SelectionResult::Refined)
            .map(|(p, _)| p.clone())
            .collect();
        assert_eq!(refined, BTreeSet::from([vec![], vec![0], vec![1]]));
        let rendered: BTreeSet<Vec<u32>> = sel
            .state
            .iter()
            .filter(|(_, &r)| r == SelectionResult::Rendered)
            .map(|(p, _)| p.clone())
            .collect();
        assert_eq!(
            rendered,
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]])
        );
    }

    #[test]
    fn hysteresis_holds_refined_children_in_the_dead_band() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        // Offset 6680 m → the mid-children (geometricError 100) sit at SSE ≈ 14 px, inside the [12.8, 16]
        // dead-band for H = 0.25 (target 16). The root (gE 1000, SSE ≈ 140) always refines; the grandchildren
        // (gE 10) are leaves. So the children's split is the ONLY decision the dead-band can flip.
        let mut p = params_at(6680.0);

        // Cold state: children are below the plain target → stop at (render) the children.
        let stateless = select(&ts, &e, &p, &mut |_| true, &mut |_, _| Visibility::Visible, &cold());
        assert_eq!(
            paths(&stateless.render),
            BTreeSet::from([vec![0], vec![1]]),
            "cold: SSE 14 < 16 ⇒ render the children, do not split"
        );

        // Hysteretic: same view, but the children were `Refined` last frame → they hold their split because
        // SSE 14 > 16/(1+0.25)=12.8. This is the anti-pop dead-band: no parent↔children flip on the threshold.
        p.hysteresis = 0.25;
        let prev = SelectionState::from([
            (vec![0u32], SelectionResult::Refined),
            (vec![1u32], SelectionResult::Refined),
        ]);
        let hyst = select(&ts, &e, &p, &mut |_| true, &mut |_, _| Visibility::Visible, &prev);
        assert_eq!(
            paths(&hyst.render),
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]]),
            "hysteresis keeps the previously-refined children split into grandchildren"
        );
    }

    const WGS84_ISH: f64 = 6_378_137.0;
}
