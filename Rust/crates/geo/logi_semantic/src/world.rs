//! The HE-0 baked-tile **runtime world**: open a store (a quadtree pyramid), triangulate every tile into
//! Unreal render space once, and serve a camera-driven **SSE-LOD** subset via [`BakedWorld::select`]
//! (coarse far, fine near — reusing `logi_geo_lod::screen_space_error`, the model the old
//! `select_hierarchical` used). The AOI is small (a 1-mile radius), so all tiles are meshed eagerly and
//! selection picks the visible mixed-level set. Deterministic; no `unsafe`.

use crate::field::{sample_tile, HeightField};
use crate::mesh::{triangulate, MeshingMode, TerrainMesh};
use crate::store::BakedStore;
use crate::{SemanticError, TileId};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// One resident tile ready to render: its id, mesh (Unreal cm, Z-up LH), and `geometric_error` (metres,
/// for SSE-LOD selection).
#[derive(Clone, Debug)]
pub struct BakedRenderTile {
    pub id: TileId,
    pub mesh: TerrainMesh,
    pub geometric_error: f32,
}

/// All baked tiles of an AOI — a quadtree **pyramid** (levels 0..=max), triangulated to Unreal render
/// space. The ENU origin is pinned at Unreal `(0,0,0)`; `origin_ecef` is that origin's WGS84 ECEF.
pub struct BakedWorld {
    origin_ecef: [f64; 3],
    tiles: Vec<BakedRenderTile>,
    index: HashMap<u64, usize>, // TileId.key → tiles index
    max_level: u8,
    // Anti-thrash hysteresis for [`BakedWorld::select`]: the tile keys that were *refined* (split into
    // children) on the previous call. A tile straddling the SSE target holds last call's refine/coarsen
    // decision until SSE leaves a dead-band, so smooth camera motion can't flip a parent↔4-children swap
    // every frame (which churns the C++ section/Nanite rebuild). `cur_refined` accumulates this call; the
    // two swap at the end. Empty ⇒ the old stateless threshold. (Mirrors `the retired stream-selector crate`.)
    prev_refined: HashSet<u64>,
    cur_refined: HashSet<u64>,
}

impl BakedWorld {
    /// Open a store directory (`manifest.json` + `tiles/`) and build every tile's render mesh with the
    /// default (uniform, no skirt) meshing.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, SemanticError> {
        Self::open_with(dir, MeshingMode::Uniform, 0.0)
    }

    /// Open a store and build every tile's render mesh with the given [`MeshingMode`] (the meshing toggle)
    /// and `skirt_depth_m` (0 = no skirt) — a downward edge wall that hides cross-tile T-junction cracks.
    pub fn open_with(dir: impl Into<PathBuf>, mode: MeshingMode, skirt_depth_m: f32) -> Result<Self, SemanticError> {
        let store = BakedStore::open(dir)?;
        let origin_ecef = store.manifest().origin.ecef;
        let manifest_tiles = &store.manifest().tiles;
        let mut tiles = Vec::with_capacity(manifest_tiles.len());
        let mut index = HashMap::with_capacity(manifest_tiles.len());
        let mut max_level = 0u8;
        // Read each tile straight from its manifest entry (O(1) per tile) — no `read_tile(id)` linear scan,
        // so opening an N-tile store is O(N), not O(N²).
        for entry in manifest_tiles {
            let id = TileId::new(entry.level, entry.x, entry.y);
            let tile = store.read_tile_entry(entry)?;
            let mesh = triangulate(&tile, mode, skirt_depth_m).into_ue_cm(); // ENU mesh discarded → move, no copy
            index.insert(id.key(), tiles.len());
            max_level = max_level.max(id.level);
            tiles.push(BakedRenderTile {
                id,
                mesh,
                geometric_error: entry.geometric_error,
            });
        }
        Ok(Self {
            origin_ecef,
            tiles,
            index,
            max_level,
            prev_refined: HashSet::new(),
            cur_refined: HashSet::new(),
        })
    }

    /// Build a world **procedurally** from a [`HeightField`] (the canonical `TerrainHeights` gameplay
    /// shares) instead of a baked store — the same SSE-LOD quadtree pyramid, generated on the fly. This is
    /// what lets the one terrain engine render a procedural world (the old `the retired render-scene crate`'s job)
    /// without a bake: it reuses [`sample_tile`] + [`triangulate`], so a procedural tile is byte-identical
    /// in layout to a baked one. `origin_ecef` is the AOI origin's WGS84 ECEF (caller-computed via
    /// `logi_geospatial`); the AOI is the `2·radius_m` square centred on it, split into `2^level` tiles per
    /// side per level `0..=leaf_level`. (Bounded + eager for now — meshes the whole pyramid up front; the
    /// residency-cache / lazy-stream path for very large worlds is the next refinement.)
    pub fn procedural(
        field: &impl HeightField,
        origin_ecef: [f64; 3],
        radius_m: f64,
        leaf_level: u8,
        grid_n: u32,
        mode: MeshingMode,
        skirt_depth_m: f32,
    ) -> Result<Self, SemanticError> {
        let root_size = 2.0 * radius_m;
        let mut tiles = Vec::new();
        let mut index = HashMap::new();
        // The root tile's relief sets the base geometric error; finer levels halve it (matches the bake).
        let mut base_ge = 1.0f32;
        for level in 0..=leaf_level {
            let per_side = 1u32 << level; // leaf_level is small + caller-clamped; no overflow in practice
            let tile_size = root_size / per_side as f64;
            for y in 0..per_side {
                for x in 0..per_side {
                    let origin_e = -radius_m + x as f64 * tile_size;
                    let origin_n = -radius_m + y as f64 * tile_size;
                    let id = TileId::new(level, x, y);
                    let st = sample_tile(field, id, grid_n, origin_e, origin_n, tile_size)?;
                    if level == 0 {
                        base_ge = (st.h_max - st.h_min).max(1.0);
                    }
                    let geometric_error = base_ge * 0.5f32.powi(level as i32);
                    let mesh = triangulate(&st, mode, skirt_depth_m).into_ue_cm(); // ENU mesh discarded → move, no copy
                    index.insert(id.key(), tiles.len());
                    tiles.push(BakedRenderTile {
                        id,
                        mesh,
                        geometric_error,
                    });
                }
            }
        }
        Ok(Self {
            origin_ecef,
            tiles,
            index,
            max_level: leaf_level,
            prev_refined: HashSet::new(),
            cur_refined: HashSet::new(),
        })
    }

    /// The AOI origin's WGS84 ECEF (metres) — the ENU frame pinned at Unreal `(0,0,0)`.
    pub fn origin_ecef(&self) -> [f64; 3] {
        self.origin_ecef
    }

    /// All resident render tiles (HE-0 serves the whole AOI).
    pub fn tiles(&self) -> &[BakedRenderTile] {
        &self.tiles
    }

    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }

    /// **SSE-LOD selection** over the baked pyramid: descend the quadtree from the root, refining a tile
    /// into its children while its screen-space error exceeds `sse_target` (and finer tiles exist). Yields
    /// the coarse-far / fine-near mixed-level visible set, reusing the kernel's
    /// `logi_geo_lod::screen_space_error` (the same model the old `select_hierarchical` used). No
    /// frustum/horizon cull yet — a later refinement. A single-level store (no root) renders everything.
    pub fn select(
        &mut self,
        camera_ue_cm: [f32; 3],
        fov_y_radians: f64,
        viewport_height: f64,
        sse_target: f64,
    ) -> Vec<TileId> {
        // Hysteresis dead-band (fraction of the SSE target): a refined tile coarsens only once SSE drops
        // below target/(1+H); a coarse tile still refines at the plain target. So a tile sitting on the
        // target holds its state instead of flip-flopping as the camera nudges SSE across the line.
        const SSE_HYSTERESIS: f64 = 0.25;
        let params = logi_geo_lod::RefineParams {
            fov_y_radians,
            viewport_height,
            sse_target,
            hysteresis: SSE_HYSTERESIS,
        };
        let root = TileId::new(0, 0, 0);
        if !self.index.contains_key(&root.key()) {
            return self.tiles.iter().map(|t| t.id).collect();
        }
        self.cur_refined.clear();
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(t) = stack.pop() {
            let Some(&idx) = self.index.get(&t.key()) else { continue };
            // Copy the scalars out so the immutable borrow of `self.tiles` ends before we touch
            // `self.cur_refined` below (disjoint-field borrows the checker can't see through an index).
            let (center, ge) = {
                let tile = &self.tiles[idx];
                (tile.mesh.bounds_center, tile.geometric_error as f64)
            };
            let dx = (center[0] - camera_ue_cm[0]) as f64;
            let dy = (center[1] - camera_ue_cm[1]) as f64;
            let dz = (center[2] - camera_ue_cm[2]) as f64;
            let dist_m = ((dx * dx + dy * dy + dz * dz).sqrt() / 100.0).max(1.0); // cm → m for SSE
                                                                                  // Refine criterion + hysteresis dead-band single-homed in `logi_geo_lod::should_refine`
                                                                                  // (audit M22): a tile refined last call holds until SSE falls below target/(1+H); a fresh tile
                                                                                  // uses the plain target (an empty `prev_refined` reproduces the old stateless behavior).
            let was_refined = self.prev_refined.contains(&t.key());
            let children = t.children();
            let has_children = children.iter().any(|ch| self.index.contains_key(&ch.key()));
            if logi_geo_lod::should_refine(ge, dist_m, &params, was_refined) && t.level < self.max_level && has_children
            {
                self.cur_refined.insert(t.key());
                for ch in children {
                    if self.index.contains_key(&ch.key()) {
                        stack.push(ch);
                    }
                }
            } else {
                out.push(t);
            }
        }
        // This call's refined set becomes next call's hysteresis reference.
        std::mem::swap(&mut self.prev_refined, &mut self.cur_refined);
        out
    }
}

/// True if a render-space bounding sphere (Unreal cm) is within `max_distance` cm of the camera — the
/// distance cull for camera-driven baked-tile streaming. `max_distance <= 0` disables culling (keep all).
pub fn within_cull_distance(bounds_center: [f32; 3], bounds_radius: f32, camera: [f32; 3], max_distance: f32) -> bool {
    if max_distance <= 0.0 {
        return true;
    }
    let dx = bounds_center[0] - camera[0];
    let dy = bounds_center[1] - camera[1];
    let dz = bounds_center[2] - camera[2];
    ((dx * dx + dy * dy + dz * dz).sqrt() - bounds_radius) <= max_distance
}

#[cfg(test)]
mod tests {
    use super::{within_cull_distance, BakedWorld};
    use crate::field::TerrainHeights;
    use crate::mesh::MeshingMode;
    use crate::TileId;

    #[test]
    fn procedural_world_builds_pyramid_and_selects() {
        let f = TerrainHeights::canonical();
        // 512 m radius AOI, pyramid levels 0..=2 → 1 + 4 + 16 = 21 tiles, generated from the field.
        let mut w = BakedWorld::procedural(&f, [6_371_000.0, 0.0, 0.0], 512.0, 2, 9, MeshingMode::Uniform, 0.0)
            .expect("procedural world");
        assert_eq!(w.tile_count(), 21);
        assert!(w
            .tiles()
            .iter()
            .all(|t| t.mesh.vertex_count() == 81 && t.geometric_error > 0.0));
        // The same SSE-LOD selector drives it: far up → the coarse root only; at the surface → refines.
        assert_eq!(
            w.select([0.0, 0.0, 5_000_000.0], 1.0, 1080.0, 16.0),
            vec![TileId::new(0, 0, 0)]
        );
        let near = w.select([0.0, 0.0, 10.0], 1.0, 1080.0, 1.0);
        assert!(
            near.len() >= 4 && near.iter().all(|t| t.level <= 2),
            "near refines: {near:?}"
        );
    }

    #[test]
    fn cull_distance_includes_near_excludes_far() {
        let c = [1000.0, 0.0, 0.0];
        let r = 100.0;
        assert!(within_cull_distance(c, r, [0.0; 3], 0.0), "max<=0 keeps all");
        assert!(within_cull_distance(c, r, [0.0; 3], 1000.0), "900 <= 1000");
        assert!(!within_cull_distance(c, r, [0.0; 3], 500.0), "900 > 500");
        assert!(within_cull_distance(c, r, [950.0, 0.0, 0.0], 10.0), "near");
    }
}
