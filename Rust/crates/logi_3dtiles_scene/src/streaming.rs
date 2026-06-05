//! The streaming **3D Tiles world** (ADR-0009, Milestone F). Drives the SSE traversal over a tileset that
//! GROWS as it descends — external `.json` tilesets are fetched + grafted into the tree — fetches + decodes
//! glTF tiles on a worker pool, and rebases each tile's ECEF geometry into an **ENU-local** frame at a fixed
//! origin for the renderer. Validated live against Google Photorealistic 3D Tiles.
//!
//! `step()` returns a lightweight render list (paths + bounds); the renderer queries [`TilesetWorld::tile_mesh`]
//! for each tile's geometry/texture **zero-copy** (no per-frame geometry cloning). The C++ side caches a
//! section per tile and only builds new ones (the terrain-lane pattern).

use glam::{DMat4, DVec3};
use logi_3dtiles_io::{DiskCache, HttpSource, RequestId, TileContentSource};
use logi_3dtiles_select::{select_tree, SelectParams, SelectedTile};
use logi_3dtiles_tileset::{parse_tileset, Tile};
use logi_geospatial::{Cartographic, Ellipsoid, EnuFrame};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

type TilePath = Vec<u32>;

#[derive(Clone, Debug)]
pub enum StreamError {
    Tileset(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContentKind {
    Tileset, // a nested `.json` external tileset → fetch + graft into the tree
    Glb,     // a renderable glTF tile → fetch + decode
}

/// A decoded tile mesh in **Unreal render space** (cm, Z-up LH), held by the world and queried zero-copy
/// via [`TilesetWorld::tile_mesh`]. The ECEF geometry is rebased to ENU-local at the world origin, then
/// converted to Unreal space (negate North), so the C++ side uploads it with no coordinate math.
struct DecodedTile {
    vertices: Vec<u8>, // 32-byte: pos f3 (Unreal cm) | normal f3 (Unreal) | uv f2
    indices: Vec<u32>,
    texture: Vec<u8>, // embedded image bytes (JPEG/PNG), or empty
    bounds_center: [f32; 3],
    bounds_radius: f32,
    /// The last frame this tile was in the render set — drives LRU eviction of the off-screen cache.
    last_seen: u64,
}

/// One frame's render list. Tile geometry is in **Unreal render space** (cm) with the ENU origin pinned
/// at Unreal `(0,0,0)`; `origin_ecef` is that origin's WGS84 ECEF (metres), for globe/atmosphere placement.
pub struct GlobeFrame {
    pub origin_ecef: [f64; 3],
    pub tiles: Vec<RenderTileInfo>,
}

/// A tile to draw this frame — geometry fetched separately via [`TilesetWorld::tile_mesh`]. Bounds are in
/// Unreal render space (cm).
pub struct RenderTileInfo {
    pub path: Vec<u32>,
    pub bounds_center: [f32; 3],
    pub bounds_radius: f32,
    pub vertex_count: u32,
    pub index_count: u32,
}

/// Zero-copy view of a tile's geometry (Unreal cm) + texture (valid until the next [`TilesetWorld::step`]).
pub struct TileMesh<'a> {
    pub vertices: &'a [u8],
    pub indices: &'a [u32],
    pub texture: &'a [u8],
}

/// A streaming 3D-Tiles world rooted at a fetched root tileset, rebased to a fixed ENU origin.
pub struct TilesetWorld {
    base_url: String,
    ellipsoid: Ellipsoid,
    enu: EnuFrame,
    root: Tile,
    source: HttpSource,
    decoded: HashMap<TilePath, DecodedTile>,
    grafted: HashSet<TilePath>,
    /// Per-grafted-subtree base URL: a tile's content URI resolves against the URL of the tileset JSON
    /// that *contains* it (the 3D Tiles external-tileset rule). Google P3DT relies on this — descendant
    /// content URIs omit the `?session=…` and inherit it from their parent tileset's fetch URL, so
    /// resolving everything against the root `base_url` (key only, no session) yields HTTP 400. Keyed by
    /// the grafted subtree-root path; the root tile (`[]`) uses `base_url`.
    bases: HashMap<TilePath, String>,
    in_flight: HashMap<RequestId, (TilePath, ContentKind, DMat4, String)>,
    next_id: RequestId,
    max_in_flight: usize,
    /// Spatial cull radius (metres): tiles farther than this from the camera are not traversed, bounding
    /// streaming to the camera's neighbourhood (stands in for full frustum culling in v1).
    max_tile_distance: f64,
    /// Monotonic step counter; stamps each decoded tile's `last_seen` for LRU eviction.
    frame_counter: u64,
    /// Resident decoded-tile cap. The on-screen working set is NEVER evicted (only off-screen tiles past
    /// this bound are dropped, oldest-first), so this bounds the cache slack, not the live frame.
    max_decoded: usize,
}

impl TilesetWorld {
    /// Build a world from the root tileset bytes + its base URL (carrying auth), centred on `origin` (the
    /// ENU rebase frame). `workers` fetch threads; `max_tile_distance` metres caps the streamed extent.
    pub fn new(
        base_url: String,
        root_tileset_bytes: &[u8],
        origin: Cartographic,
        workers: usize,
        max_tile_distance: f64,
        cache: Option<Arc<DiskCache>>,
    ) -> Result<Self, StreamError> {
        let tileset =
            parse_tileset(root_tileset_bytes).map_err(|e| StreamError::Tileset(e.to_string()))?;
        let ellipsoid = Ellipsoid::wgs84();
        let enu = EnuFrame::from_cartographic(&ellipsoid, origin);
        Ok(Self {
            base_url,
            ellipsoid,
            enu,
            root: tileset.root,
            source: HttpSource::new(workers, cache),
            decoded: HashMap::new(),
            grafted: HashSet::new(),
            bases: HashMap::new(),
            in_flight: HashMap::new(),
            next_id: 1,
            max_in_flight: 64,
            max_tile_distance,
            frame_counter: 0,
            // ~4096 resident tiles of off-screen cache slack (the live working set is always retained on
            // top). Bounds the Rust-owned geometry/texture heap as the camera roams (mirrors the C++
            // section cap + the terrain MESH_CACHE_CAP). The whole point of pairing this with a lower SSE.
            max_decoded: 4096,
        })
    }

    /// Resolve a Cesium **ion** asset to its tileset URL, fetch the root tileset, and build a streaming
    /// world centred on `(longitude_deg, latitude_deg, height_m)`. Blocking network I/O (one endpoint
    /// resolve + one root fetch) — call off the render thread. The ion `token` is used only for the
    /// resolve and is **not** retained (per-tile auth lives in the resolved tileset URLs).
    #[allow(clippy::too_many_arguments)]
    pub fn from_ion(
        asset_id: u64,
        token: &str,
        longitude_deg: f64,
        latitude_deg: f64,
        height_m: f64,
        workers: usize,
        max_tile_distance: f64,
        cache: Option<Arc<DiskCache>>,
    ) -> Result<Self, StreamError> {
        let asset = logi_3dtiles_ion::resolve_asset(asset_id, token)
            .map_err(|e| StreamError::Tileset(format!("ion resolve: {e:?}")))?;
        let root_bytes = logi_3dtiles_io::http_get(&asset.tileset_url)
            .map_err(|e| StreamError::Tileset(format!("root fetch: {e:?}")))?;
        let origin = Cartographic::from_degrees(longitude_deg, latitude_deg, height_m);
        Self::new(asset.tileset_url, &root_bytes, origin, workers, max_tile_distance, cache)
    }

    /// Advance one step: drain completed fetches (decode glTF / graft tilesets), run the SSE + distance
    /// traversal, request new loads, and return the renderable frame (a render list).
    pub fn step(
        &mut self,
        camera_ecef: DVec3,
        fov_y_radians: f64,
        viewport_height: f64,
        max_sse: f64,
    ) -> GlobeFrame {
        self.frame_counter += 1;
        self.drain_completed();

        let resolved: HashSet<TilePath> = self
            .decoded
            .keys()
            .cloned()
            .chain(self.grafted.iter().cloned())
            .chain(self.in_flight.values().map(|(p, _, _, _)| p.clone()))
            .collect();

        let params = SelectParams {
            camera_position: camera_ecef,
            fov_y_radians,
            // Guard the SSE divide: a 0 viewport height yields SSE 0 → only the contentless root selected
            // (a blank globe). A degenerate/uninitialised editor viewport must not blank the stream.
            viewport_height: viewport_height.max(1.0),
            max_screen_space_error: max_sse,
        };
        let max_d = self.max_tile_distance;
        // `is_loaded` for render-fallback = has DECODED geometry (NOT merely in-flight or grafted), so the
        // traversal keeps descending past not-yet-decoded placeholders and the anti-pop fallback is always a
        // real mesh. Duplicate-request suppression uses the broader `resolved` set inside `request_loads`.
        let decoded = &self.decoded;
        let sel = select_tree(
            &self.root,
            &self.ellipsoid,
            &params,
            &mut |p| decoded.contains_key(p),
            &mut |center, radius| (camera_ecef - center).length() - radius < max_d,
        );

        self.request_loads(&sel.load, &resolved);

        let fc = self.frame_counter;
        let mut current: HashSet<TilePath> = HashSet::with_capacity(sel.render.len());
        let tiles: Vec<RenderTileInfo> = sel
            .render
            .iter()
            .filter_map(|rt| {
                self.decoded.get(&rt.path).map(|d| {
                    current.insert(rt.path.clone());
                    RenderTileInfo {
                        path: rt.path.clone(),
                        bounds_center: d.bounds_center,
                        bounds_radius: d.bounds_radius,
                        vertex_count: (d.vertices.len() / 32) as u32,
                        index_count: d.indices.len() as u32,
                    }
                })
            })
            .collect();

        // Stamp the live working set, then evict off-screen decoded tiles past the cache bound. The live
        // set (returned in `tiles`, queried zero-copy by the C++ side THIS frame) is never evicted; the
        // PRIOR frame was already consumed before this `step`, so freeing non-live buffers can't dangle.
        for p in &current {
            if let Some(d) = self.decoded.get_mut(p) {
                d.last_seen = fc;
            }
        }
        self.evict_decoded(&current);

        GlobeFrame { origin_ecef: self.enu.origin_ecef.to_array(), tiles }
    }

    /// Drop off-screen decoded tiles once the resident set exceeds `max_decoded`, oldest (`last_seen`)
    /// first. Never evicts a tile in `live` (this frame's render set) — that buffer is handed to the
    /// renderer zero-copy until the next `step`, so freeing it would be a use-after-free across the FFI.
    fn evict_decoded(&mut self, live: &HashSet<TilePath>) {
        if self.decoded.len() <= self.max_decoded {
            return;
        }
        let mut cands: Vec<(u64, TilePath)> = self
            .decoded
            .iter()
            .filter(|(p, _)| !live.contains(*p))
            .map(|(p, d)| (d.last_seen, p.clone()))
            .collect();
        cands.sort_unstable_by_key(|(ls, _)| *ls);
        let overflow = self.decoded.len() - self.max_decoded;
        for (_, p) in cands.into_iter().take(overflow) {
            self.decoded.remove(&p);
        }
    }

    /// [`step`](Self::step) driven by an **Unreal-space** camera position (cm), the same render frame the
    /// returned tile geometry is in, with the ENU origin pinned at Unreal `(0,0,0)`. Converts the camera
    /// to ECEF internally for the selection traversal.
    pub fn step_from_ue_camera(
        &mut self,
        camera_ue_cm: [f64; 3],
        fov_y_radians: f64,
        viewport_height: f64,
        max_sse: f64,
    ) -> GlobeFrame {
        let camera_ecef = self.enu.local_to_ecef(ue_cm_to_enu(camera_ue_cm));
        self.step(camera_ecef, fov_y_radians, viewport_height, max_sse)
    }

    /// Zero-copy geometry/texture for a rendered tile (valid until the next `step`).
    pub fn tile_mesh(&self, path: &[u32]) -> Option<TileMesh<'_>> {
        self.decoded.get(path).map(|d| TileMesh {
            vertices: &d.vertices,
            indices: &d.indices,
            texture: &d.texture,
        })
    }

    pub fn resident_tiles(&self) -> usize {
        self.decoded.len()
    }
    pub fn in_flight(&self) -> usize {
        self.source.in_flight()
    }
    /// Bytes resident in the persistent disk cache (0 if none configured).
    pub fn cached_bytes(&self) -> u64 {
        self.source.cached_bytes()
    }

    fn drain_completed(&mut self) {
        let mut done = Vec::new();
        self.source.poll_completed(0, &mut done);
        for loaded in done {
            let Some((path, kind, world, url)) = self.in_flight.remove(&loaded.id) else {
                continue;
            };
            if loaded.bytes.is_empty() {
                continue; // failed fetch
            }
            match kind {
                ContentKind::Tileset => {
                    self.graft(&path, &loaded.bytes);
                    // Descendants of this grafted subtree resolve their content URIs against the URL this
                    // external tileset was fetched from (carries the inherited `?session=…`).
                    self.bases.insert(path, url);
                }
                ContentKind::Glb => {
                    if let Ok(g) = logi_gltf::decode_glb(&loaded.bytes) {
                        if let Some(mut d) = self.rebase_decoded(g, world) {
                            d.last_seen = self.frame_counter; // freshly resident ⇒ seen this frame
                            self.decoded.insert(path, d);
                        }
                    }
                }
            }
        }
    }

    fn request_loads(&mut self, loads: &[SelectedTile], resolved: &HashSet<TilePath>) {
        for lt in loads {
            if self.source.in_flight() >= self.max_in_flight {
                break;
            }
            if resolved.contains(&lt.path) {
                continue;
            }
            let Some(uri) = lt.content_uris.first() else {
                continue;
            };
            // Classify on the PATH extension, not a substring of the whole URL: a `.glb` tile's signed
            // query (`?session=…&key=…`) can contain ".json" and would be mis-sent to the tileset parser.
            let kind = if is_tileset_uri(uri) { ContentKind::Tileset } else { ContentKind::Glb };
            let base = self.base_for(&lt.path).to_string();
            let Ok(full) = logi_3dtiles_io::resolve_url(&base, uri) else {
                continue;
            };
            let id = self.next_id;
            self.next_id += 1;
            self.source.request(id, &full, 0);
            self.in_flight.insert(id, (lt.path.clone(), kind, lt.world_transform, full));
        }
    }

    /// The base URL a tile's content URI resolves against: the nearest grafted ancestor's tileset URL
    /// (the longest path-prefix recorded in `bases`), or the root `base_url` if none. This propagates the
    /// inherited `?session=…` down each external-tileset subtree (Google P3DT) and keeps path-relative
    /// nested-tileset URIs resolving against their own directory.
    fn base_for(&self, path: &[u32]) -> &str {
        let mut best: &str = &self.base_url;
        let mut best_len = 0usize;
        for (p, url) in &self.bases {
            if p.len() > best_len && p.len() <= path.len() && path[..p.len()] == p[..] {
                best = url;
                best_len = p.len();
            }
        }
        best
    }

    /// Graft a fetched external tileset's root into the placeholder tile at `path`: its content + children
    /// replace the placeholder's, and its transform composes into the placeholder's so the grafted subtree
    /// lands in the right ECEF frame. The next traversal then descends into it.
    fn graft(&mut self, path: &[u32], json_bytes: &[u8]) {
        if let Ok(child) = parse_tileset(json_bytes) {
            let ext = child.root;
            if let Some(tile) = tile_at_mut(&mut self.root, path) {
                let composed = tile.matrix() * ext.matrix();
                tile.transform = Some(composed.to_cols_array());
                tile.content = ext.content;
                tile.contents = ext.contents;
                tile.children = ext.children;
            }
            self.grafted.insert(path.to_vec());
        }
    }

    /// Combine a decoded glTF's primitives into one mesh, rebasing each vertex tile-local → ECEF → ENU-local
    /// at the fixed origin → Unreal render space; take the first non-empty texture. The effective transform
    /// per primitive is `world * node_transform`: `world` is the composed tileset tile transform, and
    /// `node_transform` is the glTF node's world matrix that Google P3DT bakes the tile's ECEF placement
    /// into — omitting it collapses every tile to the geocenter (~6371 km off).
    fn rebase_decoded(&self, g: logi_gltf::DecodedGltf, world: DMat4) -> Option<DecodedTile> {
        let mut vertices: Vec<u8> = Vec::new();
        let mut indices: Vec<u32> = Vec::new();
        let mut base = 0u32;
        let mut mn = [f32::MAX; 3];
        let mut mx = [f32::MIN; 3];
        // glTF is Y-up; 3D Tiles tile space (and our WGS84 ECEF) is Z-up. The +90°-about-X rotation
        // converts the tile's glTF-Y-up-ECEF placement (baked into the node transform by P3DT) into
        // standard Z-up ECEF: (x,y,z) → (x,-z,y). Omitting it swaps Y/Z and lands the tile ~Earth-diameter
        // off. Chain: ecef = tile_world · yup2zup · node · vertex.
        let yup2zup = DMat4::from_rotation_x(std::f64::consts::FRAC_PI_2);
        for prim in &g.primitives {
            // Compose the tile transform, the Y-up→Z-up fix, and the primitive's glTF node transform (the
            // ECEF placement), in f64, before the ECEF→local rebase.
            let node = glam::Mat4::from_cols_array_2d(&prim.node_transform).as_dmat4();
            let m = world * yup2zup * node;
            let vcount = prim.vertices.len() / 32;
            vertices.reserve(vcount * 32);
            for v in 0..vcount {
                let o = v * 32;
                let (px, py, pz) = read3(&prim.vertices, o);
                let (nx, ny, nz) = read3(&prim.vertices, o + 12);
                let u = rdf(&prim.vertices, o + 24);
                let vv = rdf(&prim.vertices, o + 28);
                let p_ecef = m.transform_point3(DVec3::new(px as f64, py as f64, pz as f64));
                let p_enu = self.enu.ecef_to_local(p_ecef);
                let n_ecef = m.transform_vector3(DVec3::new(nx as f64, ny as f64, nz as f64));
                let n_enu = (self.enu.ecef_to_enu * n_ecef).normalize_or_zero();
                // ENU metres (RH) → Unreal render space (cm, LH, Z-up): negate North. See `enu_pos_to_ue`.
                let pe = enu_pos_to_ue(p_enu);
                let ne = enu_dir_to_ue(n_enu);
                write3(&mut vertices, pe);
                write3(&mut vertices, ne);
                push_f32(&mut vertices, u);
                push_f32(&mut vertices, vv);
                for k in 0..3 {
                    mn[k] = mn[k].min(pe[k]);
                    mx[k] = mx[k].max(pe[k]);
                }
            }
            // Rebase indices into the combined buffer; skip on overflow rather than wrapping (hostile input).
            for &i in &prim.indices {
                if let Some(idx) = i.checked_add(base) {
                    indices.push(idx);
                }
            }
            base = base.saturating_add(vcount as u32);
        }
        if vertices.is_empty() {
            return None;
        }
        let texture =
            g.images.into_iter().map(|im| im.data).find(|d| !d.is_empty()).unwrap_or_default();
        let bounds_center = [(mn[0] + mx[0]) * 0.5, (mn[1] + mx[1]) * 0.5, (mn[2] + mx[2]) * 0.5];
        let bounds_radius =
            0.5 * ((mx[0] - mn[0]).powi(2) + (mx[1] - mn[1]).powi(2) + (mx[2] - mn[2]).powi(2)).sqrt();
        Some(DecodedTile { vertices, indices, texture, bounds_center, bounds_radius, last_seen: 0 })
    }
}

// ---- helpers --------------------------------------------------------------------------------------

/// True if a content URI points at an external `.json` tileset (vs a `.glb`/`.b3dm` geometry tile),
/// judged by the PATH extension after stripping the query/fragment — so a signed tile URL whose token
/// happens to contain ".json" isn't misclassified.
fn is_tileset_uri(uri: &str) -> bool {
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    path.rsplit('.').next().is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

fn tile_at_mut<'a>(root: &'a mut Tile, path: &[u32]) -> Option<&'a mut Tile> {
    let mut t = root;
    for &i in path {
        t = t.children.get_mut(i as usize)?;
    }
    Some(t)
}

#[inline]
fn rdf(b: &[u8], o: usize) -> f32 {
    f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
#[inline]
fn read3(b: &[u8], o: usize) -> (f32, f32, f32) {
    (rdf(b, o), rdf(b, o + 4), rdf(b, o + 8))
}
#[inline]
fn push_f32(b: &mut Vec<u8>, v: f32) {
    b.extend_from_slice(&v.to_le_bytes());
}
#[inline]
fn write3(b: &mut Vec<u8>, v: [f32; 3]) {
    for f in v {
        b.extend_from_slice(&f.to_le_bytes());
    }
}

/// ENU metres (East, North, Up; right-handed Z-up) → Unreal render space (cm; **left-handed** Z-up).
/// Negating North (the Y axis) flips RH→LH — the same reflection the M1 building lane uses — and it also
/// flips triangle winding into Unreal's front-face convention, so tile indices are passed through
/// unchanged (reversing them would double-flip and back-face-cull the whole tile).
#[inline]
fn enu_pos_to_ue(p: DVec3) -> [f32; 3] {
    [(p.x * 100.0) as f32, (-p.y * 100.0) as f32, (p.z * 100.0) as f32]
}
/// Direction counterpart of [`enu_pos_to_ue`] (no scale): negate North only.
#[inline]
fn enu_dir_to_ue(d: DVec3) -> [f32; 3] {
    [d.x as f32, -d.y as f32, d.z as f32]
}
/// Unreal render space (cm) → ENU metres — the inverse of [`enu_pos_to_ue`], for the camera position.
#[inline]
fn ue_cm_to_enu(p: [f64; 3]) -> DVec3 {
    DVec3::new(p[0] / 100.0, -p[1] / 100.0, p[2] / 100.0)
}
