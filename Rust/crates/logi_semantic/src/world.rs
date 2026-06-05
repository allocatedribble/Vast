//! The HE-0 baked-tile **runtime world**: open a store, triangulate every tile into Unreal render space
//! once, and serve the render set. The AOI is small (a 1-mile radius ⇒ at most a few hundred leaves), so
//! HE-0 loads the whole AOI eagerly and serves all tiles; camera-driven SSE selection over the shared
//! kernel machinery (`logi_terrain_lod` / cache / scheduler) is a later-HE refinement. Deterministic; no
//! `unsafe`.

use crate::mesh::{triangulate, MeshingMode, TerrainMesh};
use crate::store::BakedStore;
use crate::{SemanticError, TileId};
use std::path::PathBuf;

/// One resident tile ready to render: its id + mesh in **Unreal render space** (cm, Z-up LH).
#[derive(Clone, Debug)]
pub struct BakedRenderTile {
    pub id: TileId,
    pub mesh: TerrainMesh,
}

/// All baked tiles of an AOI, triangulated to Unreal render space. The ENU origin is pinned at Unreal
/// `(0,0,0)`; `origin_ecef` is that origin's WGS84 ECEF (for globe / sky-atmosphere placement).
pub struct BakedWorld {
    origin_ecef: [f64; 3],
    tiles: Vec<BakedRenderTile>,
}

impl BakedWorld {
    /// Open a store directory (`manifest.json` + `tiles/`) and build every tile's render mesh with the
    /// default (uniform) meshing.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, SemanticError> {
        Self::open_with(dir, MeshingMode::Uniform)
    }

    /// Open a store and build every tile's render mesh with the given [`MeshingMode`] — the meshing
    /// toggle (uniform grid, or the adaptive TIN).
    pub fn open_with(dir: impl Into<PathBuf>, mode: MeshingMode) -> Result<Self, SemanticError> {
        let store = BakedStore::open(dir)?;
        let origin_ecef = store.manifest().origin.ecef;
        let mut tiles = Vec::with_capacity(store.manifest().tiles.len());
        for id in store.tile_ids() {
            let tile = store.read_tile(id)?;
            let mesh = triangulate(&tile, mode).to_ue_cm();
            tiles.push(BakedRenderTile { id, mesh });
        }
        Ok(Self { origin_ecef, tiles })
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
}
