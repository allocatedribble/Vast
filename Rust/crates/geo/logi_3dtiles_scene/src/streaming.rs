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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    ///
    /// This is the **only non-blocking, kernel-reachable** constructor: it does no network I/O — the caller
    /// supplies the already-fetched root bytes — so it is safe to call from the streaming kernel / owner
    /// thread (audit H10). All subsequent tile/external-tileset fetches happen on the [`HttpSource`] worker
    /// pool and drain through [`step`](Self::step), never blocking the owner. The blocking [`from_ion`]
    /// convenience constructor must run off-thread (tools / setup only).
    pub fn new(
        base_url: String,
        root_tileset_bytes: &[u8],
        origin: Cartographic,
        workers: usize,
        max_tile_distance: f64,
        cache: Option<Arc<DiskCache>>,
    ) -> Result<Self, StreamError> {
        let tileset = parse_tileset(root_tileset_bytes).map_err(|e| StreamError::Tileset(e.to_string()))?;
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
    /// world centred on `(longitude_deg, latitude_deg, height_m)`.
    ///
    /// **Off-thread / tools-only — NOT kernel-reachable.** This does two **blocking** round-trips (the ion
    /// endpoint resolve + the root-tileset fetch) before delegating to the non-blocking [`new`](Self::new);
    /// even with the shared agent's timeouts (audit H9) a slow endpoint stalls the calling thread for up to
    /// the overall timeout, so it must never run on the render/owner thread (audit H10) — call it from setup
    /// code or a worker and hand the resulting world to the kernel. Both fetches use the shared
    /// timeout-bounded HTTP agent. The ion `token` is used only for the resolve and is **not** retained
    /// (per-tile auth lives in the resolved tileset URLs).
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
        Self::new(
            asset.tileset_url,
            &root_bytes,
            origin,
            workers,
            max_tile_distance,
            cache,
        )
    }

    /// Advance one step: drain completed fetches (decode glTF / graft tilesets), run the SSE + distance
    /// traversal, request new loads, and return the renderable frame (a render list).
    pub fn step(&mut self, camera_ecef: DVec3, fov_y_radians: f64, viewport_height: f64, max_sse: f64) -> GlobeFrame {
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

        GlobeFrame {
            origin_ecef: self.enu.origin_ecef.to_array(),
            tiles,
        }
    }

    /// Drop off-screen decoded tiles once the resident set exceeds `max_decoded`, oldest (`last_seen`)
    /// first. Never evicts a tile in `live` (this frame's render set) — that buffer is handed to the
    /// renderer zero-copy until the next `step`, so freeing it would be a use-after-free across the FFI.
    fn evict_decoded(&mut self, live: &HashSet<TilePath>) {
        if self.decoded.len() <= self.max_decoded {
            return;
        }
        let overflow = self.decoded.len() - self.max_decoded;
        // Borrow `(last_seen, &path)` for every evictable (off-screen) tile — no path is cloned yet (audit
        // M15: the old code cloned ALL candidate paths and full-sorted them, often to drop a single tile).
        let mut cands: Vec<(u64, &TilePath)> = self
            .decoded
            .iter()
            .filter(|(p, _)| !live.contains(*p))
            .map(|(p, d)| (d.last_seen, p))
            .collect();
        // Partition so the `overflow` oldest are in `cands[..overflow]` — O(n) average, no full sort, and
        // it touches only the recency keys. Then clone just those paths (≤ overflow), drop the borrow, and
        // remove. `take(overflow)` is safe whether or not `overflow` exceeds the candidate count.
        let n = overflow.min(cands.len());
        if n < cands.len() {
            cands.select_nth_unstable_by_key(n, |(ls, _)| *ls);
        }
        let to_remove: Vec<TilePath> = cands.into_iter().take(n).map(|(_, p)| p.clone()).collect();
        for p in to_remove {
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
            let Some((path, uri_hint, world, url)) = self.in_flight.remove(&loaded.id) else {
                continue;
            };
            if loaded.bytes.is_empty() {
                continue; // failed fetch
            }
            // Decide tileset-vs-glTF from the ACTUAL fetched bytes, not the request-time URI extension
            // (audit M18): extensionless / query-only tileset endpoints are common and the URI hint
            // mis-sends them to the glTF decoder, dropping the subtree. The URI hint only breaks ties when
            // the content is unrecognisable (e.g. an empty-but-nonzero body).
            let kind = sniff_content_kind(&loaded.bytes).unwrap_or(uri_hint);
            match kind {
                ContentKind::Tileset => {
                    // Only record the subtree base URL if the graft actually attached (audit M17). The old
                    // code inserted the base unconditionally even when the external tileset failed to parse,
                    // so the subtree silently never attached and — because `resolved`/`bases` then claimed it
                    // was handled — was never retried. On a parse failure we drop the placeholder's claim so
                    // a later traversal can re-request it.
                    match self.graft(&path, &loaded.bytes) {
                        Ok(()) => {
                            // Descendants of this grafted subtree resolve their content URIs against the URL
                            // this external tileset was fetched from (carries the inherited `?session=…`).
                            self.bases.insert(path, url);
                        }
                        Err(e) => {
                            // Surface the failure; the placeholder stays un-grafted and eligible for retry.
                            tracing::warn!(path = ?path, error = ?e, "external tileset graft failed");
                        }
                    }
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
            let kind = if is_tileset_uri(uri) {
                ContentKind::Tileset
            } else {
                ContentKind::Glb
            };
            let base = self.base_for(&lt.path).to_string();
            let Ok(full) = logi_3dtiles_io::resolve_url(&base, uri) else {
                continue;
            };
            let id = self.next_id;
            self.next_id += 1;
            self.source.request(id, &full, 0);
            self.in_flight
                .insert(id, (lt.path.clone(), kind, lt.world_transform, full));
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
    /// lands in the right ECEF frame. The next traversal then descends into it. Returns an error if the
    /// external tileset bytes don't parse or the placeholder path no longer resolves, so the caller can
    /// surface it and leave the subtree eligible for retry instead of silently dropping it (audit M17).
    fn graft(&mut self, path: &[u32], json_bytes: &[u8]) -> Result<(), StreamError> {
        let child = parse_tileset(json_bytes).map_err(|e| StreamError::Tileset(e.to_string()))?;
        let ext = child.root;
        let tile = tile_at_mut(&mut self.root, path)
            .ok_or_else(|| StreamError::Tileset(format!("graft target path {path:?} no longer exists")))?;
        let composed = tile.matrix() * ext.matrix();
        tile.transform = Some(composed.to_cols_array());
        tile.content = ext.content;
        tile.contents = ext.contents;
        tile.children = ext.children;
        self.grafted.insert(path.to_vec());
        Ok(())
    }

    /// Combine a decoded glTF's primitives into one mesh, rebasing each vertex tile-local → ECEF → ENU-local
    /// at the fixed origin → Unreal render space; take the first non-empty texture. The effective transform
    /// per primitive is `world * node_transform`: `world` is the composed tileset tile transform, and
    /// `node_transform` is the glTF node's world matrix that Google P3DT bakes the tile's ECEF placement
    /// into — omitting it collapses every tile to the geocenter (~6371 km off).
    fn rebase_decoded(&self, g: logi_gltf::DecodedGltf, world: DMat4) -> Option<DecodedTile> {
        // Accumulate transformed vertices as f32 (8 per vertex), cast to bytes once at the end — avoids the
        // per-float byte-by-byte read/write of the old path (audit M12).
        let mut verts_f32: Vec<f32> = Vec::new();
        let mut indices: Vec<u32> = Vec::new();
        let mut base = 0u32;
        // ±INFINITY sentinels (not f32::MAX/MIN): a degenerate/empty/all-NaN mesh then yields a non-finite
        // center/radius that the finiteness guard below catches, instead of a ~f32::MAX radius silently
        // driving culling/placement (audit M14).
        let mut mn = [f32::INFINITY; 3];
        let mut mx = [f32::NEG_INFINITY; 3];
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
            // Read each 32-byte vertex as a whole [f32; 8] in one `bytemuck` call instead of eight separate
            // `f32::from_le_bytes` reads (audit M12). `pod_read_unaligned` (not `cast_slice`) because the
            // glTF-decoded `Vec<u8>` has alignment 1 — a `&[u8]`→`&[f32]` cast would panic on misalignment,
            // and a decoder must never panic on bad bytes. `chunks_exact(32)` drops any non-multiple-of-32
            // tail (corrupt buffer) without panicking. Target is little-endian (x86_64), matching the LE
            // wire layout the interleave writers produce.
            let mut vcount = 0u32;
            verts_f32.reserve(prim.vertices.len() / 4);
            for chunk in prim.vertices.chunks_exact(32) {
                let vtx: [f32; 8] = bytemuck::pod_read_unaligned(chunk);
                let p_ecef = m.transform_point3(DVec3::new(vtx[0] as f64, vtx[1] as f64, vtx[2] as f64));
                let p_enu = self.enu.ecef_to_local(p_ecef);
                let n_ecef = m.transform_vector3(DVec3::new(vtx[3] as f64, vtx[4] as f64, vtx[5] as f64));
                let n_enu = (self.enu.ecef_to_enu * n_ecef).normalize_or_zero();
                // ENU metres (RH) → Unreal render space (cm, LH, Z-up): negate North. See `enu_pos_to_ue`.
                let pe = enu_pos_to_ue(p_enu);
                let ne = enu_dir_to_ue(n_enu);
                verts_f32.extend_from_slice(&[pe[0], pe[1], pe[2], ne[0], ne[1], ne[2], vtx[6], vtx[7]]);
                for k in 0..3 {
                    mn[k] = mn[k].min(pe[k]);
                    mx[k] = mx[k].max(pe[k]);
                }
                vcount += 1;
            }
            // Rebase indices into the combined buffer; skip on overflow rather than wrapping (hostile input).
            for &i in &prim.indices {
                if let Some(idx) = i.checked_add(base) {
                    indices.push(idx);
                }
            }
            base = base.saturating_add(vcount);
        }
        if verts_f32.is_empty() {
            return None;
        }
        let texture = g
            .images
            .into_iter()
            .map(|im| im.data)
            .find(|d| !d.is_empty())
            .unwrap_or_default();
        let (bounds_center, bounds_radius) = aabb_to_sphere(mn, mx);
        // Reject a tile whose bounds aren't finite (all-NaN / degenerate geometry): a non-finite center or
        // radius would poison frustum culling and placement downstream (audit M14). With ±INFINITY
        // sentinels, a mesh with ≥1 finite vertex yields finite bounds; only genuinely degenerate input
        // (every component NaN/inf) fails here.
        if !bounds_center.iter().all(|c| c.is_finite()) || !bounds_radius.is_finite() {
            return None;
        }
        // One cast from the f32 accumulator to the byte buffer the renderer uploads (M12).
        let vertices: Vec<u8> = bytemuck::cast_slice(&verts_f32).to_vec();
        Some(DecodedTile {
            vertices,
            indices,
            texture,
            bounds_center,
            bounds_radius,
            last_seen: 0,
        })
    }
}

// ---- helpers --------------------------------------------------------------------------------------

/// Best-effort *hint* that a content URI points at an external `.json` tileset (vs a `.glb`/`.b3dm`
/// geometry tile), judged by the PATH extension after stripping the query/fragment — so a signed tile URL
/// whose token happens to contain ".json" isn't misclassified. This is only a request-time hint: the
/// authoritative classification sniffs the fetched bytes in `drain_completed` (see [`sniff_content_kind`]),
/// because extensionless / query-only tileset endpoints carry no usable extension (audit M18).
fn is_tileset_uri(uri: &str) -> bool {
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    path.rsplit('.')
        .next()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

/// Classify fetched tile content by sniffing its leading bytes (audit M18): a `glTF` magic ⇒ a GLB
/// geometry tile; an opening `{` (after any UTF-8 BOM / leading ASCII whitespace) ⇒ a JSON tileset.
/// Returns `None` when neither signature matches, so the caller can fall back to the URI hint.
fn sniff_content_kind(bytes: &[u8]) -> Option<ContentKind> {
    if bytes.starts_with(b"glTF") {
        return Some(ContentKind::Glb);
    }
    // Skip a UTF-8 BOM, then any leading ASCII whitespace, and look for the JSON object opener.
    let rest = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let first = rest.iter().find(|b| !b.is_ascii_whitespace());
    match first {
        Some(b'{') => Some(ContentKind::Tileset),
        _ => None,
    }
}

fn tile_at_mut<'a>(root: &'a mut Tile, path: &[u32]) -> Option<&'a mut Tile> {
    let mut t = root;
    for &i in path {
        t = t.children.get_mut(i as usize)?;
    }
    Some(t)
}

// The ENU↔Unreal coordinate convention (×100, negate North) and the AABB→sphere reduction live in their
// one home, `logi_geo_mesh` (audit M21/DUP-5, AGENTS.md §1A).
use logi_geo_mesh::{aabb_to_sphere, enu_dir_to_ue, enu_pos_to_ue, ue_cm_to_enu};

#[cfg(test)]
mod streaming_tests {
    use super::*;

    #[test]
    fn sniff_classifies_by_magic_not_extension() {
        // glTF magic ⇒ GLB even from an extensionless / query-only URL.
        assert_eq!(sniff_content_kind(b"glTF\x02\0\0\0"), Some(ContentKind::Glb));
        // A JSON object ⇒ tileset, tolerating a UTF-8 BOM and leading whitespace.
        assert_eq!(sniff_content_kind(b"{\"root\":{}}"), Some(ContentKind::Tileset));
        assert_eq!(
            sniff_content_kind(b"\xEF\xBB\xBF  \n{\"asset\":{}}"),
            Some(ContentKind::Tileset)
        );
        // Unrecognisable content ⇒ None (caller falls back to the URI hint). Empty too.
        assert_eq!(sniff_content_kind(b"<html>"), None);
        assert_eq!(sniff_content_kind(b""), None);
    }

    #[test]
    fn is_tileset_uri_strips_query_before_extension() {
        assert!(is_tileset_uri("https://h/root.json"));
        assert!(is_tileset_uri("https://h/root.JSON?access_token=T"));
        // A .glb whose signed query merely contains ".json" must NOT be classed as a tileset.
        assert!(!is_tileset_uri("https://h/x.glb?session=a.json.b"));
        // Extensionless endpoints have no usable hint — byte sniffing is authoritative.
        assert!(!is_tileset_uri("https://h/v1/3dtiles?key=K"));
    }
}
