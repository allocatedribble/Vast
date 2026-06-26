//! 3D Tiles **tile selection** — the screen-space-error traversal that turns a camera view over a
//! tileset BVH into the tiles to render + the tiles to load, for the actorless 3D-Tiles stack
//! (ADR-0009, Milestone F). This is the Rust generalisation of the kernel's regular-quadtree
//! `select_hierarchical` to cesium-native's pointer-tree.
//!
//! Implements: per-volume distance (region/box/sphere → an ECEF bounding sphere, plan §9.4 globe geometry),
//! the SSE refine decision (the same model as `logi_geo_lod` / 3D Tiles) with a **hysteresis dead-band**,
//! both `REPLACE` and `ADD` refinement (per-tile, inherited), and the cesium-native **cross-frame kick** (plan §9.2): a per-tile
//! [`SelectionResult`] is folded up as [`TraversalDetails`] and persisted as a [`SelectionState`] so a fresh,
//! not-yet-loaded refinement renders the coarse ancestor instead of a hole-y partial split, then switches to
//! the fine LOD **atomically** once the subtree fully loads (and never pops backward once it's been shown).
//! The per-tile load state + a [`Visibility`] classification are injected as predicates, so the streaming
//! loop (`logi_3dtiles_scene`) owns frustum/distance culling. **Frustum-aware LOD** is wired (§9.2 culled-SSE):
//! in-view tiles refine to `max_screen_space_error`, off-view-but-in-range tiles to the coarser
//! `culled_screen_space_error` (a resident coarse shell — shadow-safe, since the engine casts shadows from a
//! larger-than-view frustum — never hard-culled by direction; only distance hard-culls). Also done: the
//! `ancestor_meets_sse` coarse-fallback load (§9.2 step 6) and, in the Geo World, the view-direction fetch
//! priority (§9.2 step 7) + the ECEF view frustum. `ADD` refinement is implemented (REPLACE/ADD chosen
//! per-tile and inherited): an ADD tile renders its own content alongside its refined children, so the kick +
//! `ancestor_meets_sse` coarse fallback are gated off for ADD (it has no holes). Both lanes are unit-tested.

#![forbid(unsafe_code)]

use glam::{DMat4, DVec3};
use hashbrown::HashMap;
use logi_3dtiles_tileset::{subdivide_bounds, BoundingVolume, ImplicitTiling, Refine, SubdivisionScheme, Tile};
use logi_geospatial::{Cartographic, Ellipsoid};
use std::rc::Rc;

// Implicit-tiling note: the Morton (Z-order) interleave + availability bit-indexing live in the
// `logi_3dtiles_implicit` codec and are invoked PROVIDER-side (the scene's `AvailabilityProvider` impl calls
// `Subtree::is_tile_available`, which interleaves the global coordinate this traversal passes it). The
// traversal therefore never reimplements Morton: it passes global `(level, x, y, z)` coordinates and tracks
// only the within-parent child index (the low Morton bits, `dx | dy<<1 | dz<<2`) for the tile `path`. (The
// codec is a dev-dependency here, used by the implicit-path unit tests to build in-memory `Subtree`s.)

// One screen-space-error model for the whole engine: reuse the kernel's canonical implementation
// (`logi_geo_lod`) instead of a second copy here. Same formula, with its degenerate-FOV/zero-distance
// clamps. The 3D-Tiles selection traversal below calls it exactly as the quadtree streamer does.
pub use logi_geo_lod::screen_space_error;

/// Canonical 64-bit tile identity: FNV-1a over the path's little-endian segment bytes. The single home of
/// the hash the whole stack keys on — the entity `TileId`, the `FlashCache` key, and (published as
/// `LogiGlobeTile.tile_key`) the C++ section-cache key. [`PathArena`] computes it INCREMENTALLY per node
/// (FNV-1a is a sequential fold, so a child's key extends its parent's), guaranteed identical to this
/// definition; tests and path-only callers use this free function directly.
pub fn path_key(path: &[u32]) -> u64 {
    let mut h = FNV_OFFSET;
    for &seg in path {
        h = fnv_extend(h, seg);
    }
    h
}

const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

/// Extend an FNV-1a state by one path segment's 4 LE bytes — the incremental step [`PathArena`] applies
/// per interned child so `arena.key(node) == path_key(path_of(node))` holds by construction.
#[inline]
fn fnv_extend(mut h: u64, seg: u32) -> u64 {
    for b in seg.to_le_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// One interned tile-path node: its parent id + the child-index step that reaches it, with the derived
/// depth and the canonical [`path_key`] cached at intern time.
struct ArenaNode {
    parent: u32,
    step: u32,
    depth: u32,
    key: u64,
}

/// Sentinel "no node" id (also the root's parent). The arena can never hold `u32::MAX` real nodes (the
/// edge map alone would exceed memory long before), so the sentinel is unambiguous.
pub const NO_NODE: u32 = u32::MAX;
/// The interned id of the root path `[]` — always the first node of every [`PathArena`].
pub const ROOT_NODE: u32 = 0;

/// **Path-interning arena** (audit 2026-06-10 §3.2): maps every discovered tile path to a stable small
/// `u32` node id, so the per-tile maps that used to key on `Vec<u32>` (the selection state here; the
/// scene's index / failure / retry / in-flight bookkeeping) key on a `u32` instead — O(1) probes with no
/// per-segment byte hashing, and no per-node `Vec` clones in the traversal's state log (previously O(V·d)
/// per pass). Ids are stable for the arena's lifetime (the scene owns one arena per streaming world), so
/// they are valid cross-frame identities. The canonical 64-bit [`path_key`] is computed incrementally at
/// intern time and cached per node.
///
/// Growth: append-only — nodes accumulate with explored extent (~32 B/node + an edge-map entry). A planet
/// fly-through discovering 1M tiles costs ~50 MB; bounding this (free-listing pruned graft subtrees, or an
/// epoch compaction) is DEFERRED until telemetry shows it matters — every id consumer would need epoch
/// coordination, and the cold-graft prune already bounds the much larger `Tile` tree itself.
pub struct PathArena {
    nodes: Vec<ArenaNode>,
    /// `(parent, step) → child` edge map — the intern lookup. u32-pair keys hash in one shot.
    edges: HashMap<(u32, u32), u32>,
}

impl Default for PathArena {
    fn default() -> Self {
        Self::new()
    }
}

impl PathArena {
    pub fn new() -> Self {
        Self {
            nodes: vec![ArenaNode {
                parent: NO_NODE,
                step: 0,
                depth: 0,
                key: FNV_OFFSET, // == path_key(&[])
            }],
            edges: HashMap::new(),
        }
    }

    /// Intern (or look up) the child of `parent` reached by child-index `step`. Amortized O(1).
    #[inline]
    pub fn child_of(&mut self, parent: u32, step: u32) -> u32 {
        if let Some(&id) = self.edges.get(&(parent, step)) {
            return id;
        }
        let p = &self.nodes[parent as usize];
        let node = ArenaNode {
            parent,
            step,
            depth: p.depth + 1,
            key: fnv_extend(p.key, step),
        };
        let id = self.nodes.len() as u32;
        self.nodes.push(node);
        self.edges.insert((parent, step), id);
        id
    }

    /// Intern a whole path from the root — O(d); used by tests and rare scene paths (graft targets).
    pub fn intern_path(&mut self, path: &[u32]) -> u32 {
        let mut n = ROOT_NODE;
        for &step in path {
            n = self.child_of(n, step);
        }
        n
    }

    /// The node's parent id (`None` for the root).
    #[inline]
    pub fn parent(&self, id: u32) -> Option<u32> {
        let p = self.nodes[id as usize].parent;
        (p != NO_NODE).then_some(p)
    }

    /// Tree depth (root = 0) — equals `path_of(id).len()`.
    #[inline]
    pub fn depth(&self, id: u32) -> u32 {
        self.nodes[id as usize].depth
    }

    /// The canonical [`path_key`] of this node's path, cached at intern time.
    #[inline]
    pub fn key(&self, id: u32) -> u64 {
        self.nodes[id as usize].key
    }

    /// Reconstruct the node's child-index path (root-first) — O(d) with one allocation; for the rare
    /// path-shaped consumers (tree grafting/walking), never the per-tile hot loops.
    pub fn path_of(&self, id: u32) -> Vec<u32> {
        let mut out = vec![0u32; self.nodes[id as usize].depth as usize];
        let mut n = id;
        for slot in out.iter_mut().rev() {
            let node = &self.nodes[n as usize];
            *slot = node.step;
            n = node.parent;
        }
        out
    }

    /// Is `ancestor` an ancestor of — or equal to — `node`? O(depth difference) parent hops.
    pub fn is_ancestor_or_self(&self, ancestor: u32, node: u32) -> bool {
        let target_depth = self.nodes[ancestor as usize].depth;
        let mut n = node;
        while self.nodes[n as usize].depth > target_depth {
            n = self.nodes[n as usize].parent;
        }
        n == ancestor
    }

    /// Interned node count (telemetry — watch growth over a long session).
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        false // the root always exists
    }
}

/// The per-frame view + LOD parameters that drive selection (camera in ECEF, plan §9.4).
#[derive(Clone, Copy, Debug)]
pub struct SelectParams {
    pub camera_position: DVec3,
    pub fov_y_radians: f64,
    pub viewport_height: f64,
    /// Refine while a tile's screen-space error exceeds this many pixels (3D Tiles `maxScreenSpaceError`).
    pub max_screen_space_error: f64,
    /// The SSE target for [`Visibility::Distant`] tiles — INSIDE the frustum but beyond the caller's
    /// full-detail band. These are *visible* (the far vista), so the demotion is mild: typically ~2×
    /// `max_screen_space_error`. Set it equal to `max_screen_space_error` to disable the distinction.
    pub distant_screen_space_error: f64,
    /// The (much looser) SSE target for tiles the visibility predicate reports [`Visibility::Offscreen`] —
    /// outside the view frustum but kept resident (for shadows the engine casts from a larger-than-view
    /// frustum, and so a camera turn finds them already coarse-loaded). These are *invisible*, so the
    /// demotion should be aggressive AND floored by the caller (cesium's `culledScreenSpaceError` is 64 px
    /// vs a 16 px max): without a floor, a fine `max_screen_space_error` knob makes the whole off-frustum
    /// hemisphere refine near-full — the 2026-06-10 ground "cut flood" (19k published tiles at 2.5 km,
    /// max_sse=1, the entire hidden disc at 2 px).
    pub culled_screen_space_error: f64,
    /// Hysteresis dead-band fraction for the refine decision (`0.0` = stateless plain threshold). A tile that
    /// was `Refined` last pass holds its split until SSE drops below `max_screen_space_error / (1 + hysteresis)`,
    /// so a camera hovering on the threshold can't flip parent↔children every frame (anti-pop). Only takes
    /// effect when `prev_state` carries the previous pass's [`SelectionResult`]s.
    pub hysteresis: f64,
}

/// How a tile's bounding sphere relates to the view this frame — the streaming visibility classification
/// (the predicate the [`select_tree_implicit`] caller supplies). **Frustum-aware LOD, not a hard cull:** `Offscreen` tiles
/// are still streamed (so the engine can cast their shadows from its larger-than-view shadow frustum, and so
/// camera turns find them ready), just at the coarser `culled_screen_space_error`. Only `Culled` drops a
/// subtree — reserve it for genuinely out-of-range tiles (e.g. beyond the streaming distance).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Visibility {
    /// Inside the view frustum (+ any edge margin) and within the full-detail band — refine to the full
    /// `max_screen_space_error`.
    Visible,
    /// Inside the frustum but beyond the full-detail band — the VISIBLE far vista; mildly demoted to
    /// `distant_screen_space_error` (typically ~2× max).
    Distant,
    /// Outside the view frustum but kept resident — INVISIBLE; aggressively demoted to the floored
    /// `culled_screen_space_error` coarse shell.
    Offscreen,
    /// Out of range entirely — drop the whole subtree (no fetch, no render).
    Culled,
}

/// A tile chosen to render or load. `path` is the child-index path from the root; `node` is its interned
/// [`PathArena`] id (the key the per-tile maps use) and `key` the canonical [`path_key`] hash.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectedTile {
    pub path: Vec<u32>,
    /// Interned [`PathArena`] id — the stable O(1) identity the caller's per-tile maps key on.
    pub node: u32,
    /// Canonical 64-bit [`path_key`] of `path` (cached from the arena) — the FlashCache / FFI tile key.
    pub key: u64,
    pub world_transform: DMat4,
    /// Content URIs to fetch — populated ONLY for `load` tiles (audit 2026-06-10 §4.7). A `render` tile's
    /// URIs are never consumed downstream (renderers key by `node`/`key`), and building them eagerly
    /// allocated a `String` per URI for every loaded interior node per pass — so the render flavor leaves
    /// this empty. (The traversal builds the same tile in both flavors when it is both rendered and queued.)
    pub content_uris: Vec<String>,
    pub geometric_error: f64,
    pub distance: f64,
    /// The tile's bounding sphere in ECEF (metres) — already computed during the walk, surfaced so a
    /// consumer (the ECS Geo World) can populate per-tile bounds without recomputing it.
    pub bounds_center: DVec3,
    pub bounds_radius: f64,
    /// `true` if this tile uses ADD refinement (renders *in addition to* its children, by design). An ADD tile
    /// is never hole-clipped — its content is meant to show alongside its descendants — so the post-process
    /// neither populates its `clip_children` nor clips a descendant against it.
    pub is_add: bool,
    /// **Geometric hole-clip** relationship (empty for almost every tile): `(group, key)` for each finer
    /// tile drawn over this one — its *immediate rendered descendants*, the loaded LOD frontier directly
    /// beneath it. Populated by the post-process in [`select_tree_implicit`] only when this tile is a coarse
    /// REPLACE stand-in covering a region where finer tiles have loaded. The selector deliberately emits the
    /// *relationship*, not clip geometry: a tileset bounding volume is only a loose cover of the actual mesh
    /// (3D Tiles guarantees a child's content lies inside it, not that it's tight), so the scene materializes
    /// each child's clip box from its decoded mesh's true AABB — which only the scene holds — and the renderer
    /// discards this tile's fragments inside those boxes. Coarse + fine then tile the ground edge-to-edge with
    /// no coplanar overlap (no z-fight, no PDO). (Keys, not paths: the scene resolves a clip child by its
    /// `tile_key` anyway.) `group` = the FIRST path step below this stand-in (which DIRECT-CHILD subtree the
    /// descendant lies in, ≤4 quadtree / ≤8 octree values) — the scene UNIONS each group's member boxes into
    /// one box when they tile the subtree densely, so a skip-level cut with dozens of descendants can't
    /// overflow the renderer's per-tile clip-box cap (the 2026-06-11 fast-camera coarse-over-fine sheets).
    pub clip_children: Vec<(u8, u64)>,
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

/// Per-tile [`SelectionResult`] from one pass, keyed by interned [`PathArena`] node id — the state a
/// stateful caller feeds back next frame (cesium-native's per-tile `TileSelectionState`, minus the frame
/// number: the *whole map* is "last frame"). Node ids are arena-stable, so the map is valid across frames
/// as long as the SAME arena is passed back. An empty map ⇒ a cold / stateless pass (no hysteresis, no
/// cross-frame anti-pop state — the cold kick still fires). u32 keys: O(1) probes, no per-segment hashing,
/// no per-entry `Vec` clone (the old `HashMap<Vec<u32>, _>` cost O(V·d) per pass — audit 2026-06-10 §3.2).
pub type SelectionState = hashbrown::HashMap<u32, SelectionResult>;

/// Per-pass "why is this region coarser than SSE wants" counters — the selection X-RAY (2026-06-11, built
/// to attribute the stuck-coarse scars). SNAPSHOT semantics: one pass's counts, never cumulative. A healthy
/// converging scene shows `loading`/`kick` falling to zero; **a scar is `leaf_under_sse` (or the scene-side
/// grafted-skip counter) sustained at quiescence** — a loaded tile rendering coarser than the SSE target
/// with nothing below it to refine into.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HoldCounts {
    /// Kicked subtrees: the coarse stand-in drawn while descendants load/texture (normal during convergence).
    pub kick: u32,
    /// Dead footprints covered by an ancestor (terminal fetch/decode failures below — never re-queued).
    pub dead: u32,
    /// Unloaded tiles queued to load this pass (covered by an ancestor meanwhile). Sustained nonzero at an
    /// IDLE fetch pipe ⇒ the loads are being dropped downstream (see the scene's grafted-skip counter).
    pub loading: u32,
    /// LOADED tiles rendered COARSER than the SSE target with NO children to refine into — a genuine tree
    /// leaf at extreme zoom, an implicit branch tip, or a graft whose children are MISSING (the scar suspect).
    pub leaf_under_sse: u32,
    /// Implicit children answered [`TileAvailability::Pending`] (their `.subtree` not fetched yet) — the
    /// walk holds the coarse ancestor until it arrives.
    pub pending_subtree: u32,
}

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
    /// The pass's hold-reason X-ray (see [`HoldCounts`]).
    pub holds: HoldCounts,
}

// ---- Implicit tiling: the availability-provider seam ----------------------------------------------
//
// A tile flagged `implicitTiling` is the root of a procedurally-subdivided quadtree/octree (3D Tiles 1.1
// `ImplicitTiling`). Its descendants are NOT stored as `tile.children`; they exist iff the bitstreams in the
// `.subtree` files say so. Those binary files must be FETCHED — a concern the streaming/scene layer owns, not
// this traversal. So selection takes an [`AvailabilityProvider`]: given a tile's implicit context + a
// candidate descendant's coordinates, it answers "is this tile available? / is its content available?" from
// the decoded `Subtree`s it holds (decoding them with `logi_3dtiles_implicit`), and signals when the subtree
// covering a coordinate isn't fetched yet so the scene can fetch it. The traversal owns the *geometry*
// (bounds subdivision, geometric error, Morton code, template-URL substitution); the provider owns the
// *availability data*. An [`explicit-only tileset`](no_availability) passes [`NoImplicit`], whose every answer
// is "unavailable" — so a tileset without implicit tiling behaves EXACTLY as before this seam existed.

/// The static context of one implicit-tiling root, handed to the [`AvailabilityProvider`] with every query so
/// it can resolve a global coordinate against the right `.subtree` (e.g. by `root_path` + the subdivision
/// scheme / `subtree_levels` in `tiling`). All fields are frame-invariant for a given implicit root.
#[derive(Clone, Copy, Debug)]
pub struct ImplicitContext<'a> {
    /// Child-index path from the tileset root to the tile carrying `implicitTiling` — the implicit root, the
    /// stable identity the provider keys its per-root subtree set by. (The implicit tree's level 0 is this
    /// tile; synthesized descendants extend below it sharing this `path` prefix — their `SelectedTile::path`
    /// is this prefix followed by one child-index step per implicit level, `dx | dy<<1 | dz<<2`.)
    pub root_path: &'a [u32],
    /// The implicit root's interned [`PathArena`] id — the same identity as `root_path`, for providers whose
    /// bookkeeping is node-keyed (the scene's grafted-subtree base map). Resolve ancestors via the `arena`
    /// the provider methods receive.
    pub root_node: u32,
    /// The root's `implicitTiling`: subdivision scheme (quadtree/octree), `subtree_levels` (how many levels
    /// each `.subtree` file spans), `available_levels` (the number of levels that can exist (a count; level 0
    /// is the implicit root, so the deepest valid level is `available_levels - 1`)), and the `subtrees`
    /// template URI. The provider uses the scheme + `subtree_levels` to map a global coord to a subtree file.
    pub tiling: &'a ImplicitTiling,
}

/// Whether a candidate implicit tile is available — the provider's answer to a tile-availability query. The
/// third state, [`Pending`](Self::Pending), is the seam's load signal: the `.subtree` covering the coordinate
/// isn't decoded yet, so the traversal must NOT descend into it (its availability is unknown), and the
/// provider has noted that the scene should fetch that subtree. Treating `Pending` as a refinement boundary
/// (render the coarse ancestor, like an unloaded explicit tile) is what makes implicit streaming converge:
/// next frame, once the subtree is fetched, the same coordinate answers [`Available`](Self::Available) and the
/// traversal descends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TileAvailability {
    /// The tile exists in this subtree — descend into / render it.
    Available,
    /// The tile does not exist (its availability bit is 0) — prune this branch.
    Unavailable,
    /// The `.subtree` covering this coordinate isn't fetched/decoded yet — availability unknown. The provider
    /// has recorded the need to fetch it; the traversal treats it as a not-yet-loaded refinement boundary.
    Pending,
}

/// The seam the scene implements to feed subtree **availability** into the implicit traversal. The traversal
/// calls it ONLY for tiles under an `implicitTiling` root, with global coordinates `(level, x, y, z)` relative
/// to that root (`level == 0` is the implicit root itself; quadtree ignores `z`). The implementation maps the
/// coordinate to the `.subtree` that covers it (using `ctx.tiling.subtree_levels` + the subdivision scheme),
/// decodes that file via `logi_3dtiles_implicit::parse_subtree`, and answers from its bitstreams
/// ([`Subtree::is_tile_available`] / [`is_content_available`]). Fetching subtrees on demand + caching decoded
/// `Subtree`s is the provider's job; this traversal stays fetch-free and deterministic.
///
/// `&mut self` so an implementation can record "fetch this subtree" / "this coord is pending" as it answers.
/// `arena` is the traversal's [`PathArena`] (read-only), so a node-keyed provider (the scene's grafted-base
/// map) can walk `ctx.root_node`'s ancestors without aliasing the traversal's mutable arena borrow.
pub trait AvailabilityProvider {
    /// Is the tile at global `(level, x, y, z)` (relative to `ctx.root_path`) available? See
    /// [`TileAvailability`] — `Pending` signals the covering `.subtree` must be fetched first.
    fn tile_availability(
        &mut self,
        ctx: &ImplicitContext,
        arena: &PathArena,
        level: u32,
        x: u32,
        y: u32,
        z: u32,
    ) -> TileAvailability;

    /// Is **content** available at global `(level, x, y, z)`? (A tile may exist without renderable content — an
    /// interior node that only subdivides.) Only called for tiles already reported [`TileAvailability::Available`];
    /// a `false` answer means "this tile exists but has no content URI to fetch" (so it's a pure interior node).
    fn content_availability(
        &mut self,
        ctx: &ImplicitContext,
        arena: &PathArena,
        level: u32,
        x: u32,
        y: u32,
        z: u32,
    ) -> bool;
}

/// The no-op [`AvailabilityProvider`] for explicit-only tilesets: every tile is `Unavailable`, so an
/// `implicitTiling` root (if one somehow appears without a provider) simply refines to nothing extra and the
/// implicit seam adds no tiles to the traversal. Pass it to [`select_tree_implicit`] for an explicit-only
/// tileset; the scene calls [`select_tree_implicit`] with a real provider once it can fetch subtrees.
pub struct NoImplicit;

impl AvailabilityProvider for NoImplicit {
    #[inline]
    fn tile_availability(
        &mut self,
        _: &ImplicitContext,
        _: &PathArena,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
    ) -> TileAvailability {
        TileAvailability::Unavailable
    }
    #[inline]
    fn content_availability(&mut self, _: &ImplicitContext, _: &PathArena, _: u32, _: u32, _: u32, _: u32) -> bool {
        false
    }
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

    /// The identity a parent starts a refine fold from (before folding in any child): a fully-renderable,
    /// nothing-shown-last-frame, no-holes subtree. Each child is then `fold_child`-ed in. (Both lanes start
    /// here, so the explicit and implicit child folds are bit-identical.)
    const REFINING: Self = Self::NEUTRAL;

    /// Fold one child's subtree details into this parent accumulator: AND the all-renderable flag, OR the
    /// any-rendered-last-frame flag, and (saturating) sum the still-loading count. The single home of the
    /// child-fold so the explicit (`tile.children`) and implicit (synthesized) lanes combine children identically.
    #[inline]
    fn fold_child(&mut self, child: TraversalDetails) {
        self.all_renderable &= child.all_renderable;
        self.any_rendered_last_frame |= child.any_rendered_last_frame;
        self.not_yet_renderable = self.not_yet_renderable.saturating_add(child.not_yet_renderable);
    }
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
            // only under shear / anisotropic scale (rare).
            (center, s[3] * max_axis_scale(world))
        }
        BoundingVolume::Box(b) => {
            let center = world.transform_point3(DVec3::new(b[0], b[1], b[2]));
            let u = world.transform_vector3(DVec3::new(b[3], b[4], b[5]));
            let v = world.transform_vector3(DVec3::new(b[6], b[7], b[8]));
            let w = world.transform_vector3(DVec3::new(b[9], b[10], b[11]));
            // EXACT circumscribed-sphere radius = the farthest OBB corner from the center. The 8 corners are
            // `center ± u ± v ± w`; opposite corners share a magnitude, so the radius is the max over the 4 sign
            // combinations of `|±u ± v ± w|`. For the ORTHOGONAL axes 3D Tiles `box` volumes use in practice this
            // equals √(|u|²+|v|²+|w|²) — TIGHT. It replaces the old `|u|+|v|+|w|` L1 bound, which was up to √3×
            // too loose (a ~5× volume envelope): that slack over-streamed a band of view-edge tiles to full
            // detail AND shortened the SSE distance (→ over-refinement). Unlike a bare √Σ|·|², the per-corner max
            // stays a SAFE over-bound even under shear / non-orthogonal axes (where √Σ|·|² would *under*-bound and
            // risk culling a tile that pokes into the frustum) — important for hostile/adversarial tilesets.
            let radius = (u + v + w)
                .length()
                .max((u + v - w).length())
                .max((u - v + w).length())
                .max((u - v - w).length());
            (center, radius)
        }
        BoundingVolume::Region(r) => {
            // 8 ellipsoid corners → enclosing sphere: tight for small leaf regions (the real case); for a
            // very wide region the mid-arc surface bulge can sit just outside it (under-bound) — cesium
            // uses an oriented box there (regions are normally tiny leaves).
            let (west, south, mut east, north, min_h, max_h) = (r[0], r[1], r[2], r[3], r[4], r[5]);
            // Antimeridian wrap: a region with east < west crosses ±π. We unwrap east by +2π so the four
            // corner longitudes span the *short* way round, giving a correct (if conservative) sphere over
            // the seam instead of one swept across the whole globe. A wide wrapped region still over-refines
            // (same limitation as a wide non-wrapped one); it is never under-bounded unsafely.
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

/// Is `point` (ECEF) inside `bv`, interpreted as a 3D Tiles 1.1 `viewerRequestVolume`? `box`/`sphere` ride the
/// tile's `world` transform; `region` is geodetic and transform-independent (converted via the ellipsoid).
/// Used to gate request+refine of a tile that declares a request volume.
fn point_inside_volume(bv: &BoundingVolume, world: &DMat4, ellipsoid: &Ellipsoid, point: DVec3) -> bool {
    match bv {
        BoundingVolume::Sphere([cx, cy, cz, r]) => {
            let center = world.transform_point3(DVec3::new(*cx, *cy, *cz));
            (point - center).length() <= r * max_axis_scale(world)
        }
        BoundingVolume::Box(b) => {
            // Center + three half-axis vectors, all transformed by `world`. Inside the OBB iff the projection of
            // `point - center` onto each half-axis is within that half-axis's length: |d·a| <= a·a.
            let center = world.transform_point3(DVec3::new(b[0], b[1], b[2]));
            let lin = glam::DMat3::from_mat4(*world);
            let ax = lin * DVec3::new(b[3], b[4], b[5]);
            let ay = lin * DVec3::new(b[6], b[7], b[8]);
            let az = lin * DVec3::new(b[9], b[10], b[11]);
            let d = point - center;
            d.dot(ax).abs() <= ax.dot(ax) && d.dot(ay).abs() <= ay.dot(ay) && d.dot(az).abs() <= az.dot(az)
        }
        BoundingVolume::Region([w, s, e, n, min_h, max_h]) => {
            // Geodetic (radians + metres), transform-independent. (Antimeridian-crossing regions where w > e
            // are not handled — vanishingly rare for a request volume; the simple in-range test suffices.)
            let c = ellipsoid.cartesian_to_cartographic(point);
            c.longitude >= *w
                && c.longitude <= *e
                && c.latitude >= *s
                && c.latitude <= *n
                && c.height >= *min_h
                && c.height <= *max_h
        }
    }
}

/// [`bounding_sphere_ecef`] for a stored [`Tile`], with the **frame-invariant region memo (S3)**: a `region`
/// volume's ECEF sphere is transform-independent and never changes, so it is computed once (8
/// `cartographic_to_cartesian` calls — 24 trig ops) and cached on the tile ([`Tile::region_sphere_ecef`]),
/// then reused every subsequent selection pass. `box`/`sphere` volumes ride the (possibly changing) `world`
/// transform and are cheap (no trig), so they take the direct path. This is the hot caller — every explicit
/// tile, every frame — so the region memo removes the dominant per-region-tile cost from the steady state.
#[inline]
fn tile_bounding_sphere_ecef(tile: &Tile, world: &DMat4, ellipsoid: &Ellipsoid) -> (DVec3, f64) {
    match &tile.bounding_volume {
        BoundingVolume::Region(_) => {
            let (c, r) = tile.region_sphere_ecef(|| {
                let (c, r) = bounding_sphere_ecef(&tile.bounding_volume, world, ellipsoid);
                (c.to_array(), r) // store as plain arrays (the tileset crate's memo is glam-free)
            });
            (DVec3::from_array(c), r)
        }
        // box / sphere: transform-dependent + cheap — never memoized.
        _ => bounding_sphere_ecef(&tile.bounding_volume, world, ellipsoid),
    }
}

/// Select the tiles to render + load for `params`, driven from a root [`Tile`], with implicit tiling wired in
/// via an [`AvailabilityProvider`]. The pluggable predicates:
/// - `is_loaded(path, node)` — is that tile's content resident? (`path` for path-shaped callers/tests; `node`
///   the interned [`PathArena`] id for O(1) map-keyed callers — both name the same tile.)
/// - `is_dead(path, node)` — is that tile's content terminally dead (fetch/decode gave up)? A dead tile is
///   treated as ABSENT (not rendered, not requested, doesn't block its parent's refine). Pass
///   `&mut |_, _| false` for callers with no terminal-failure signal (e.g. explicit-only tilesets).
/// - `visibility(center, radius)` — the bounding sphere's [`Visibility`] (frustum-aware LOD; `|_, _|
///   Visibility::Visible` disables culling — everything at full detail).
///
/// `arena` is the caller-owned path interner: pass THE SAME arena every pass (node ids are the cross-frame
/// identities `prev_state` and the caller's maps key on). `prev_state` is the [`SelectionState`] returned by
/// the **previous** pass (pass an empty map for a cold / stateless pass); it drives the hysteresis dead-band
/// (`params.hysteresis`) AND the cross-frame anti-pop **kick**. This pass's state is returned in
/// [`Selection::state`] for the caller to feed back next frame.
///
/// For an explicit-only tileset (no `implicitTiling`), pass [`NoImplicit`] as the provider — every implicit
/// root then refines to nothing extra. When the walk reaches a tile whose `implicit_tiling.is_some()`, it
/// synthesizes that tile's quadtree/octree descendants from the provider's availability answers — subdividing
/// the root's bounds ([`subdivide_bounds`]), halving the geometric error per level, and substituting the
/// content/subtree template URIs — INSTEAD of descending its (normally empty) explicit `children`. Everything
/// else (SSE refine, REPLACE, the depth cap, the visibility gates, the cross-frame kick + hysteresis) is
/// identical to the explicit path: implicit children flow through the SAME per-node decision core. `provider`
/// is `&mut` so it can record subtree-fetch needs as it answers (a [`TileAvailability::Pending`] coordinate
/// becomes a not-yet-loaded refinement boundary).
#[allow(clippy::too_many_arguments)]
pub fn select_tree_implicit(
    root: &Tile,
    ellipsoid: &Ellipsoid,
    params: &SelectParams,
    arena: &mut PathArena,
    is_loaded: &mut dyn FnMut(&[u32], u32) -> bool,
    is_dead: &mut dyn FnMut(&[u32], u32) -> bool,
    visibility: &mut dyn FnMut(DVec3, f64) -> Visibility,
    prev_state: &SelectionState,
    provider: &mut dyn AvailabilityProvider,
) -> Selection {
    // Render AND state are built as **append logs** (not maps) so a kick can `truncate` a subtree's tentative
    // entries off in O(1) — a subtree's contributions are always a contiguous suffix. This is what keeps a
    // kicked descendant from leaking a stale `Rendered` result into next frame's state. Both collapse to
    // their final (deduped) forms once, after the walk.
    let mut render: Vec<Rc<SelectedTile>> = Vec::new();
    let mut load: Vec<SelectedTile> = Vec::new();
    let mut state_log: Vec<(u32, SelectionResult)> = Vec::new();
    let mut ctx = Ctx {
        ellipsoid,
        params,
        tan_half_fov: logi_geo_lod::tan_half_fov_y(params.fov_y_radians),
        arena,
        is_loaded,
        is_dead,
        visibility,
        prev_state,
        provider,
        render: &mut render,
        load: &mut load,
        state: &mut state_log,
        nearest_loaded: None,
        holds: HoldCounts::default(),
    };
    let mut path = Vec::new();
    // The root's refine mode is REQUIRED by spec; pass REPLACE as the inherited default for a malformed root
    // that omits it (matches Cesium's lenient default). `refine_or` in `visit` then uses the root's own mode.
    visit(root, &mut path, ROOT_NODE, root.matrix(), Refine::Replace, &mut ctx);
    let holds = ctx.holds;
    // Collapse the state log into the keyed map (each node appears once — a kicked subtree's entries were
    // truncated off, never duplicated). This is next frame's `prev_state`.
    let state: SelectionState = state_log.into_iter().collect();
    // Dedup by node id (an ancestor used as fallback for several descendants, or pushed before a sibling
    // kicked, appears once). Collapsing into a map first drops the duplicate `Rc`s eagerly, so the survivor is
    // the sole owner and `try_unwrap` *moves* it out — only a genuinely-shared tile would clone. Draw order is
    // intentionally NOT sorted: the Geo engine is render-only / non-authoritative (ADR-0007).
    let mut dedup: HashMap<u32, Rc<SelectedTile>> = HashMap::with_capacity(render.len());
    for rc in render {
        dedup.entry(rc.node).or_insert(rc);
    }
    let mut render: Vec<SelectedTile> = dedup
        .into_values()
        .map(|rc| Rc::try_unwrap(rc).unwrap_or_else(|rc| (*rc).clone()))
        .collect();
    // Deterministic draw order — sort by path (lexicographic == DFS pre-order). The engine is render-only /
    // non-authoritative (ADR-0007), so order doesn't affect render *correctness*; but a STABLE order makes the
    // published frame reproducible run-to-run (the dedup above drains a `HashMap`, whose iteration order is
    // seed-dependent) — a precondition for an incremental/diff selection that compares this frame's cut against
    // last frame's. And it's effectively free: `attach_clip_children` already required this exact ordering and
    // was paying its own O(R log R) sort of an index permutation to get it — now it consumes the sorted `render`
    // directly (one sort, not two).
    render.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    // Geometric hole-clip (see `SelectedTile::clip_children`): hand each coarse REPLACE stand-in the paths of
    // the finer tiles drawn over it, so the host can discard the overlap instead of z-fighting it.
    attach_clip_children(&mut render);
    Selection {
        render,
        load,
        state,
        holds,
    }
}

/// Cap on clip CANDIDATES carried per tile (the scene reduces them to ≤ the renderer's box cap by unioning
/// each direct-child GROUP — see `SelectedTile::clip_children`). 32 bounds the per-publish AABB work and the
/// per-tile Vec while comfortably covering a two-skip-level frontier; candidates are attached LARGEST-first
/// ([`attach_clip_children`]), so anything past the cap is the least-visible residue. The RENDERER's per-tile
/// box cap (`LOGI_CLIP_MAX` = 8 in the C++ material) is enforced scene-side at materialization, where the
/// group-union reduction makes overflow structurally rare (≤4 quadtree / ≤8 octree groups).
const MAX_CLIP_CANDIDATES_PER_TILE: usize = 32;

/// **Geometric hole-clip** post-process: for every rendered tile, attach to its `clip_children` the paths of
/// the finer tiles drawn over it — its *immediate rendered descendants* (the loaded LOD frontier directly
/// beneath it). A coarse REPLACE stand-in covering a partially-loaded region thereby learns exactly which finer
/// tiles took over; the scene materializes each child's clip box from its decoded mesh's tight AABB, and the
/// renderer discards the stand-in's fragments there: no coplanar double-draw, no z-fight, no PDO. The
/// relationship is purely structural — by 3D Tiles' spatial coherence a descendant's content lies inside its
/// ancestor's footprint — and only the NEAREST rendered ancestor in each branch needs it (a deeper descendant
/// is covered transitively by the nearer one). ADD tiles are skipped: they render alongside their children by
/// design and must not be clipped. **`render` must already be path-sorted** (the caller sorts it for
/// deterministic draw order); given that, this is an O(R) linear sweep over the render set `R` — no sort here.
fn attach_clip_children(render: &mut [SelectedTile]) {
    if render.len() < 2 {
        return; // need an ancestor AND a descendant for any clip to exist
    }
    // `render` is path-sorted (lexicographic == DFS pre-order: a path sorts immediately before its extensions,
    // and a whole subtree is one contiguous run), so we walk it directly. `stack` holds the chain of rendered
    // ancestors of the cursor (each entry's path a prefix of the next). For each tile, pop entries that aren't
    // its prefix; the top is then its nearest rendered ancestor. Defer the mutations (they borrow `render`
    // mutably) until after the immutable sweep.
    let mut stack: Vec<usize> = Vec::new();
    let mut attach: Vec<(usize, usize)> = Vec::new(); // (ancestor, descendant) indices into `render`
    for i in 0..render.len() {
        while let Some(&top) = stack.last() {
            if is_path_prefix(&render[top].path, &render[i].path) {
                break;
            }
            stack.pop();
        }
        if let Some(&anc) = stack.last() {
            // ADD ancestors render in addition to their children — never hole-clipped.
            if !render[anc].is_add {
                attach.push((anc, i));
            }
        }
        stack.push(i);
    }
    // Cap order matters under skip-level overflow (> MAX candidates for one ancestor): take the LARGEST
    // footprints first, so the surplus that draws coplanar-unclipped is the least-visible slivers — path
    // order picked an arbitrary subset and left big blur-over-sharp patches (PIE 2026-06-09). Group by
    // ancestor, largest `bounds_radius` first within each group; the sort is O(A log A) over the (small)
    // attach list and deterministic (`total_cmp`, unique indices).
    attach.sort_unstable_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| render[b.1].bounds_radius.total_cmp(&render[a.1].bounds_radius))
            .then_with(|| a.1.cmp(&b.1))
    });
    for (anc, desc) in attach {
        if render[anc].clip_children.len() < MAX_CLIP_CANDIDATES_PER_TILE {
            // Group tag: which DIRECT-CHILD subtree of the ancestor this descendant lies in — the first path
            // step below the ancestor (a child index, ≤3 quadtree / ≤7 octree; u8 is ample). The scene unions
            // each group's boxes so deep skip-level frontiers reduce to ≤ one box per direct child.
            let group = render[desc].path[render[anc].path.len()].min(u8::MAX as u32) as u8;
            let key = render[desc].key;
            render[anc].clip_children.push((group, key));
        }
    }
}

/// Is `a` a (non-strict) prefix of `b`? In [`attach_clip_children`]'s sweep `a` is always a stacked ancestor and
/// `b` the cursor, and all render paths are unique, so a `true` here always means a STRICT ancestor (shorter
/// path) — equal-length distinct paths can't be prefix-related.
#[inline]
fn is_path_prefix(a: &[u32], b: &[u32]) -> bool {
    a.len() <= b.len() && b[..a.len()] == *a
}

/// cesium-native's `loadingDescendantLimit`: when a fresh (nothing-shown-last-frame) refinement has more than
/// this many still-loading descendants, render the coarse ancestor instead of waiting (the kick) — bounds how
/// many holes a not-yet-ready subtree may show. Cesium's default; tune if pop-vs-coarse balance needs it.
const LOADING_DESCENDANT_LIMIT: u32 = 20;

/// Hostile-input depth cap (C1). An external-tileset graft *chain* — each fetched `.json` refining into another
/// external `.json` — can deepen the tree without bound, overflowing this recursion's stack and growing memory
/// forever. (`serde_json` already caps a *single* response's nesting at depth 128; only the chain is unbounded.)
/// Beyond this depth the traversal drops the subtree, exactly like a `Culled` tile. It sits far above any real
/// tileset (planet-scale trees are ~tens of levels), so legitimate data never reaches it. Bounding `visit` here
/// also bounds grafting transitively: a tile we never descend into is never requested, so its `.json` is never
/// fetched and grafted (the scene's `graft_external` re-checks it as defense-in-depth).
pub const MAX_TILE_DEPTH: u32 = 512;

struct Ctx<'a> {
    ellipsoid: &'a Ellipsoid,
    params: &'a SelectParams,
    /// `tan(params.fov_y_radians / 2)`, computed ONCE per pass (the FOV is constant across the whole
    /// traversal) so the per-tile `should_refine` does no transcendental on the hot path.
    tan_half_fov: f64,
    /// The caller-owned path interner: every visited node interns its id here (amortized O(1)), and the
    /// per-tile state/dedup key on those ids instead of hashing path Vecs (audit 2026-06-10 §3.2).
    arena: &'a mut PathArena,
    is_loaded: &'a mut dyn FnMut(&[u32], u32) -> bool,
    /// Is that tile's content **terminally dead** (fetch/decode gave up — the scene will never serve it)? A dead
    /// tile is treated as ABSENT (like a culled / non-existent tile): not rendered, not requested, and it does
    /// NOT block its parent's refinement — so one dead fine tile can't pin its whole region to the coarse parent
    /// (which, drawn as the stand-in, would blur every loaded sibling). `|_, _| false` ⇒ no deaths (explicit lanes).
    is_dead: &'a mut dyn FnMut(&[u32], u32) -> bool,
    visibility: &'a mut dyn FnMut(DVec3, f64) -> Visibility,
    /// Subtree-availability seam for implicit tiling: consulted only when [`visit`] reaches an `implicitTiling`
    /// root, to synthesize its quadtree/octree descendants. [`NoImplicit`] for explicit-only callers.
    provider: &'a mut dyn AvailabilityProvider,
    /// Last pass's per-tile results — read for the hysteresis dead-band + the cross-frame kick.
    prev_state: &'a SelectionState,
    /// This pass's render **append log** (deduped after the walk); the kick `truncate`s a subtree off it.
    render: &'a mut Vec<Rc<SelectedTile>>,
    load: &'a mut Vec<SelectedTile>,
    /// This pass's per-tile results as an **append log**, parallel to `render` so the kick truncates both in
    /// lockstep (a kicked descendant leaves no `Rendered` entry). Collapsed to [`SelectionState`] after the walk.
    state: &'a mut Vec<(u32, SelectionResult)>,
    /// The nearest loaded **ancestor** of the node currently being visited — the coarse stand-in to draw while
    /// finer tiles stream. Threaded on `Ctx` (saved/restored by [`decide`] around each refine) rather than as a
    /// recursion argument, so the explicit and implicit lanes share it uniformly. `None` until some ancestor is
    /// resident.
    nearest_loaded: Option<Rc<SelectedTile>>,
    /// This pass's hold-reason X-ray (see [`HoldCounts`]). NOT truncated by the kick: a kicked subtree's
    /// interior holds still happened this pass and are exactly what the kick is waiting on.
    holds: HoldCounts,
}

/// The frame-varying facts a node contributes to the per-node decision core ([`decide`]) — computed the same
/// way for an explicit `Tile` and a synthesized implicit tile, so both share ONE copy of the SSE / render /
/// load / kick / state logic. (The node's *identity* — its `SelectedTile` and its children — is supplied to
/// [`decide`] as closures, the only thing that differs between the explicit and implicit lanes.)
struct NodeFacts {
    distance: f64,
    geometric_error: f64,
    /// Is this node's own content resident? (`is_loaded(path)`.)
    loaded: bool,
    /// Is this node's content terminally dead (`is_dead(path)`)? A dead tile is treated as absent — see [`Ctx`].
    dead: bool,
    /// This node's effective 3D Tiles `refine` mode, INHERITED down the tree (`refine_or(parent)`). REPLACE =
    /// children render in place of this tile; ADD = this tile's own content renders IN ADDITION to its refined
    /// children (3D Tiles 1.1 §"Refinement"). Consulted by [`decide`]; mixed ADD/REPLACE trees are honored
    /// per-node. Implicit descendants inherit the implicit root's mode.
    refine: Refine,
    /// Did this node draw on screen last frame (own content `Rendered`, or its coarse fallback `Kicked`)?
    rendered_last: bool,
    /// Was this node `Refined` last frame (drives the hysteresis dead-band)?
    refined_last: bool,
    /// The SSE pixel target for this node this frame (full detail in view; coarser when `Offscreen`).
    sse_target: f64,
    /// Is this node inside the view frustum + detail band ([`Visibility::Visible`])? `false` for an `Offscreen`
    /// coarse-shell tile. Gates `mustContinueRefiningToDeeperTiles`: the anti-detail-VANISH rule only protects
    /// CURRENTLY-VISIBLE detail; an off-view tile has no shown detail to hold, so it coarsens to the shell
    /// normally (otherwise turning the camera away would freeze the previously-in-view detail off-screen).
    in_view: bool,
    /// Does this node have any children to refine into? (Explicit: `!children.is_empty()`. Implicit: at least
    /// one available child coordinate, or more implicit levels remain — see [`visit_implicit`].)
    has_children: bool,
}

/// Build a `SelectedTile`. This allocates the path Vec (and, for the LOAD flavor, a `Vec<String>` of
/// content URIs), so it is called ONLY when the tile is actually kept — a resident fallback or an SSE-leaf
/// to request — never for the transient interior nodes that just pass an inherited fallback down.
/// `with_uris` picks the flavor (audit 2026-06-10 §4.7): `true` for `load` tiles (the fetch path consumes the
/// URIs), `false` for `render`/coverage tiles (URIs are never read downstream — renderers key by `node`/`key`
/// — and eagerly cloning a String per URI for every loaded interior node was a per-pass allocation tax).
#[allow(clippy::too_many_arguments)]
fn make_selected(
    path: &[u32],
    node: u32,
    key: u64,
    world: DMat4,
    tile: &Tile,
    distance: f64,
    center: DVec3,
    radius: f64,
    is_add: bool,
    scaled_geometric_error: f64,
    with_uris: bool,
) -> SelectedTile {
    SelectedTile {
        path: path.to_vec(),
        node,
        key,
        world_transform: world,
        content_uris: if with_uris {
            tile.content_uris().map(String::from).collect()
        } else {
            Vec::new()
        },
        // 3D Tiles 1.1: geometricError scaled by the transform's largest scaling factor (a tile scaled k× has
        // k× the error; no-op for P3DT, scale=1). Computed ONCE per visit — `visit` already derived this exact
        // value for `NodeFacts` (the formula's single home), so it is threaded here rather than re-paying
        // `max_axis_scale`'s three sqrts and risking the two copies drifting.
        geometric_error: scaled_geometric_error,
        distance,
        bounds_center: center,
        bounds_radius: radius,
        is_add,
        clip_children: Vec::new(),
    }
}

/// Visit a tile (cesium-native's `visitTile`), returning the [`TraversalDetails`] its subtree folds up. Side
/// effects land in `ctx`: tiles to draw appended to `render`, tiles to fetch pushed to `load`, each tile's
/// [`SelectionResult`] recorded in `state`. The nearest loaded **ancestor** (the coarse stand-in to draw while
/// finer tiles stream) is threaded on `ctx.nearest_loaded`, saved/restored by [`decide`] around each refine.
fn visit(
    tile: &Tile,
    path: &mut Vec<u32>,
    node: u32,
    world: DMat4,
    parent_refine: Refine,
    ctx: &mut Ctx,
) -> TraversalDetails {
    // Depth cap (C1) — `path.len()` is this tile's depth. Beyond the cap, drop the subtree (no render/fetch/kick
    // pressure): bounds the recursion's stack against a pathological external-tileset graft chain, and — since
    // an undescended tile is never requested — transitively bounds the grafting that would deepen the tree.
    if path.len() > MAX_TILE_DEPTH as usize {
        return TraversalDetails::NEUTRAL;
    }
    let (center, radius) = tile_bounding_sphere_ecef(tile, &world, ctx.ellipsoid);
    // Hostile/malformed input (H7): a non-finite bounding sphere — from an overflowing coordinate (JSON `1e400`
    // parses to ∞), or a `transform` whose cumulative product overflows down a deep chain, neither catchable by
    // per-field parse validation. The distance math below leans on `f64::max`, and `(∞|NaN).max(0.0) == 0.0`, so
    // such a tile would read as "infinitely close" and force its WHOLE subtree to refine + stream at full detail
    // (a resource-exhaustion vector). We can't place it — drop the subtree, exactly like a `Culled` tile. Checked
    // here (the single chokepoint that consumes bounds) so it covers parsed, grafted, and implicit tiles alike.
    if !center.is_finite() || !radius.is_finite() {
        return TraversalDetails::NEUTRAL;
    }
    // viewerRequestVolume (3D Tiles 1.1): skip this tile + its whole subtree while the viewer is OUTSIDE the
    // request volume — no request, no refine, no render (the content only appears once the viewer enters it).
    if let Some(rv) = &tile.viewer_request_volume {
        if !point_inside_volume(rv, &world, ctx.ellipsoid, ctx.params.camera_position) {
            return TraversalDetails::NEUTRAL;
        }
    }
    // Frustum-aware LOD: out-of-range drops the subtree; off-view tiles are KEPT (shadow casters + camera-turn
    // readiness) but refine only to the coarser `culled_screen_space_error`; in-view tiles get full detail.
    let vis = (ctx.visibility)(center, radius);
    let sse_target = match vis {
        Visibility::Culled => return TraversalDetails::NEUTRAL, // out of range — no fetch/render, no kick pressure
        Visibility::Offscreen => ctx.params.culled_screen_space_error,
        Visibility::Distant => ctx.params.distant_screen_space_error,
        Visibility::Visible => ctx.params.max_screen_space_error,
    };
    let in_view = vis == Visibility::Visible;
    let distance = ((ctx.params.camera_position - center).length() - radius).max(0.0);
    let loaded = (ctx.is_loaded)(path, node);
    let dead = (ctx.is_dead)(path, node);
    let prev = ctx.prev_state.get(&node).copied();
    // 3D Tiles `refine` mode, inherited from the parent when this tile omits it (spec: required on the root,
    // inherited otherwise). Drives ADD-vs-REPLACE in `decide` and is passed down to this tile's children.
    let refine = tile.refine_or(parent_refine);

    let facts = NodeFacts {
        distance,
        // geometricError scaled by the transform's largest scaling factor (3D Tiles 1.1; no-op for P3DT, scale=1).
        geometric_error: tile.geometric_error * max_axis_scale(&world),
        loaded,
        dead,
        refine,
        rendered_last: matches!(prev, Some(SelectionResult::Rendered | SelectionResult::Kicked)),
        refined_last: matches!(prev, Some(SelectionResult::Refined)),
        sse_target,
        in_view,
        // An implicit-tiling root's descendants are NOT in `tile.children` (normally empty) — they live at
        // level 1+ of the implicit tree, which exists only if `available_levels > 1`. So an implicit root has
        // children iff it permits at least one level below the root; their per-coordinate existence is then the
        // provider's availability bits (checked during refine; an all-unavailable refine renders self via the
        // empty-refine fallback in `decide`). A plain tile refines iff it has explicit children.
        has_children: match &tile.implicit_tiling {
            Some(it) => it.available_levels > 1,
            None => !tile.children.is_empty(),
        },
    };

    // The node's `SelectedTile` (built lazily, only when actually kept) and its child-refinement step are the
    // ONLY things that differ between an explicit tile and an implicit root. Everything else — SSE, render/
    // load, the kick, the cross-frame state — is the shared [`decide`] core. `build` takes the `path` from
    // `decide` (not by capture) so `decide` can still hand `&mut path` to the `refine` closure; `refine` reads
    // `ctx.nearest_loaded` (which `decide` sets to this node's coverage before calling it). The bool is the
    // [`make_selected`] flavor: `true` = a `load` tile (with content URIs), `false` = render/coverage.
    let key = ctx.arena.key(node);
    let build = |p: &[u32], with_uris: bool| {
        make_selected(
            p,
            node,
            key,
            world,
            tile,
            distance,
            center,
            radius,
            refine == Refine::Add,
            facts.geometric_error,
            with_uris,
        )
    };
    if let Some(implicit) = &tile.implicit_tiling {
        // Implicit root (the implicit tree's level 0): synthesize its quadtree/octree descendants from the
        // availability provider. The implicit content template lives on THIS tile's content; its bounds +
        // geometric error are this tile's, halved/subdivided per level for descendants.
        let root_bounds = tile.bounding_volume;
        let root_error = tile.geometric_error;
        let template = tile.content_uris().next();
        let root_path_len = path.len();
        decide(ctx, path, node, &facts, build, |ctx, path| {
            // The implicit root itself is level 0, coordinate (0,0,0); descend its children.
            descend_implicit_children(
                ctx,
                path,
                node,
                world,
                root_path_len,
                node, // this implicit ROOT's interned id, threaded to every descendant's ImplicitContext
                &root_bounds,
                implicit,
                root_error,
                template,
                refine, // implicit descendants inherit the implicit root's refine mode
                0,
                0,
                0,
                0,
            )
        })
    } else {
        // Explicit subtree: descend `tile.children` — each inherits THIS tile's resolved refine mode.
        decide(ctx, path, node, &facts, build, |ctx, path| {
            let mut sub = TraversalDetails::REFINING;
            for (i, child) in tile.children.iter().enumerate() {
                let cnode = ctx.arena.child_of(node, i as u32);
                path.push(i as u32);
                let cd = visit(child, path, cnode, world * child.matrix(), refine, ctx);
                path.pop();
                sub.fold_child(cd);
            }
            sub
        })
    }
}

/// The per-node **decision core** shared by the explicit and implicit lanes (cesium-native's `visitTile`
/// body). Given a node's [`NodeFacts`], a `build` closure that materializes its [`SelectedTile`] on demand,
/// and a `refine` closure that descends its children (returning the folded subtree [`TraversalDetails`]), it
/// runs the identical SSE-leaf-or-refine decision, the not-loaded coarse-fallback, the cross-frame **kick**,
/// and the `Rendered`/`Refined`/`Kicked` state recording. Factoring it here is what lets implicit children
/// reuse every subtlety of the streaming behavior (kick, hysteresis, anti-hole loads) verbatim.
fn decide<B, R>(
    ctx: &mut Ctx,
    path: &mut Vec<u32>,
    node: u32,
    facts: &NodeFacts,
    build: B,
    refine: R,
) -> TraversalDetails
where
    B: Fn(&[u32], bool) -> SelectedTile,
    R: FnOnce(&mut Ctx, &mut Vec<u32>) -> TraversalDetails,
{
    let (loaded, rendered_last) = (facts.loaded, facts.rendered_last);

    // The coarse stand-in for this region: this node if it's resident (built now, shared via Rc), else the
    // nearest resident ancestor handed down on `ctx.nearest_loaded`. We never build a SelectedTile for an
    // unloaded interior node here. RENDER flavor (no content URIs — see `make_selected`): coverage only ever
    // lands in `render`, whose consumers key by node/key and never read URIs.
    let coverage: Option<Rc<SelectedTile>> = if loaded {
        Some(Rc::new(build(path, false)))
    } else {
        ctx.nearest_loaded.clone()
    };
    // Is the nearest loaded ancestor my DIRECT parent? Marks this node as the FIRST unloaded level below the
    // rendered frontier — the only interior rung the ladder loads this pass (see the refine-tail load arm).
    let parent_is_coverage = !loaded && coverage.as_ref().is_some_and(|c| c.path.len() + 1 == path.len());

    // Refine criterion single-homed in `logi_geo_lod::should_refine` (audit M22), with the hysteresis
    // dead-band: a tile `Refined` last frame holds its split until SSE drops below `target/(1+hysteresis)`.
    let meets = !logi_geo_lod::should_refine(
        facts.geometric_error,
        facts.distance,
        &logi_geo_lod::RefineParams {
            tan_half_fov_y: ctx.tan_half_fov, // precomputed once per pass (no per-tile transcendental)
            viewport_height: ctx.params.viewport_height,
            sse_target: facts.sse_target, // full detail in view; coarser when off-view (Offscreen)
            hysteresis: ctx.params.hysteresis,
        },
        facts.refined_last,
    );

    // mustContinueRefiningToDeeperTiles (cesium-native `Tileset::_mustContinueRefiningToDeeperTiles`): a tile
    // that NOW meets SSE but was `Refined` last frame and isn't loaded yet must KEEP refining — its finer
    // descendants that were already on screen stay shown, instead of popping down to a coarse stand-in while
    // this level streams in. This is the anti-detail-VANISH rule (the zoom-out / fast-pull-back case), distinct
    // from the hysteresis dead-band (which prevents threshold *dithering*): without it, the instant the camera
    // retreats far enough that a coarser tile satisfies SSE, Geo would drop to `nearest_loaded` (a possibly much
    // coarser ancestor) until this tile's own mesh arrives. We also queue this tile's own content (in the refine
    // tail below — cesium loads it Urgent) so it can take over as the proper coarse base once it lands. REPLACE
    // only (ADD always draws itself anyway) and only when it actually HAS children to keep showing.
    let must_continue = meets
        && facts.in_view // only protect CURRENTLY-VISIBLE detail; an off-view tile coarsens to its shell
        && facts.has_children
        && facts.refined_last
        && !loaded
        && !facts.dead
        && facts.refine != Refine::Add;

    // RENDER at this level: this node meets SSE (and isn't holding deeper already-shown detail via
    // `must_continue`), or is a leaf (nothing finer to refine into).
    if (meets || !facts.has_children) && !must_continue {
        if loaded {
            // X-ray: rendered COARSER than the SSE target with nothing to refine into (entered via
            // `!has_children` while `should_refine` said refine) — the structural can't-refine hold.
            if !meets {
                ctx.holds.leaf_under_sse += 1;
            }
            ctx.render.push(coverage.expect("loaded ⇒ coverage is self"));
            ctx.state.push((node, SelectionResult::Rendered));
            return TraversalDetails {
                all_renderable: true,
                any_rendered_last_frame: rendered_last,
                not_yet_renderable: 0,
            };
        }
        // Terminally-dead content (see `Ctx::is_dead`): never requested again, BUT its footprint must not be
        // a hole (audit 2026-06-10 holes follow-up). Draw the nearest loaded ancestor over it — exactly like
        // an unloaded leaf, minus the load. The pre-hole-clip code left dead footprints ABSENT because the
        // stand-in was the WHOLE parent tile blurring every loaded sibling; the geometric hole-clip now cuts
        // the stand-in to precisely the uncovered area (the siblings clip it), so coverage is strictly better
        // than a see-through-to-space hole — and a fetch/decode give-up burst no longer punches permanent
        // wedges in the ground. `not_yet_renderable: 0` (it will never load — it must not hold a kick or feed
        // the pending count); `all_renderable: false` (the ancestor stand-in must keep covering forever).
        if facts.dead {
            ctx.holds.dead += 1; // X-ray: dead footprint covered by the ancestor forever
            if let Some(anc) = &coverage {
                ctx.render.push(Rc::clone(anc));
            }
            // `all_renderable: true`: the dead footprint must never BLOCK its parent's atomic reveal (the
            // loaded siblings would otherwise wait forever on a tile that can never load) — the coverage push
            // above keeps the ancestor drawn over the dead area regardless, clipped by the revealed siblings.
            return TraversalDetails {
                all_renderable: true,
                any_rendered_last_frame: rendered_last,
                not_yet_renderable: 0,
            };
        }
        // Not loaded → request it; meanwhile draw the coarse ancestor (if any) so the region isn't a hole.
        // Its OWN content wasn't drawn, so leave it absent from `state` (≠ Rendered for next frame's kick).
        ctx.holds.loading += 1; // X-ray: queued; covered by the ancestor meanwhile
        ctx.load.push(build(path, true));
        if let Some(anc) = &coverage {
            ctx.render.push(Rc::clone(anc));
        }
        return TraversalDetails {
            all_renderable: false,
            any_rendered_last_frame: rendered_last,
            not_yet_renderable: 1,
        };
    }

    // REFINE. The per-tile `refine` mode decides whether this tile's OWN content is REPLACED by its children
    // (REPLACE) or rendered IN ADDITION to them (ADD; 3D Tiles 1.1 §"Refinement").
    let is_add = facts.refine == Refine::Add;

    // ADD: emit this tile's own content NOW — *before* the children's marks — so it sits outside the suffix a
    // deeper REPLACE-child kick truncates. Under ADD there are no holes (the parent always draws), so we never
    // draw a coarse ANCESTOR stand-in for it: when loaded we render it, when not we just queue its own additive
    // content. `(self_renderable, self_not_yet)` fold into the return so an unloaded ADD parent still counts as
    // one still-loading tile to ITS ancestor. For REPLACE these are `(true, 0)` and this block is inert.
    let (add_self_renderable, add_self_not_yet) = if is_add {
        if loaded {
            ctx.render.push(coverage.clone().expect("loaded ⇒ coverage is self"));
            ctx.state.push((node, SelectionResult::Rendered));
            (true, 0)
        } else if !facts.dead {
            ctx.holds.loading += 1; // X-ray: additive parent content queued
            ctx.load.push(build(path, true)); // stream the parent's own additive content (no ancestor stand-in)
            (false, 1)
        } else {
            ctx.holds.dead += 1; // X-ray: dead ADD parent (nothing to add; doesn't block)
            (true, 0)
        }
    } else {
        (true, 0)
    };

    // Remember the render+state+load log lengths (AFTER any ADD self-emit) so a kick can `truncate` the
    // children's tentative entries back off (a contiguous suffix), and so we can detect an EMPTY refine below.
    let render_mark = ctx.render.len();
    let state_mark = ctx.state.len();
    let load_mark = ctx.load.len();
    // Hand `coverage` down as the children's `nearest_loaded` (save/restore the previous value so this node's
    // siblings see the right stand-in afterward — the field replaces the old explicit `nearest_loaded` argument).
    let prev_nearest = std::mem::replace(&mut ctx.nearest_loaded, coverage.clone());
    let sub = refine(ctx, path);
    ctx.nearest_loaded = prev_nearest;

    // EMPTY REFINE → render self as a leaf. This node "wanted" to refine (it met the has-children test) but the
    // refine produced NOTHING — no descendant rendered, none queued to load, none pending. For implicit tiling
    // this is the norm at a branch tip: a node whose `available_levels` permits children but the subtree's
    // availability bits report none (or the explicit case: every child culled / non-finite). Treating it as a
    // refine would silently leave a HOLE (the exact "refines to nothing" planet-stuck bug). Instead it is
    // effectively a leaf: render its own content (if loaded) or queue it + draw the coarse ancestor — the same
    // code as the leaf branch above. (Pending children make `not_yet_renderable > 0`, so they fall through to
    // the kick below, not here.)
    let empty_refine = ctx.render.len() == render_mark
        && ctx.load.len() == load_mark
        && sub.not_yet_renderable == 0
        && sub.all_renderable;
    if empty_refine {
        // ADD: this tile's own content was already emitted above, and the refine produced nothing finer — so
        // fold the self-state and return (don't run the REPLACE leaf-fallback, which would re-emit / stand-in).
        if is_add {
            return TraversalDetails {
                all_renderable: add_self_renderable,
                any_rendered_last_frame: rendered_last,
                not_yet_renderable: add_self_not_yet,
            };
        }
        // REPLACE: no tentative child state to truncate (nothing was appended); just emit self as a leaf.
        if loaded {
            // X-ray: this node WANTED to refine (it took the refine path) but the refine produced nothing —
            // an implicit branch tip, every child culled, or a graft whose children are MISSING (scar suspect).
            ctx.holds.leaf_under_sse += 1;
            ctx.render.push(coverage.expect("loaded ⇒ coverage is self"));
            ctx.state.push((node, SelectionResult::Rendered));
            return TraversalDetails {
                all_renderable: true,
                any_rendered_last_frame: rendered_last,
                not_yet_renderable: 0,
            };
        }
        // Dead content with an empty refine → COVERED by the nearest loaded ancestor, never a hole (the same
        // holes follow-up as the leaf branch above; never re-queued).
        if facts.dead {
            ctx.holds.dead += 1; // X-ray
            if let Some(anc) = &coverage {
                ctx.render.push(Rc::clone(anc));
            }
            return TraversalDetails {
                all_renderable: true, // never blocks the reveal — see the leaf dead-branch note
                any_rendered_last_frame: rendered_last,
                not_yet_renderable: 0,
            };
        }
        ctx.holds.loading += 1; // X-ray: empty-refine leaf queued
        ctx.load.push(build(path, true));
        if let Some(anc) = &coverage {
            ctx.render.push(Rc::clone(anc));
        }
        return TraversalDetails {
            all_renderable: false,
            any_rendered_last_frame: rendered_last,
            not_yet_renderable: 1,
        };
    }

    // KICK: a fresh refinement (showed nothing last frame) that isn't fully loaded would draw holes — fall
    // back to the coarse tile when there'd be too many holes, or this tile is itself a ready stand-in.
    // `sub.any_rendered_last_frame` is DESCENDANTS-only, so a kicked coarse tile keeps kicking each frame
    // until ALL its descendants load, then switches atomically (no partial-LOD pop-in).
    // KICK — REPLACE only. ADD has no holes (the parent always renders its own content), so it never falls back
    // to a coarse stand-in: gate the entire kick off for ADD.
    let kick = !is_add && !sub.all_renderable && !sub.any_rendered_last_frame;
    // ATOMIC REVEAL for a loaded parent (holes follow-up, 2026-06-10 — restores the `|| loaded` arm the
    // 2026-06-09 partial-reveal change removed). The partial-reveal experiment let a loaded parent's children
    // render the instant each arrived, with the parent's coarse mesh hole-clipped to fill only the unloaded
    // footprints. Its first full-scale eyeball showed why cesium never coexists parent+child for REPLACE: the
    // clip primitive is a BOX, but a P3DT tile's plan-view footprint is IRREGULAR — the box-cover's corners
    // outside the child's actual mesh clip the parent over ground the child never draws (triangular
    // see-through holes), while any under-cover leaves coplanar z-fighting. No box can exact-clip an
    // irregular footprint, so for the LOADING path the parent now holds its own render until the subtree is
    // fully renderable, then switches atomically — no coexistence, no clip needed, no holes, no z-fight, by
    // construction. The reveal latency this re-buys is far smaller than when partial reveal was introduced
    // (fetch-priority concentration + the flash/geom caches + the texture gate converge a sibling set fast).
    // Coverage + clip still serve the states with no atomic alternative: dead footprints (covered forever,
    // clipped by live siblings), unloaded parents (the nearest loaded ancestor covers a partially-loaded
    // frontier), and `must_continue` transitions. The exact-footprint upgrade, if reveal latency ever bites
    // again, is a stencil/custom-depth footprint mask — not a tighter box.
    let will_kick = kick && (sub.not_yet_renderable > LOADING_DESCENDANT_LIMIT || loaded);
    if will_kick {
        ctx.holds.kick += 1; // X-ray: coarse stand-in held until the subtree is fully renderable
                             // Drop the descendants' tentative renders AND state entries ("kick" them) — leaving no stale
                             // `Rendered` behind — then draw the coarse stand-in and record this tile as `Kicked`.
        ctx.render.truncate(render_mark);
        ctx.state.truncate(state_mark);
        if let Some(cov) = &coverage {
            ctx.render.push(Rc::clone(cov)); // draw the coarse stand-in instead
        }
        if !loaded && !facts.dead {
            ctx.load.push(build(path, true)); // and keep loading it (a dead tile is never requested)
        }
        ctx.state.push((node, SelectionResult::Kicked));
        return TraversalDetails {
            all_renderable: loaded,
            any_rendered_last_frame: sub.any_rendered_last_frame || rendered_last,
            not_yet_renderable: if loaded { 0 } else { 1 },
        };
    }
    // Refine accepted — the children render (already appended). Record `Refined` for the hysteresis dead-band.
    //
    // **Coarse-ancestor load** (REPLACE only — an ADD parent already queued its own content above): enqueue THIS
    // refining tile's own (coarser) content so a coarse stand-in becomes available for its still-loading
    // descendants instead of holes. Fires (once) when the subtree isn't fully loaded AND any of:
    //   (a) `must_continue` — this tile just met SSE but is holding its already-shown deeper detail until its own
    //       mesh lands (cesium loads it Urgent); load even if the subtree is fully renderable so it can coarsen.
    //   (b) `coverage.is_none()` — there is NO loaded ancestor to stand in for the holes. This is the
    //       cesium-native **`preloadAncestors`** behavior (issue #7): on a COLD approach to a never-visited
    //       region, warm the coarse ancestor chain top-down so a coarse base appears before the fine frontier,
    //       not a hole. It is **self-limiting** — it fires only along the *uncovered* frontier (the moment any
    //       ancestor loads, descendants have `coverage.is_some()` and stop triggering it), and the coarser
    //       tile's higher geometric error sorts it ahead of the fine leaves in the (priority + per-step-capped)
    //       load queue, so the chain lands coarse-first. (This replaces an earlier `any_rendered_last_frame`
    //       gate that suppressed the cold-approach case — the gate is what left a never-visited region without a
    //       coarse base. A literal cesium `preloadSiblings` is NOT added: Geo's `Offscreen` classification
    //       already keeps the whole in-range hemisphere coarse-resident, a superset of culled-sibling preload.)
    // THE REFINEMENT LADDER, ONE RUNG AT A TIME (atomic-reveal follow-up, 2026-06-10): under the atomic
    // reveal a kicked ancestor releases when its DIRECT children are loaded+textured (a loaded child that
    // must keep refining kicks LOCALLY and reports renderable), so the cut sharpens level-by-level — but
    // only if the intermediate levels actually LOAD. Two over-corrections book-end the right arm:
    //  * the old `coverage.is_none()`-only gate (preloadAncestors, tuned for the partial-reveal era where
    //    intermediates never rendered) starved the ladder entirely — a kicked parent waited on the whole
    //    multi-level SSE frontier ("zoom in and it never sharpens");
    //  * queueing EVERY unloaded interior flooded the pipeline — thousands of levels-deep prepares parked
    //    Resident-untextured behind the C++ texture round-trip, ballooning the published set to the resident
    //    cap (render=17k, flash-restore thrash) while the fetch lane idled.
    // The bounded form: queue an unloaded refining interior ONLY when it is the FIRST unloaded level below
    // the rendered frontier (`parent_is_coverage` — my direct parent is the loaded stand-in), or when NOTHING
    // above is loaded (`coverage.is_none()` — the cold-approach top-down warm-up). Each branch then carries
    // at most one in-flight rung; the next queues when this one lands+textures and the anchor descends.
    // `must_continue` keeps its urgent self-load (the zoom-out take-over case).
    if !is_add
        && !loaded
        && !facts.dead
        && (must_continue || ((coverage.is_none() || parent_is_coverage) && !sub.all_renderable))
    {
        ctx.load.push(build(path, true));
    }
    ctx.state.push((node, SelectionResult::Refined));
    // Fold the ADD self-contribution into the returned details (no-op for REPLACE: `(true, 0)`).
    TraversalDetails {
        all_renderable: sub.all_renderable && add_self_renderable,
        any_rendered_last_frame: sub.any_rendered_last_frame || rendered_last,
        not_yet_renderable: sub.not_yet_renderable.saturating_add(add_self_not_yet),
    }
}

/// Visit a synthesized implicit tile at global `(level, x, y, z)` (its child-index `path` already pushed by the
/// caller). Mirrors [`visit`] for a node whose bounds / geometric-error / content come from subdividing the
/// implicit root instead of a stored `Tile`: it computes the same [`NodeFacts`] and runs the same [`decide`]
/// core, then refines into its OWN implicit children. The caller only recurses into children the provider
/// reported [`TileAvailability::Available`], so this tile definitely exists; whether it has *content* (a URI to
/// fetch/render) is a separate availability bit. `root_error` is the implicit root's geometric error and
/// `template` its content-URI template (both frame-invariant, threaded down from the root).
#[allow(clippy::too_many_arguments)]
fn visit_implicit(
    ctx: &mut Ctx,
    path: &mut Vec<u32>,
    node: u32,
    world: DMat4,
    root_path_len: usize,
    root_node: u32,
    root_bounds: &BoundingVolume,
    implicit: &ImplicitTiling,
    root_error: f64,
    template: Option<&str>,
    refine: Refine,
    level: u32,
    x: u32,
    y: u32,
    z: u32,
) -> TraversalDetails {
    // Depth cap (C1): the implicit tree shares the global depth bound (the `path` carries the implicit steps,
    // so `path.len()` already counts them) — a hostile `available_levels` can't deepen the recursion past it.
    if path.len() > MAX_TILE_DEPTH as usize {
        return TraversalDetails::NEUTRAL;
    }
    // Bounds: subdivide the implicit root's volume to this coordinate (frame-invariant geometry), ride `world`.
    let child_bv = subdivide_bounds(root_bounds, implicit.subdivision_scheme, level, x, y, z);
    let (center, radius) = bounding_sphere_ecef(&child_bv, &world, ctx.ellipsoid);
    if !center.is_finite() || !radius.is_finite() {
        return TraversalDetails::NEUTRAL; // hostile/degenerate bounds — drop, exactly like `visit` (H7)
    }
    let vis = (ctx.visibility)(center, radius);
    let sse_target = match vis {
        Visibility::Culled => return TraversalDetails::NEUTRAL,
        Visibility::Offscreen => ctx.params.culled_screen_space_error,
        Visibility::Distant => ctx.params.distant_screen_space_error,
        Visibility::Visible => ctx.params.max_screen_space_error,
    };
    let in_view = vis == Visibility::Visible;
    let distance = ((ctx.params.camera_position - center).length() - radius).max(0.0);
    let loaded = (ctx.is_loaded)(path, node);
    let dead = (ctx.is_dead)(path, node);
    let prev = ctx.prev_state.get(&node).copied();
    // Geometric error halves each implicit level (3D Tiles implicit convention): root_error / 2^level. Saturate
    // a pathological deep level to a 0 error (⇒ "meets SSE" ⇒ a leaf), never an overflowing/negative value.
    let denom = 1u64.checked_shl(level).unwrap_or(u64::MAX) as f64;
    // geometricError halves per implicit level, then is scaled by the transform's largest scale (3D Tiles 1.1;
    // no-op for P3DT where scale=1).
    let geometric_error = (root_error / denom) * max_axis_scale(&world);

    // Does this implicit tile have any children to refine into? Only if more levels are permitted.
    // (`available_levels` counts levels that CAN exist; level 0 = the root, so the deepest valid level is
    // `available_levels - 1` and a node at it is a leaf. The provider's per-child availability is then the
    // finer gate during refine.)
    let has_children = level + 1 < implicit.available_levels;

    let facts = NodeFacts {
        distance,
        geometric_error,
        loaded,
        dead,
        refine, // implicit descendants inherit the implicit root's refine mode (threaded in)
        rendered_last: matches!(prev, Some(SelectionResult::Rendered | SelectionResult::Kicked)),
        refined_last: matches!(prev, Some(SelectionResult::Refined)),
        sse_target,
        in_view,
        has_children,
    };
    // Content URI: the implicit root's templated content URI filled at this coordinate — but ONLY if the
    // provider reports content available here (a tile may exist purely to subdivide). Built lazily in `build`
    // (and only for the LOAD flavor — the render flavor's URIs are never consumed; audit 2026-06-10 §4.7).
    let content_available = {
        let ctx_implicit = ImplicitContext {
            root_path: &path[..root_path_len],
            root_node,
            tiling: implicit,
        };
        ctx.provider
            .content_availability(&ctx_implicit, ctx.arena, level, x, y, z)
    };
    let key = ctx.arena.key(node);
    let build = |p: &[u32], with_uris: bool| {
        let content_uris = match (with_uris && content_available, template) {
            (true, Some(t)) => vec![logi_3dtiles_tileset::substitute_template(t, level, x, y, z)],
            _ => Vec::new(), // render flavor, no content available, or no template ⇒ no URI to fetch
        };
        SelectedTile {
            path: p.to_vec(),
            node,
            key,
            world_transform: world,
            content_uris,
            geometric_error,
            distance,
            bounds_center: center,
            bounds_radius: radius,
            is_add: refine == Refine::Add,
            clip_children: Vec::new(),
        }
    };
    decide(ctx, path, node, &facts, build, |ctx, path| {
        descend_implicit_children(
            ctx,
            path,
            node,
            world,
            root_path_len,
            root_node,
            root_bounds,
            implicit,
            root_error,
            template,
            refine,
            level,
            x,
            y,
            z,
        )
    })
}

/// Enumerate + visit the implicit children of the node at `(level, x, y, z)`: for each of the `child_count`
/// child cells, ask the provider whether the child tile is available, and — if so — push its child-index step
/// and recurse via [`visit_implicit`], folding the subtree details. A [`TileAvailability::Pending`] child (its
/// `.subtree` not fetched yet) is treated as a not-yet-loaded refinement boundary: we count it as one
/// still-loading descendant so the **kick** keeps the coarse ancestor on screen until the subtree arrives, but
/// we do not recurse (its descendants' availability is unknown). This is the implicit lane's analogue of
/// `visit`'s `for child in tile.children` loop — the only structural difference between the two lanes.
///
/// The child's GLOBAL coordinate is `(2x+dx, 2y+dy, 2z+dz)`; its child-index `step` within this parent is the
/// low bits of its Morton code, `dx | dy<<1 | dz<<2` — exactly `0..child_count`. (The provider interleaves the
/// global coordinate into the full Morton code internally, via `logi_3dtiles_implicit::Subtree::is_tile_available`
/// — the single home of the interleave; the traversal only needs the within-parent step for the `path`.)
#[allow(clippy::too_many_arguments)]
fn descend_implicit_children(
    ctx: &mut Ctx,
    path: &mut Vec<u32>,
    node: u32,
    world: DMat4,
    root_path_len: usize,
    root_node: u32,
    root_bounds: &BoundingVolume,
    implicit: &ImplicitTiling,
    root_error: f64,
    template: Option<&str>,
    refine: Refine,
    level: u32,
    x: u32,
    y: u32,
    z: u32,
) -> TraversalDetails {
    let scheme = implicit.subdivision_scheme;
    let child_level = level + 1;
    let mut sub = TraversalDetails::REFINING;
    // Quadtree splits x/y (2×2); octree also splits z (2×2×2). dz's range is the only scheme difference.
    let z_extent = if scheme == SubdivisionScheme::Octree { 2 } else { 1 };
    for dz in 0..z_extent {
        for dy in 0..2u32 {
            for dx in 0..2u32 {
                let step = dx | (dy << 1) | (dz << 2); // child index within parent == low Morton bits
                let (cx, cy, cz) = (2 * x + dx, 2 * y + dy, 2 * z + dz);
                let availability = {
                    let ctx_implicit = ImplicitContext {
                        root_path: &path[..root_path_len],
                        root_node,
                        tiling: implicit,
                    };
                    ctx.provider
                        .tile_availability(&ctx_implicit, ctx.arena, child_level, cx, cy, cz)
                };
                match availability {
                    TileAvailability::Unavailable => {} // child doesn't exist — prune this branch
                    TileAvailability::Pending => {
                        // The covering `.subtree` isn't fetched yet (the provider noted the fetch need). We
                        // can't render or even name this child — but its REGION must not be a hole. Mirror the
                        // explicit unloaded-leaf path: draw the coarse stand-in (`ctx.nearest_loaded`, the
                        // nearest loaded ancestor, set by `decide` for this node) over the pending region, and
                        // count it as one still-loading descendant. If the parent later kicks, this tentative
                        // ancestor render is truncated with the rest; if it doesn't (few holes), the coarse
                        // ancestor covers the gap until the subtree arrives and the child becomes Available.
                        ctx.holds.pending_subtree += 1; // X-ray: held coarse on a missing `.subtree`
                        if let Some(anc) = ctx.nearest_loaded.clone() {
                            ctx.render.push(anc);
                        }
                        sub.all_renderable = false;
                        sub.not_yet_renderable = sub.not_yet_renderable.saturating_add(1);
                    }
                    TileAvailability::Available => {
                        let cnode = ctx.arena.child_of(node, step);
                        path.push(step);
                        let cd = visit_implicit(
                            ctx,
                            path,
                            cnode,
                            world,
                            root_path_len,
                            root_node,
                            root_bounds,
                            implicit,
                            root_error,
                            template,
                            refine,
                            child_level,
                            cx,
                            cy,
                            cz,
                        );
                        path.pop();
                        sub.fold_child(cd);
                    }
                }
            }
        }
    }
    sub
}

#[cfg(test)]
mod tests {
    use super::*;
    use logi_3dtiles_tileset::parse_tileset;
    use logi_3dtiles_tileset::Tileset;
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
            distant_screen_space_error: 32.0, // 2× — the visible far vista, mildly demoted
            culled_screen_space_error: 64.0,  // 4× — off-view tiles refine to a coarse shell
            hysteresis: 0.0,
        }
    }

    fn paths(tiles: &[SelectedTile]) -> BTreeSet<Vec<u32>> {
        tiles.iter().map(|t| t.path.clone()).collect()
    }

    /// An empty prior state ⇒ a cold / stateless pass (no hysteresis, no cross-frame anti-pop state —
    /// the cold kick still fires).
    fn cold() -> SelectionState {
        SelectionState::new()
    }

    #[test]
    fn hold_counts_xray_attributes_leaf_kick_and_loading() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        // Camera at 100 m: the gE=10 grandchildren are leaves and still FAR over the 16 px target there —
        // every loaded grandchild renders coarser-than-wanted ⇒ leaf_under_sse (4); nothing else holds.
        let all_loaded = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(100.0),
            &mut PathArena::new(),
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        assert_eq!(
            all_loaded.holds.leaf_under_sse, 4,
            "4 under-SSE leaves (the scar counter)"
        );
        assert_eq!(all_loaded.holds.kick, 0);
        assert_eq!(all_loaded.holds.loading, 0);
        // Same camera, only the ROOT loaded: the unloaded descendants count `loading`, and the loaded root
        // KICKS (a ready stand-in over a fresh, not-fully-renderable refinement holds its own render).
        let root_only = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(100.0),
            &mut PathArena::new(),
            &mut |path, _| path.is_empty(),
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        assert!(
            root_only.holds.loading > 0,
            "unloaded descendants count as loading holds"
        );
        assert!(
            root_only.holds.kick > 0,
            "the loaded root kicks over its loading subtree"
        );
        assert_eq!(
            root_only.holds.leaf_under_sse, 0,
            "no loaded under-SSE leaf in this state"
        );
    }

    #[test]
    fn far_camera_renders_only_the_root() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        let sel = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(200_000.0),
            &mut PathArena::new(),
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        assert_eq!(paths(&sel.render), BTreeSet::from([vec![]])); // just the root
        assert!(sel.load.is_empty());
    }

    #[test]
    fn near_camera_refines_to_leaves() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        // Everything loaded ⇒ never a kick (all_renderable everywhere) ⇒ plain SSE refinement to the leaves.
        let sel = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(2000.0),
            &mut PathArena::new(),
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&sel.render),
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]])
        );
        assert!(sel.load.is_empty());
    }

    /// The same 3-level tree as `nested_tileset` but with `refine:"ADD"` on the root (inherited by all).
    fn refine_tileset(root_refine: &str, child0_refine: &str) -> Tileset {
        let json = format!(
            r#"{{ "asset": {{"version":"1.1"}}, "geometricError": 2000,
              "root": {{ "boundingVolume": {{"sphere":[{C},0,0,1]}}, "geometricError": 1000, "refine":"{root_refine}",
                "content": {{"uri":"r.glb"}}, "children": [
                  {{ "boundingVolume": {{"sphere":[{C},0,0,1]}}, "geometricError": 100, "refine":"{child0_refine}", "content": {{"uri":"0.glb"}},
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

    #[test]
    fn add_refinement_renders_the_parent_in_addition_to_its_children() {
        // Same geometry + camera as `near_camera_refines_to_leaves` (which, under REPLACE, renders ONLY the 4
        // leaves) — but with ADD refinement. Per 3D Tiles 1.1, "when refined it renders itself AND its children
        // simultaneously", so every refining ancestor ALSO draws its own content: the whole tree renders.
        let ts = refine_tileset("ADD", "ADD");
        let e = Ellipsoid::wgs84();
        let sel = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(2000.0),
            &mut PathArena::new(),
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&sel.render),
            BTreeSet::from([vec![], vec![0], vec![1], vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1],]),
            "ADD: every refining ancestor renders in addition to its children"
        );
        assert!(sel.load.is_empty());
    }

    #[test]
    fn mixed_refine_honors_each_tile_own_mode() {
        // Root is ADD; child [0] OVERRIDES to REPLACE; child [1] INHERITS ADD. Each tile's own mode must be
        // honored (spec: "any combination of additive and replacement refinement"). So [0] (REPLACE) hides
        // itself when it refines (only its leaves draw), while the root and [1] (ADD) render themselves too.
        let ts = refine_tileset("ADD", "REPLACE");
        let e = Ellipsoid::wgs84();
        let sel = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(2000.0),
            &mut PathArena::new(),
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&sel.render),
            BTreeSet::from([
                vec![],     // root: ADD → renders itself
                vec![1],    // [1]: inherited ADD → renders itself
                vec![0, 0], // [0]'s leaves only ([0] is REPLACE → hidden when refined)
                vec![0, 1],
                vec![1, 0], // [1]'s leaves (alongside [1])
                vec![1, 1],
            ]),
            "mixed tree: [0] REPLACE hides itself; root + [1] ADD render themselves"
        );
        assert!(
            !paths(&sel.render).contains(&vec![0]),
            "the REPLACE child [0] must NOT render itself"
        );
    }

    #[test]
    fn viewer_request_volume_gates_request_and_refine() {
        // Root with a `viewerRequestVolume` sphere of radius 1000 m centered at the bounds point: the viewer is
        // inside iff within 1000 m of (C,0,0). The camera at `params_at(offset)` is `offset` metres out.
        let json = format!(
            r#"{{ "asset": {{"version":"1.1"}}, "geometricError": 2000,
              "root": {{ "boundingVolume": {{"sphere":[{C},0,0,1]}}, "geometricError": 10, "refine":"REPLACE",
                "viewerRequestVolume": {{"sphere":[{C},0,0,1000]}}, "content": {{"uri":"r.glb"}} }} }}"#
        );
        let ts = parse_tileset(json.as_bytes()).unwrap();
        let e = Ellipsoid::wgs84();
        // 2000 m out — OUTSIDE the 1000 m request volume → the tile + subtree are skipped entirely.
        let outside = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(2000.0),
            &mut PathArena::new(),
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        assert!(
            outside.render.is_empty() && outside.load.is_empty(),
            "outside the viewerRequestVolume ⇒ no request, no refine, no render"
        );
        // 500 m out — INSIDE the request volume → the (loaded) leaf renders.
        let inside = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(500.0),
            &mut PathArena::new(),
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&inside.render),
            BTreeSet::from([vec![]]),
            "inside the viewerRequestVolume ⇒ the tile renders"
        );
    }

    #[test]
    fn a_loaded_parent_with_an_unready_child_reveals_atomically_not_partially() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        // Grandchild [0,0] isn't resident; everything else is. A cold pass (nothing shown last frame).
        // ATOMIC REVEAL (holes follow-up 2026-06-10): the loaded [0] KICKS — it renders its own coarse
        // content alone (no coexistence with [0,1], so no clip box and none of the box-vs-irregular-footprint
        // holes) — while [0,0] keeps loading underneath (the kick truncates renders, never loads). [1]'s
        // subtree is fully loaded → it reveals its grandchildren independently (the kick anchors per tile,
        // not at the root: kicked-loaded [0] reports all_renderable, so the root refines normally).
        let mut is_loaded = |p: &[u32], _: u32| p != [0, 0];
        let arena = &mut PathArena::new();
        let s1 = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(2000.0),
            arena,
            &mut is_loaded,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&s1.render),
            BTreeSet::from([vec![0], vec![1, 0], vec![1, 1]]),
            "[0] holds its own render atomically; [1] reveals; nothing coexists"
        );
        assert_eq!(paths(&s1.load), BTreeSet::from([vec![0, 0]]));
        assert!(
            s1.render.iter().all(|t| t.clip_children.is_empty()),
            "atomic reveal needs no hole-clip boxes on the loading path"
        );

        // The straggler lands → [0] switches to its grandchildren ATOMICALLY next pass.
        let s2 = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(2000.0),
            arena,
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &s1.state,
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&s2.render),
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]]),
            "once the subtree is fully renderable the fine cut replaces the kicked parent atomically"
        );
    }

    #[test]
    fn a_loaded_ancestor_anchors_the_kick_over_an_unloaded_parent_then_converges() {
        // root loaded; the mid-tile [0] is NOT loaded and neither is its child [0,1]; [0,0] and the whole [1]
        // subtree are. The nearest LOADED ancestor able to cover [0]'s unready region is the ROOT, so the root
        // KICKS (atomic — it renders alone, no coexistence, no clip), while the unready frontier keeps
        // loading underneath (kick truncates renders, never the load queue).
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        let mut is_loaded = |p: &[u32], _: u32| p != [0] && p != [0, 1];
        let arena = &mut PathArena::new();
        let s1 = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(2000.0),
            arena,
            &mut is_loaded,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&s1.render),
            BTreeSet::from([vec![]]),
            "the loaded root anchors the kick: one coarse cover, never holes, never coexistence"
        );
        assert_eq!(
            paths(&s1.load),
            BTreeSet::from([vec![0], vec![0, 1]]),
            "the unready frontier keeps streaming under the kick — INCLUDING the unloaded intermediate [0]              (the refinement ladder: the root releases per level, so the next level down must load)"
        );

        // [0,1] lands → the whole cut is renderable ([0] itself is an unloaded INTERIOR tile — a refining
        // interior needs no own content) → the root reveals the fine cut atomically.
        let mut is_loaded2 = |p: &[u32], _: u32| p != [0];
        let s2 = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(2000.0),
            arena,
            &mut is_loaded2,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &s1.state,
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&s2.render),
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]]),
            "convergence releases the anchor and the fine cut replaces it atomically"
        );
    }

    #[test]
    fn a_dead_child_no_longer_pins_its_parent_to_coarse_so_siblings_refine_sharp() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        // Same setup as the kick test above, but [0,0] is TERMINALLY DEAD (fetch/decode gave up), not merely
        // still-loading. WITHOUT R1 the dead [0,0] kept [0] kicked to coarse forever (drawing the whole coarse
        // parent over the loaded sibling [0,1] — the "blurry coarse over sharp" overlap). WITH R1 it's treated
        // as absent: [0,1] refines sharp, [0,0]'s footprint is a hole, and [0,0] is NOT re-queued for load.
        let mut is_loaded = |p: &[u32], _: u32| p != [0, 0];
        let mut is_dead = |p: &[u32], _: u32| p == [0, 0];
        let sel = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(2000.0),
            &mut PathArena::new(),
            &mut is_loaded,
            &mut is_dead,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        // Holes follow-up (2026-06-10): the dead [0,0]'s footprint is COVERED by its loaded parent [0],
        // which renders as a stand-in hole-clipped by the loaded sibling [0,1] — so the sibling still
        // refines sharp (the original R1 bug stays fixed, by the CLIP now, not by absence) and the dead
        // footprint shows coarse imagery instead of a see-through hole.
        assert_eq!(
            paths(&sel.render),
            BTreeSet::from([vec![0], vec![0, 1], vec![1, 0], vec![1, 1]]),
            "dead [0,0] is covered by the clipped stand-in [0]; its loaded sibling [0,1] still refines sharp"
        );
        let standin = sel.render.iter().find(|t| t.path == [0]).unwrap();
        assert_eq!(
            standin.clip_children,
            vec![(1u8, path_key(&[0, 1]))], // group 1 = the [0,1] direct-child subtree
            "the stand-in is clipped by the loaded sibling, so only the dead footprint shows coarse"
        );
        assert!(sel.load.is_empty(), "a dead tile must never be re-queued for load");
    }

    #[test]
    fn a_fully_dead_subtree_falls_back_to_the_coarse_parent_not_a_big_hole() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        // BOTH of [0]'s children are dead. R1 must NOT punch a big hole: with the whole sibling set dead, [0]'s
        // refine is empty, so it renders the coarse parent [0] for its region (no holes), while [1] refines.
        let mut is_loaded = |p: &[u32], _: u32| p != [0, 0] && p != [0, 1];
        let mut is_dead = |p: &[u32], _: u32| p == [0, 0] || p == [0, 1];
        let sel = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(2000.0),
            &mut PathArena::new(),
            &mut is_loaded,
            &mut is_dead,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&sel.render),
            BTreeSet::from([vec![0], vec![1, 0], vec![1, 1]]),
            "a fully-dead subtree renders its coarse parent, not a hole"
        );
        assert!(sel.load.is_empty());
    }

    #[test]
    fn fresh_unloaded_subtree_kicks_to_root_and_loads_all_leaves() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        // Only the root is resident; the whole tree is a fresh, unloaded refinement → the root kicks (it is a
        // ready coarse stand-in) and renders alone, while all four leaves are queued.
        let mut is_loaded = |p: &[u32], _: u32| p.is_empty();
        let sel = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(2000.0),
            &mut PathArena::new(),
            &mut is_loaded,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        assert_eq!(paths(&sel.render), BTreeSet::from([vec![]])); // root only — no holes
                                                                  // The whole ladder is queued: the mid LEVELS as well as the leaves, so the kicked root can release
                                                                  // one level at a time (mids land first — higher geometric error sorts them ahead in the fetch queue).
        assert_eq!(
            paths(&sel.load),
            BTreeSet::from([vec![0], vec![0, 0], vec![0, 1], vec![1], vec![1, 0], vec![1, 1]])
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
        let sel = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(2000.0),
            &mut PathArena::new(),
            &mut |_, _| true,
            &mut |_, _| false,
            &mut visibility,
            &cold(),
            &mut NoImplicit,
        );
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
        let visible = select_tree_implicit(
            &ts.root,
            &e,
            &p,
            &mut PathArena::new(),
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&visible.render),
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]]),
            "in view ⇒ full detail (refine to grandchildren)"
        );
        let offscreen = select_tree_implicit(
            &ts.root,
            &e,
            &p,
            &mut PathArena::new(),
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Offscreen,
            &cold(),
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&offscreen.render),
            BTreeSet::from([vec![0], vec![1]]),
            "off view ⇒ coarse shell (stop at the children, the culled SSE target)"
        );
    }

    #[test]
    fn distant_tiles_refine_to_the_mid_demoted_distant_sse() {
        // The 3-way split (2026-06-10 ground cut flood): Distant (in-frustum far vista) sits BETWEEN
        // Visible and Offscreen. Same 3000 m offset as the test above — mid-children at SSE ≈ 31 px:
        // above the 16 px view target, BELOW the 32 px distant target, below the 64 px culled target —
        // so a Distant tile stops exactly where an Offscreen one does here, but a finer distant target
        // (24 px) refines past them while the culled target still would not.
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        let p = params_at(3000.0);
        let distant = select_tree_implicit(
            &ts.root,
            &e,
            &p,
            &mut PathArena::new(),
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Distant,
            &cold(),
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&distant.render),
            BTreeSet::from([vec![0], vec![1]]),
            "distant ⇒ stops at the children (31 px < the 32 px distant target)"
        );
        let mut finer = p;
        finer.distant_screen_space_error = 24.0; // now 31 px EXCEEDS the distant target ⇒ refine on
        let distant_finer = select_tree_implicit(
            &ts.root,
            &e,
            &finer,
            &mut PathArena::new(),
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Distant,
            &cold(),
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&distant_finer.render),
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]]),
            "a finer distant target refines Distant tiles deeper — it is read, not the culled one"
        );
    }

    #[test]
    fn kick_keeps_coarse_until_a_subtree_fully_loads_then_refines() {
        // Golden cross-frame trace (§9.5): grandchildren stream in over three frames; the selection state
        // persists between them. The kick must show coarse (no holes), refine each subtree atomically the
        // moment it's fully loaded, and never pop backward.
        use hashbrown::HashSet;
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        let p = params_at(2000.0); // near → wants the gE-10 grandchildren
        let mut loaded: HashSet<Vec<u32>> = HashSet::from([vec![], vec![0], vec![1]]); // mids loaded, leaves not
        let mut state = cold();
        // ONE arena across all three frames: the threaded `state` is keyed by its node ids.
        let arena = &mut PathArena::new();

        // Frame 1: no grandchild is loaded → each loaded mid-tile KICKS to its own coarse content (no holes);
        // all four grandchildren are queued.
        let s1 = select_tree_implicit(
            &ts.root,
            &e,
            &p,
            arena,
            &mut |q: &[u32], _| loaded.contains(q),
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &state,
            &mut NoImplicit,
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
        let s2 = select_tree_implicit(
            &ts.root,
            &e,
            &p,
            arena,
            &mut |q: &[u32], _| loaded.contains(q),
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &state,
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&s2.render),
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1]]),
            "frame 2: [0] fully loaded → fine; [1] still coarse"
        );
        state = s2.state;

        // Frame 3: [1]'s grandchildren arrive → full detail everywhere; the traversal has converged.
        loaded.extend([vec![1, 0], vec![1, 1]]);
        let s3 = select_tree_implicit(
            &ts.root,
            &e,
            &p,
            arena,
            &mut |q: &[u32], _| loaded.contains(q),
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &state,
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&s3.render),
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]]),
            "frame 3: converged to full detail"
        );
    }

    /// `mustContinueRefiningToDeeperTiles`: a tile that NOW meets SSE but was `Refined` last frame and isn't
    /// resident keeps showing its finer (loaded) descendants instead of popping to a coarse stand-in — the
    /// anti-detail-VANISH rule (zoom-out / pull-back). Here the camera sits where the MID level [0]/[1]
    /// (gE=100) meets SSE=16 but the root (gE=1000) does not; the mid tiles are NOT resident while their leaves
    /// ARE. Without the rule, [0]/[1] would render their coarse loaded ancestor (the root) and the detail would
    /// collapse; with it, the leaves keep drawing and the mid tiles are queued so they can later take over.
    #[test]
    fn must_continue_refining_holds_shown_detail_until_the_coarser_tile_loads() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        let p = params_at(7000.0); // gE=100 meets SSE 16 (≈13.4); gE=1000 does not (≈134) → root refines
                                   // Resident: root + the four leaves, but NOT the mid tiles [0],[1].
        let loaded: BTreeSet<Vec<u32>> = BTreeSet::from([vec![], vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]]);
        // Last frame: root + mids `Refined`, leaves `Rendered` → `refined_last` holds for [0],[1]. The
        // prior state is node-keyed, so intern its paths in the SAME arena the pass will use.
        let arena = &mut PathArena::new();
        let mut state = SelectionState::new();
        for refined in [&[][..], &[0], &[1]] {
            state.insert(arena.intern_path(refined), SelectionResult::Refined);
        }
        for leaf in [&[0u32, 0][..], &[0, 1], &[1, 0], &[1, 1]] {
            state.insert(arena.intern_path(leaf), SelectionResult::Rendered);
        }
        let sel = select_tree_implicit(
            &ts.root,
            &e,
            &p,
            arena,
            &mut |q: &[u32], _| loaded.contains(q),
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &state,
            &mut NoImplicit,
        );
        // Detail held: the leaves keep rendering (NOT collapsed to the coarse root).
        assert_eq!(
            paths(&sel.render),
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]]),
            "must_continue keeps the already-shown leaves instead of popping to the coarse root"
        );
        // And the now-SSE-adequate mid tiles are queued so they can take over (coarsen) once loaded.
        let load_paths = paths(&sel.load);
        assert!(
            load_paths.contains(&vec![0]) && load_paths.contains(&vec![1]),
            "the now-SSE-adequate mid tiles are queued to load so they can replace their leaves, got {load_paths:?}"
        );
    }

    /// The companion to the rule above: WITHOUT the prior `Refined` state (a cold pass), the same view+residency
    /// must NOT hold deep detail — a mid tile that meets SSE but isn't loaded falls back to its coarse ancestor.
    #[test]
    fn cold_pass_does_not_hold_unrequested_deep_detail() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        let p = params_at(7000.0);
        let loaded: BTreeSet<Vec<u32>> = BTreeSet::from([vec![], vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]]);
        let sel = select_tree_implicit(
            &ts.root,
            &e,
            &p,
            &mut PathArena::new(),
            &mut |q: &[u32], _| loaded.contains(q),
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(), // no prior Refined state ⇒ no must_continue
            &mut NoImplicit,
        );
        // Mid tiles meet SSE and aren't loaded ⇒ they render the coarse loaded root, not their leaves.
        assert_eq!(
            paths(&sel.render),
            BTreeSet::from([vec![]]),
            "cold pass: mid tiles meet SSE and aren't loaded ⇒ fall back to the coarse root"
        );
    }

    /// #7 `preloadAncestors`: on a COLD approach (nothing resident, no prior state) to a region whose subtree
    /// refines below the kick threshold, the coarse-ancestor chain is queued top-down so a coarse base lands
    /// before the fine frontier — instead of only the deep leaves loading (no coarse stand-in until they do).
    /// The interior refining tiles (root + mids), which have NO loaded ancestor (`coverage.is_none()`), enqueue
    /// their own content; without the arm only the 4 leaves would (< the kick threshold, so no kick fires).
    #[test]
    fn cold_approach_warms_the_coarse_ancestor_chain() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        let sel = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(2000.0), // close: root→mids→leaves all refine
            &mut PathArena::new(),
            &mut |_, _| false, // NOTHING resident (cold)
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(), // no prior state
            &mut NoImplicit,
        );
        let load = paths(&sel.load);
        // The coarse ancestors are warmed (queued), not just the deep leaves.
        assert!(
            load.contains(&vec![]) && load.contains(&vec![0]) && load.contains(&vec![1]),
            "cold approach must queue the coarse ancestor chain (root + mids), got {load:?}"
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
        // box: radius = the farthest corner = √(3²+4²+12²) = 13 for these orthogonal half-axes (the EXACT
        // circumscribed sphere). NOT the old `|u|+|v|+|w|` = 19 L1 over-bound (√169 vs 3+4+12) — that was
        // √(169)/19 ≈ 1.46× too large here (up to √3× for a cube), over-streaming view-edge tiles.
        let (_, rb) = bounding_sphere_ecef(
            &BoundingVolume::Box([0.0, 0.0, 0.0, 3.0, 0.0, 0.0, 0.0, 4.0, 0.0, 0.0, 0.0, 12.0]),
            &id,
            &e,
        );
        assert!((rb - 13.0).abs() < 1e-9);
        // Sheared (non-orthogonal) axes: the per-corner max must stay a SAFE OVER-bound (≥ every corner
        // distance), where a bare √Σ|·|² would UNDER-bound. Axes u=v=w=(1,0,0) collapse to a degenerate box
        // whose farthest "corner" is at |u+v+w| = 3; √Σ|·|² would give √3 ≈ 1.73 < 3 (unsafe). We must get 3.
        let (_, rsheared) = bounding_sphere_ecef(
            &BoundingVolume::Box([0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0]),
            &id,
            &e,
        );
        assert!((rsheared - 3.0).abs() < 1e-9, "sheared box must not under-bound");
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
    fn refining_interiors_are_queued_as_the_ladder_even_under_a_resident_ancestor() {
        // The REFINEMENT LADDER (2026-06-10, replaces the old `coverage.is_none()` self-limiting preload —
        // that suppression starved the atomic reveal: a kicked ancestor releases per LEVEL, so the unloaded
        // intermediate levels must load even when a coarse ancestor is already resident and covering).
        use hashbrown::HashSet;
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        let p = params_at(2000.0); // near → root→mids→leaves all refine

        // (a) Root resident + one grandchild: the unloaded interior mid-tiles ARE queued (the ladder), along
        // with the unloaded leaves — everything the cut needs to descend one level at a time.
        let with_root: HashSet<Vec<u32>> = HashSet::from([vec![], vec![0, 0]]);
        let s_cov = select_tree_implicit(
            &ts.root,
            &e,
            &p,
            &mut PathArena::new(),
            &mut |q: &[u32], _| with_root.contains(q),
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        let load = paths(&s_cov.load);
        assert!(
            load.contains(&vec![0]) && load.contains(&vec![1]),
            "unloaded refining interiors are queued (the ladder), got {load:?}"
        );

        // (b) NOTHING resident: the whole chain (root + mids + leaves) queues — the cold-approach warm.
        let s_cold = select_tree_implicit(
            &ts.root,
            &e,
            &p,
            &mut PathArena::new(),
            &mut |_, _| false,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        assert!(
            paths(&s_cold.load).contains(&vec![]),
            "with no resident ancestor the root itself is queued too, got {:?}",
            paths(&s_cold.load)
        );
    }

    #[test]
    fn state_records_the_interior_splits_and_leaf_renders() {
        let ts = nested_tileset();
        let e = Ellipsoid::wgs84();
        // Near camera, all loaded → root + both mid-children Refined; the four grandchildren Rendered.
        // The state is node-keyed; map ids back to paths through the arena for the assertions.
        let arena = &mut PathArena::new();
        let sel = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(2000.0),
            arena,
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        let by_result = |want: SelectionResult| -> BTreeSet<Vec<u32>> {
            sel.state
                .iter()
                .filter(|(_, &r)| r == want)
                .map(|(&n, _)| arena.path_of(n))
                .collect()
        };
        assert_eq!(
            by_result(SelectionResult::Refined),
            BTreeSet::from([vec![], vec![0], vec![1]])
        );
        assert_eq!(
            by_result(SelectionResult::Rendered),
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
        let stateless = select_tree_implicit(
            &ts.root,
            &e,
            &p,
            &mut PathArena::new(),
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&stateless.render),
            BTreeSet::from([vec![0], vec![1]]),
            "cold: SSE 14 < 16 ⇒ render the children, do not split"
        );

        // Hysteretic: same view, but the children were `Refined` last frame → they hold their split because
        // SSE 14 > 16/(1+0.25)=12.8. This is the anti-pop dead-band: no parent↔children flip on the threshold.
        // The prior state is node-keyed: intern its paths in the SAME arena the pass uses.
        p.hysteresis = 0.25;
        let arena = &mut PathArena::new();
        let prev = SelectionState::from([
            (arena.intern_path(&[0]), SelectionResult::Refined),
            (arena.intern_path(&[1]), SelectionResult::Refined),
        ]);
        let hyst = select_tree_implicit(
            &ts.root,
            &e,
            &p,
            arena,
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &prev,
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&hyst.render),
            BTreeSet::from([vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]]),
            "hysteresis keeps the previously-refined children split into grandchildren"
        );
    }

    #[test]
    fn non_finite_bounding_volume_drops_the_subtree() {
        // Hostile input (H7): every numeric literal here is finite (serde_json rejects ∞/out-of-range numbers
        // at parse), but child [1] pairs a near-`f64::MAX` sphere center (1e308) with a ×10 scale `transform`.
        // The transform PRODUCT (10 · 1e308 = 1e309) overflows to ∞ — a non-finiteness no per-field parse check
        // can catch, only the visit-time guard on the computed center. Such a tile must be dropped: its distance
        // would otherwise read as 0 (via `∞.max(0.0)`) and force its whole subtree to refine + stream.
        let json = format!(
            r#"{{ "asset": {{"version":"1.1"}}, "geometricError": 2000,
              "root": {{ "boundingVolume": {{"sphere":[{C},0,0,1]}}, "geometricError": 1000, "refine":"REPLACE",
                "content": {{"uri":"r.glb"}}, "children": [
                  {{ "boundingVolume": {{"sphere":[{C},0,0,1]}}, "geometricError": 10, "content": {{"uri":"ok.glb"}} }},
                  {{ "boundingVolume": {{"sphere":[1e308,0,0,1]}}, "geometricError": 10, "content": {{"uri":"bad.glb"}},
                     "transform": [10,0,0,0, 0,10,0,0, 0,0,10,0, 0,0,0,1] }} ] }} }}"#
        );
        let ts = parse_tileset(json.as_bytes()).expect("all literals are finite — parses fine");
        // Sanity: the bad child's computed center really is non-finite (so the guard, not the parser, protects).
        let world = ts.root.matrix() * ts.root.children[1].matrix();
        let (center, _) = bounding_sphere_ecef(&ts.root.children[1].bounding_volume, &world, &Ellipsoid::wgs84());
        assert!(
            !center.is_finite(),
            "transform product should overflow to ∞, got {center:?}"
        );

        let e = Ellipsoid::wgs84();
        let sel = select_tree_implicit(
            &ts.root,
            &e,
            &params_at(2000.0),
            &mut PathArena::new(),
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        // The valid near child renders; the non-finite child contributes nothing to render OR load.
        assert_eq!(paths(&sel.render), BTreeSet::from([vec![0]]));
        assert!(sel.load.is_empty(), "the dropped subtree queues nothing");
    }

    /// Build a single linear chain of `depth` edges: the root's only descendant path is `depth` zeros, ending
    /// in a leaf (no children). Interiors carry a huge geometric error so they always want to refine; the leaf
    /// renders. Used to exercise the C1 depth cap without a network/graft harness.
    fn linear_chain(depth: usize) -> Tile {
        let bv = BoundingVolume::Sphere([C, 0.0, 0.0, 1.0]);
        let leaf = || Tile {
            bounding_volume: bv,
            geometric_error: 0.0, // a leaf renders regardless; 0 just makes "meets SSE" explicit
            refine: Some(logi_3dtiles_tileset::Refine::Replace),
            transform: None,
            content: None,
            contents: Vec::new(),
            children: Vec::new(),
            implicit_tiling: None,
            viewer_request_volume: None,
            bounds_memo: std::cell::OnceCell::new(),
        };
        let mut t = leaf();
        for _ in 0..depth {
            t = Tile {
                bounding_volume: bv,
                geometric_error: 1.0e9, // huge ⇒ always refine into the single child
                refine: Some(logi_3dtiles_tileset::Refine::Replace),
                transform: None,
                content: None,
                contents: Vec::new(),
                children: vec![t],
                implicit_tiling: None,
                viewer_request_volume: None,
                bounds_memo: std::cell::OnceCell::new(),
            };
        }
        t
    }

    #[test]
    fn deep_within_cap_chain_refines_to_its_leaf() {
        // A legitimately deep (but within-cap) chain still refines all the way to its leaf — the cap must not
        // clip real trees. Depth 40 ≪ MAX_TILE_DEPTH; everything loaded so there's no kick.
        let e = Ellipsoid::wgs84();
        let root = linear_chain(40);
        let sel = select_tree_implicit(
            &root,
            &e,
            &params_at(2000.0),
            &mut PathArena::new(),
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        assert_eq!(
            paths(&sel.render),
            BTreeSet::from([vec![0u32; 40]]),
            "refines to the depth-40 leaf"
        );
    }

    #[test]
    fn deep_chain_beyond_cap_is_bounded() {
        // A pathological chain DEEPER than the cap (stand-in for an unbounded external-tileset graft chain).
        // The traversal must terminate (completing this test is the no-stack-overflow proof) and never turn a
        // tile deeper than the cap into a selected/loaded tile. Without the cap, the depth-(cap+50) leaf would
        // render at a path longer than the cap and this bound would fail.
        // Run on a generous stack (matching the production stream thread): the traversal recurses to the cap
        // (MAX_TILE_DEPTH=512) and a debug-build frame × 512 exceeds the 2 MB default test-thread stack. The
        // point under test is the CAP (no tile past it) + termination, not the thread's stack size.
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(|| {
                let e = Ellipsoid::wgs84();
                let root = linear_chain(MAX_TILE_DEPTH as usize + 50);
                let sel = select_tree_implicit(
                    &root,
                    &e,
                    &params_at(2000.0),
                    &mut PathArena::new(),
                    &mut |_, _| true,
                    &mut |_, _| false,
                    &mut |_, _| Visibility::Visible,
                    &cold(),
                    &mut NoImplicit,
                );
                assert!(
                    sel.render
                        .iter()
                        .chain(&sel.load)
                        .all(|t| t.path.len() <= MAX_TILE_DEPTH as usize),
                    "no selected tile may sit deeper than the cap"
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }

    const WGS84_ISH: f64 = 6_378_137.0;

    // ---- Implicit tiling: drive the seam with an in-memory `Subtree` (the codec) ------------------
    //
    // These prove the codec→traversal wiring WITHOUT the scene/fetch layer: a test [`AvailabilityProvider`]
    // backed by a `logi_3dtiles_implicit::Subtree` (built directly, in memory) answers the traversal's
    // availability queries; the assertions check that a known availability pattern yields exactly the expected
    // synthesized children at each level, with correct template URLs + subdivided bounds, and that unavailable
    // / pending subtrees prune / kick. The implicit Morton + bit-index math is the codec's (unit-tested there);
    // we lean on it via `Subtree`, never reimplement it.

    use logi_3dtiles_implicit::{bit_index, Availability, Subtree};
    // `subdivide_bounds`, `ImplicitTiling`, `SubdivisionScheme`, `BoundingVolume`, `Tile` already come via
    // `super::*`; only `Content` + `TemplateUri` (used to build the test root tile) are new here.
    use logi_3dtiles_tileset::{Content, TemplateUri};

    /// Build a tile-availability `Subtree` (single internal "buffer" — just a `Vec<u8>` bitstream) in which
    /// exactly the listed global `(level, x, y, z)` tiles are available, content available everywhere a tile
    /// is. `child_count` = 4 (quadtree) / 8 (octree). For these tests the subtree's root coincides with the
    /// implicit root (global coords == subtree-relative coords), so we index bits directly by global coord.
    fn subtree_with(available: &[(u32, u32, u32, u32)], child_count: u32) -> Subtree {
        // Highest bit index we must address → bitstream byte length.
        let max_bit = available
            .iter()
            .map(|&(l, x, y, z)| {
                let m = if child_count == 8 {
                    logi_3dtiles_implicit::morton_3d(x, y, z)
                } else {
                    logi_3dtiles_implicit::morton_2d(x, y)
                };
                bit_index(l, m, child_count)
            })
            .max()
            .unwrap_or(0);
        let mut bytes = vec![0u8; (max_bit as usize / 8) + 1];
        for &(l, x, y, z) in available {
            let m = if child_count == 8 {
                logi_3dtiles_implicit::morton_3d(x, y, z)
            } else {
                logi_3dtiles_implicit::morton_2d(x, y)
            };
            let idx = bit_index(l, m, child_count);
            bytes[(idx >> 3) as usize] |= 1 << (idx & 7);
        }
        Subtree {
            child_count,
            tile_availability: Availability::Bitstream(bytes),
            content_availability: vec![Availability::Constant(true)], // content wherever a tile exists
            child_subtree_availability: Availability::Constant(false), // single subtree (no children)
        }
    }

    /// A test provider over ONE in-memory root `Subtree` whose root is the implicit root (so global coords are
    /// subtree-relative). Levels within the subtree answer from its bitstreams; a deeper level (no child
    /// subtree registered) is reported [`TileAvailability::Pending`] when `pending_below` is set — exercising
    /// the not-yet-fetched refinement boundary. Records every coordinate it was asked about (for assertions).
    struct MapProvider {
        subtree: Subtree,
        subtree_levels: u32,
        /// When `Some(n)`, a query at `level >= n` (beyond the loaded subtree) returns `Pending` rather than
        /// reading the (absent) child subtree — the "must fetch the next subtree" signal.
        pending_below: Option<u32>,
        asked: std::cell::RefCell<Vec<(u32, u32, u32, u32)>>,
    }

    impl MapProvider {
        fn new(subtree: Subtree, subtree_levels: u32) -> Self {
            Self {
                subtree,
                subtree_levels,
                pending_below: None,
                asked: std::cell::RefCell::new(Vec::new()),
            }
        }
        fn pending_below(mut self, level: u32) -> Self {
            self.pending_below = Some(level);
            self
        }
    }

    impl AvailabilityProvider for MapProvider {
        fn tile_availability(
            &mut self,
            _: &ImplicitContext,
            _: &PathArena,
            level: u32,
            x: u32,
            y: u32,
            z: u32,
        ) -> TileAvailability {
            self.asked.borrow_mut().push((level, x, y, z));
            if let Some(p) = self.pending_below {
                if level >= p {
                    return TileAvailability::Pending; // the covering child `.subtree` isn't fetched yet
                }
            }
            // The root subtree covers levels 0..subtree_levels; deeper levels would need a child subtree.
            if level >= self.subtree_levels {
                return TileAvailability::Pending;
            }
            if self.subtree.is_tile_available(level, x, y, z) {
                TileAvailability::Available
            } else {
                TileAvailability::Unavailable
            }
        }
        fn content_availability(
            &mut self,
            _: &ImplicitContext,
            _: &PathArena,
            level: u32,
            x: u32,
            y: u32,
            z: u32,
        ) -> bool {
            self.subtree.is_content_available(0, level, x, y, z)
        }
    }

    /// An implicit-root tile: a `region` root with a `{level}/{x}/{y}` content template + `implicitTiling`.
    /// `available_levels` bounds the implicit tree's depth; `subtree_levels` is informational for the provider.
    fn implicit_root(
        region: [f64; 6],
        geometric_error: f64,
        scheme: SubdivisionScheme,
        subtree_levels: u32,
        available_levels: u32,
    ) -> Tile {
        Tile {
            bounding_volume: BoundingVolume::Region(region),
            geometric_error,
            refine: Some(logi_3dtiles_tileset::Refine::Replace),
            transform: None,
            content: Some(Content {
                uri: "content/{level}/{x}/{y}.glb".to_string(),
                bounding_volume: None,
            }),
            contents: Vec::new(),
            children: Vec::new(),
            implicit_tiling: Some(ImplicitTiling {
                subdivision_scheme: scheme,
                subtree_levels,
                available_levels,
                subtrees: TemplateUri {
                    uri: "subtrees/{level}/{x}/{y}.subtree".to_string(),
                },
            }),
            viewer_request_volume: None,
            bounds_memo: std::cell::OnceCell::new(),
        }
    }

    /// A small equator region patch (≈ a few hundred km on a side) the camera can sit near, so the implicit
    /// levels span a useful SSE range. Heights 0..100.
    fn small_region() -> [f64; 6] {
        [-0.02, -0.02, 0.02, 0.02, 0.0, 100.0]
    }

    /// Camera near the centre of `small_region()` (just above the equator surface point), looking at it. Offset
    /// metres above the surface; everything in view + loaded so no kick unless a test forces it.
    fn implicit_params(offset: f64) -> SelectParams {
        SelectParams {
            camera_position: DVec3::new(C + offset, 0.0, 0.0),
            fov_y_radians: std::f64::consts::FRAC_PI_3,
            viewport_height: 1080.0,
            max_screen_space_error: 16.0,
            distant_screen_space_error: 32.0,
            culled_screen_space_error: 64.0,
            hysteresis: 0.0,
        }
    }

    #[test]
    fn implicit_root_with_no_provider_refines_to_nothing() {
        // The whole point of the bug: WITHOUT a provider, an implicit root is stuck at the coarsest LOD. With
        // the `NoImplicit` provider, it must render ONLY the root (loaded) — never
        // descend — exactly the silent-stuck-planet behavior we're fixing only when a provider IS supplied.
        let e = Ellipsoid::wgs84();
        let root = implicit_root(small_region(), 5000.0, SubdivisionScheme::Quadtree, 3, 3);
        let sel = select_tree_implicit(
            &root,
            &e,
            &implicit_params(2000.0), // near → would refine deep IF children existed
            &mut PathArena::new(),
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            &mut NoImplicit,
        );
        assert_eq!(paths(&sel.render), BTreeSet::from([vec![]]), "no provider ⇒ root only");
        assert!(sel.load.is_empty());
    }

    #[test]
    fn implicit_quadtree_refines_to_available_children_with_urls_and_bounds() {
        // Availability: root (0,0,0,0), all four level-1 children, and under child (0,0) one level-2 tile
        // (0,0). `available_levels = 3`. Near camera + everything loaded ⇒ refine to the deepest available.
        let e = Ellipsoid::wgs84();
        let region = small_region();
        let avail = [
            (0, 0, 0, 0),
            (1, 0, 0, 0),
            (1, 1, 0, 0),
            (1, 0, 1, 0),
            (1, 1, 1, 0),
            (2, 0, 0, 0), // only this one level-2 tile exists
        ];
        let mut provider = MapProvider::new(subtree_with(&avail, 4), 3);
        let root = implicit_root(region, 5000.0, SubdivisionScheme::Quadtree, 3, 3);
        let sel = select_implicit_tree(&root, &e, &implicit_params(50.0), &mut provider);

        // Expected RENDER set: the available leaves of the SSE-refined tree. With a very near camera the
        // traversal refines every available branch to its deepest available tile:
        //   - under (0,0): level-2 (0,0) exists → path [0, 0] renders (its parent [0] refines)
        //   - the other three level-1 children have NO level-2 tiles → they are leaves → [1], [2], [3] render
        // (child-index steps: (0,0)→0, (1,0)→1, (0,1)→2, (1,1)→3; the level-2 (0,0) under parent (0,0)→step 0.)
        assert_eq!(
            paths(&sel.render),
            BTreeSet::from([vec![0, 0], vec![1], vec![2], vec![3]]),
            "refine each available branch to its deepest available tile"
        );

        // RENDER tiles carry no content URIs (audit 2026-06-10 §4.7 — never consumed downstream). URL
        // substitution is checked below through the LOAD set of a nothing-loaded pass.
        let by_path = |p: &[u32]| sel.render.iter().find(|t| t.path == p).unwrap();
        assert!(by_path(&[0, 0]).content_uris.is_empty(), "render tiles carry no URIs");

        // URL substitution (LOAD flavor): rerun the same view with NOTHING loaded — the queued tiles carry
        // their templated content URIs. [0,0] is implicit level 2, global (0,0) → content/2/0/0.glb; [1] is
        // level 1, global (1,0) → content/1/1/0.glb; [2] is level 1, global (0,1) → content/1/0/1.glb.
        let mut provider2 = MapProvider::new(subtree_with(&avail, 4), 3);
        let cold_sel = select_implicit_tree_loaded(&root, &e, &implicit_params(50.0), &mut provider2, &|_| false);
        let by_load = |p: &[u32]| cold_sel.load.iter().find(|t| t.path == p).unwrap();
        assert_eq!(by_load(&[0, 0]).content_uris, vec!["content/2/0/0.glb".to_string()]);
        assert_eq!(by_load(&[1]).content_uris, vec!["content/1/1/0.glb".to_string()]);
        assert_eq!(by_load(&[2]).content_uris, vec!["content/1/0/1.glb".to_string()]);

        // Bounds: a synthesized tile's sphere must equal `bounding_sphere_ecef(subdivide_bounds(...))`. Check
        // the [1] tile (level 1, coord (1,0)) against the spec subdivision of the root region.
        let want_bv = subdivide_bounds(&BoundingVolume::Region(region), SubdivisionScheme::Quadtree, 1, 1, 0, 0);
        let (wc, wr) = bounding_sphere_ecef(&want_bv, &DMat4::IDENTITY, &e);
        let t1 = by_path(&[1]);
        assert!(
            (t1.bounds_center - wc).length() < 1e-6,
            "bounds center matches subdivision"
        );
        assert!(
            (t1.bounds_radius - wr).abs() < 1e-6,
            "bounds radius matches subdivision"
        );

        // Geometric error halves per level: root 5000 → level 1 = 2500, level 2 = 1250.
        assert!((by_path(&[1]).geometric_error - 2500.0).abs() < 1e-9);
        assert!((by_path(&[0, 0]).geometric_error - 1250.0).abs() < 1e-9);
    }

    #[test]
    fn implicit_unavailable_child_is_pruned() {
        // Root + three of four level-1 children available; child (1,1) [step 3] is NOT. `available_levels=2`
        // so level-1 tiles are leaves. The missing child must contribute nothing to render OR load.
        let e = Ellipsoid::wgs84();
        let avail = [(0, 0, 0, 0), (1, 0, 0, 0), (1, 1, 0, 0), (1, 0, 1, 0)]; // (1,1,0) absent
        let mut provider = MapProvider::new(subtree_with(&avail, 4), 2);
        let root = implicit_root(small_region(), 5000.0, SubdivisionScheme::Quadtree, 2, 2);
        let sel = select_implicit_tree(&root, &e, &implicit_params(50.0), &mut provider);
        assert_eq!(
            paths(&sel.render),
            BTreeSet::from([vec![0], vec![1], vec![2]]),
            "only the three available level-1 children render; the absent (1,1) [step 3] is pruned"
        );
        assert!(
            !sel.render.iter().any(|t| t.path == [3]),
            "no tile at the unavailable coordinate"
        );
        assert!(!sel.load.iter().any(|t| t.path == [3]));
    }

    #[test]
    fn implicit_pending_subtree_kicks_to_the_coarse_root() {
        // The root subtree spans only level 0 (`subtree_levels = 1`): the four level-1 children live in child
        // subtrees that aren't fetched → the provider reports them `Pending`. With ONLY the root loaded, the
        // traversal must KICK to the coarse root (no holes) and queue the root, exactly like a fresh unloaded
        // explicit refinement — converging once the child subtrees arrive (a later frame, not modeled here).
        let e = Ellipsoid::wgs84();
        let avail = [(0, 0, 0, 0)]; // only the root tile is in the loaded subtree
        let mut provider = MapProvider::new(subtree_with(&avail, 4), 4).pending_below(1);
        let root = implicit_root(small_region(), 5000.0, SubdivisionScheme::Quadtree, 4, 4);
        // Only the implicit root (path []) is resident; its children are Pending.
        let sel = select_implicit_tree_loaded(&root, &e, &implicit_params(50.0), &mut provider, &|p| p.is_empty());
        assert_eq!(
            paths(&sel.render),
            BTreeSet::from([vec![]]),
            "pending children ⇒ kick to the coarse root (no holes)"
        );
    }

    #[test]
    fn implicit_octree_uses_the_z_axis() {
        // Octree: root + two level-1 children that DIFFER ONLY IN Z — (0,0,0) [step 0] and (0,0,1) [step 4].
        // This proves z participates: a quadtree (ignoring z) could never distinguish or place them. With
        // `available_levels = 2` they are leaves.
        let e = Ellipsoid::wgs84();
        let avail = [(0, 0, 0, 0), (1, 0, 0, 0), (1, 0, 0, 1)];
        let mut provider = MapProvider::new(subtree_with(&avail, 8), 2);
        let root = implicit_root(small_region(), 5000.0, SubdivisionScheme::Octree, 2, 2);
        let sel = select_implicit_tree(&root, &e, &implicit_params(50.0), &mut provider);
        assert_eq!(
            paths(&sel.render),
            BTreeSet::from([vec![0], vec![4]]),
            "the two z-differing octree children render at steps 0 and 4"
        );
        // Their content URLs carry the z coordinate: (0,0,0)→content/1/0/0.glb, (0,0,1)→content/1/0/0.glb.
        // (The {z} placeholder is absent from this template, so both fill {level}/{x}/{y} identically — the
        // distinction is in their BOUNDS, which we check: the upper child sits higher in ECEF.)
        let by_path = |p: &[u32]| sel.render.iter().find(|t| t.path == p).unwrap();
        let lower = by_path(&[0]); // z = 0 → lower height slab
        let upper = by_path(&[4]); // z = 1 → upper height slab

        // Upper slab's center is farther from the geocenter (greater height) than the lower slab's:
        assert!(
            upper.bounds_center.length() > lower.bounds_center.length(),
            "octree z=1 child must sit higher (greater ECEF radius) than z=0: upper {:.3} vs lower {:.3}",
            upper.bounds_center.length(),
            lower.bounds_center.length()
        );
    }

    #[test]
    fn implicit_far_camera_renders_only_the_root() {
        // Sanity that implicit SSE works like explicit: a far camera meets SSE at the root and does NOT
        // descend, even though children are available — so the provider's children are never even queried past
        // the first level (the root itself renders).
        let e = Ellipsoid::wgs84();
        let avail = [(0, 0, 0, 0), (1, 0, 0, 0), (1, 1, 0, 0), (1, 0, 1, 0), (1, 1, 1, 0)];
        let mut provider = MapProvider::new(subtree_with(&avail, 4), 2);
        let root = implicit_root(small_region(), 50.0, SubdivisionScheme::Quadtree, 2, 2); // tiny gE
        let sel = select_implicit_tree(&root, &e, &implicit_params(5_000_000.0), &mut provider); // very far
        assert_eq!(
            paths(&sel.render),
            BTreeSet::from([vec![]]),
            "far ⇒ root meets SSE, no refine"
        );
        assert!(sel.load.is_empty());
    }

    #[test]
    fn implicit_sse_stops_refining_when_the_level_meets_the_target() {
        // Implicit SSE must be REAL (driven by the per-level geometric error), not "refine until no children".
        // A 4-level-available tree, all coords available, but a MID-distance camera: tuned so the level-1
        // children meet the 16 px target (so refinement stops AT them) while the root does not (it refines).
        // If implicit refinement ignored SSE it would descend to level 3 instead.
        let e = Ellipsoid::wgs84();
        let region = small_region();
        // Every tile up to level 3 available (root + full quadtree to level 3).
        let mut avail = vec![(0u32, 0u32, 0u32, 0u32)];
        for level in 1..=3u32 {
            let span = 1u32 << level;
            for x in 0..span {
                for y in 0..span {
                    avail.push((level, x, y, 0));
                }
            }
        }
        let mut provider = MapProvider::new(subtree_with(&avail, 4), 4);
        // Root geometric error 200 m; level-1 = 100, level-2 = 50. Pick a distance where level-1 (gE 100) sits
        // just under the 16 px target but the root (gE 200) sits above it → refine the root, stop at level 1.
        let root = implicit_root(region, 200.0, SubdivisionScheme::Quadtree, 4, 4);
        // Far enough that the gE-100 level-1 tiles meet SSE. (Found empirically against the SSE formula; the
        // point is ONLY that some level meets the target before the deepest available level.)
        let sel = select_implicit_tree(&root, &e, &implicit_params(60_000.0), &mut provider);
        let max_depth = sel.render.iter().map(|t| t.path.len()).max().unwrap_or(0);
        assert!(
            (1..=2).contains(&max_depth),
            "SSE should stop refining at a shallow level (1–2), not descend to the deepest available (3); \
             got max depth {max_depth} with render {:?}",
            paths(&sel.render)
        );
        assert!(
            !sel.render.iter().any(|t| t.path.is_empty()),
            "the root refined (it does not meet SSE), so the root tile itself does not render"
        );
    }

    /// Helper: run `select_tree_implicit` with everything loaded + everything visible (the common case for the
    /// structural implicit tests above).
    fn select_implicit_tree(
        root: &Tile,
        e: &Ellipsoid,
        params: &SelectParams,
        provider: &mut dyn AvailabilityProvider,
    ) -> Selection {
        select_tree_implicit(
            root,
            e,
            params,
            &mut PathArena::new(),
            &mut |_, _| true,
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            provider,
        )
    }

    /// Helper: `select_tree_implicit` with a custom `is_loaded` predicate (for the kick test), all visible.
    fn select_implicit_tree_loaded(
        root: &Tile,
        e: &Ellipsoid,
        params: &SelectParams,
        provider: &mut dyn AvailabilityProvider,
        is_loaded: &dyn Fn(&[u32]) -> bool,
    ) -> Selection {
        select_tree_implicit(
            root,
            e,
            params,
            &mut PathArena::new(),
            &mut |p, _| is_loaded(p),
            &mut |_, _| false,
            &mut |_, _| Visibility::Visible,
            &cold(),
            provider,
        )
    }
}
