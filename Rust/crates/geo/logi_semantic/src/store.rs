//! The baked **tile store**: a `manifest.json` (provenance / origin / tiling / tile index) plus the
//! per-tile `.lst` files the offline `Tools/logi_bake` emits. [`BakedStore`] opens + reads it;
//! [`BakedTileSource`] adapts it to the streaming kernel's [`TileContentSource`] fetch contract (a
//! deterministic, no-network local source — the actorless selection/cache/scheduler reuse unchanged).

use crate::format::SemanticTile;
use crate::{SemanticError, TileId};
use logi_3dtiles_io::{Loaded, RequestId, TileContentSource};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::PathBuf;

/// Current manifest schema version (refuse an unknown major rather than misread).
pub const MANIFEST_FORMAT_VERSION: u32 = 1;

/// The AOI origin: the ENU frame origin (and its WGS84 ECEF), from which every tile's metric offsets are
/// measured. The runtime builds the `EnuFrame` from this to place tiles on the globe.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ManifestOrigin {
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub height_m: f64,
    pub ecef: [f64; 3],
}

/// The local quadtree shape.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ManifestTiling {
    pub root_size_m: f64,
    pub leaf_level: u8,
    pub grid_n: u32,
}

/// One tile's index entry. `sha256` is the bake's integrity/golden hash (the runtime relies on the
/// in-file CRC-32 instead, so this stays a Python-side gate and is optional here).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ManifestTile {
    pub level: u8,
    pub x: u32,
    pub y: u32,
    pub file: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub height_min: f32,
    #[serde(default)]
    pub height_max: f32,
    /// Geometric error (metres) for SSE-LOD selection (= root relief · 0.5^level). 0 on a single-level store.
    #[serde(default)]
    pub geometric_error: f32,
}

/// The store manifest (subset this crate needs; unknown fields are ignored).
#[derive(Clone, Debug, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    #[serde(default)]
    pub bake_tool: String,
    #[serde(default)]
    pub seed: u64,
    pub origin: ManifestOrigin,
    pub tiling: ManifestTiling,
    pub tiles: Vec<ManifestTile>,
}

/// An opened baked tile store rooted at a directory containing `manifest.json` + the `.lst` tiles.
pub struct BakedStore {
    dir: PathBuf,
    manifest: Manifest,
}

impl BakedStore {
    /// Open a store: read + parse `manifest.json`, validating the schema version.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, SemanticError> {
        let dir = dir.into();
        let text = std::fs::read_to_string(dir.join("manifest.json")).map_err(|e| SemanticError::Io(e.to_string()))?;
        let manifest: Manifest = serde_json::from_str(&text).map_err(|e| SemanticError::Manifest(e.to_string()))?;
        if manifest.format_version != MANIFEST_FORMAT_VERSION {
            return Err(SemanticError::Version(manifest.format_version));
        }
        Ok(Self { dir, manifest })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// All tile ids in the store (manifest order).
    pub fn tile_ids(&self) -> Vec<TileId> {
        self.manifest
            .tiles
            .iter()
            .map(|t| TileId::new(t.level, t.x, t.y))
            .collect()
    }

    /// Read + decode one tile by id (validating its CRC-32). This does a linear scan of the manifest to
    /// find the entry; when reading **every** tile, iterate [`Manifest::tiles`] and call
    /// [`Self::read_tile_entry`] instead (avoids the O(n²) repeated scan — see `BakedWorld::open_with`).
    pub fn read_tile(&self, id: TileId) -> Result<SemanticTile, SemanticError> {
        let entry = self
            .manifest
            .tiles
            .iter()
            .find(|t| t.level == id.level && t.x == id.x && t.y == id.y)
            .ok_or(SemanticError::TileNotFound(id))?;
        self.read_tile_entry(entry)
    }

    /// Read + decode the tile named by a manifest `entry` (validating its CRC-32). O(1) — no manifest scan.
    pub fn read_tile_entry(&self, entry: &ManifestTile) -> Result<SemanticTile, SemanticError> {
        let bytes = std::fs::read(self.dir.join(&entry.file)).map_err(|e| SemanticError::Io(e.to_string()))?;
        SemanticTile::decode(&bytes)
    }
}

/// A [`TileContentSource`] over a local baked store. **Deliberately blocking-but-local:** unlike the
/// off-thread HTTP source, `request` reads the tile file (relative `url`) **synchronously on the owner
/// thread** and queues it; `poll_completed` then drains in deterministic id order. This is intentional —
/// a baked store is on local disk (a read is microseconds) and the whole point of this lane is
/// byte-for-byte deterministic replay, which a worker pool's nondeterministic completion order would
/// break. Do not "fix" it to fetch off-thread without an ADR; for a remote store use the HTTP source.
///
/// A missing/unreadable file yields empty bytes (the owner treats empty as a failed load — no panic),
/// matching the HTTP source's contract; the underlying IO error is logged before being dropped.
pub struct BakedTileSource {
    dir: PathBuf,
    ready: Vec<Loaded>,
    in_flight: HashSet<RequestId>,
}

impl BakedTileSource {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            ready: Vec::new(),
            in_flight: HashSet::new(),
        }
    }
}

impl TileContentSource for BakedTileSource {
    fn request(&mut self, id: RequestId, url: &str, _now_tick: u64) {
        // Synchronous local read — see the type doc (deterministic local lane, blocking by design).
        let path = self.dir.join(url);
        let bytes = std::fs::read(&path).unwrap_or_else(|e| {
            // Surface the cause before degrading to an empty (failed) load, so a bad bake/permission isn't
            // silently swallowed (the owner only sees "empty bytes").
            tracing::warn!(request = id, path = %path.display(), error = %e, "baked tile read failed");
            Vec::new()
        });
        self.in_flight.insert(id);
        self.ready.push(Loaded { id, bytes });
    }

    fn poll_completed(&mut self, _now_tick: u64, out: &mut Vec<Loaded>) {
        self.ready.sort_by_key(|l| l.id); // deterministic completion order
        for loaded in self.ready.drain(..) {
            if self.in_flight.remove(&loaded.id) {
                out.push(loaded);
            }
        }
    }

    fn cancel(&mut self, id: RequestId) {
        self.in_flight.remove(&id);
        self.ready.retain(|l| l.id != id);
    }

    fn in_flight(&self) -> usize {
        self.in_flight.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TileId;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static CTR: AtomicU64 = AtomicU64::new(0);

    fn tmp_dir() -> PathBuf {
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("logi_sem_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_tile(dir: &Path, t: &SemanticTile) {
        std::fs::write(dir.join(t.id.file_name()), t.encode()).unwrap();
    }

    #[test]
    fn store_open_lists_and_reads_tiles() {
        let dir = tmp_dir();
        let id = TileId::new(1, 0, 1);
        let t = SemanticTile::new(id, 3, -50.0, -50.0, 32.0, vec![1.0; 9]).unwrap();
        write_tile(&dir, &t);
        let manifest = format!(
            r#"{{"format_version":1,"bake_tool":"test","seed":7,
                 "origin":{{"lat_deg":39.6,"lon_deg":-75.6,"height_m":10.0,"ecef":[1.0,2.0,3.0]}},
                 "tiling":{{"root_size_m":100.0,"leaf_level":1,"grid_n":3}},
                 "tiles":[{{"level":1,"x":0,"y":1,"file":"{}"}}]}}"#,
            t.id.file_name()
        );
        std::fs::write(dir.join("manifest.json"), manifest).unwrap();

        let store = BakedStore::open(&dir).unwrap();
        assert_eq!(store.manifest().seed, 7);
        assert_eq!(store.tile_ids(), vec![id]);
        assert_eq!(store.read_tile(id).unwrap(), t);
        assert!(matches!(
            store.read_tile(TileId::new(1, 1, 1)),
            Err(SemanticError::TileNotFound(_))
        ));
    }

    #[test]
    fn store_rejects_unknown_format_version() {
        let dir = tmp_dir();
        std::fs::write(
            dir.join("manifest.json"),
            r#"{"format_version":99,"origin":{"lat_deg":0,"lon_deg":0,"height_m":0,"ecef":[0,0,0]},
               "tiling":{"root_size_m":1,"leaf_level":0,"grid_n":2},"tiles":[]}"#,
        )
        .unwrap();
        assert_eq!(BakedStore::open(&dir).err(), Some(SemanticError::Version(99)));
    }

    #[test]
    fn baked_source_reads_and_polls_deterministically() {
        let dir = tmp_dir();
        let id = TileId::new(0, 0, 0);
        let t = SemanticTile::new(id, 2, 0.0, 0.0, 10.0, vec![0.0; 4]).unwrap();
        write_tile(&dir, &t);

        let mut src = BakedTileSource::new(&dir);
        src.request(42, &id.file_name(), 0);
        assert_eq!(src.in_flight(), 1);
        let mut out = Vec::new();
        src.poll_completed(0, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, 42);
        assert_eq!(SemanticTile::decode(&out[0].bytes).unwrap(), t);
        assert_eq!(src.in_flight(), 0);

        // A missing file is an empty (failed) load, not a panic.
        src.request(7, "does_not_exist.lst", 0);
        let mut out2 = Vec::new();
        src.poll_completed(0, &mut out2);
        assert_eq!(out2.len(), 1);
        assert!(out2[0].bytes.is_empty());
    }
}
