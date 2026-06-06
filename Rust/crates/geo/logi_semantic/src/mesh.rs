//! Heightfield → render mesh. Triangulates a [`SemanticTile`] into the engine-wide interleaved 32-byte
//! vertex contract (`pos f3 | normal f3 | uv f2`) + `u32` indices, in **ENU metres**, with a helper to
//! convert to **Unreal render space** (cm, Z-up LH) — the same negate-North reflection the globe/building
//! lanes use ([`super::format`] is south→north, west→east row-major).
//!
//! Two meshers, selected by [`MeshingMode`] (the meshing toggle):
//! - **Uniform** — every grid sample becomes a vertex. Any `grid_n ≥ 2`.
//! - **TIN** — an adaptive **right-triangulated irregular network** (RTIN / "martini": dense on slopes,
//!   sparse on flats) driven by a max vertical error. Needs `grid_n = 2^k + 1`; falls back to Uniform.
//!
//! Both can append a downward **skirt** (a vertical wall hanging from the tile's perimeter) to hide
//! cross-tile T-junction cracks — TIN tiles are crack-free internally but neighbouring tiles can pick
//! different edge vertices. Skirts are wound outward (their reflected UE faces point away from the tile).

use crate::format::SemanticTile;

/// The 32-byte interleaved vertex contract (`pos f3 | normal f3 | uv f2`), re-exported from its one home,
/// [`logi_geo_mesh`] (AGENTS.md §1A).
pub use logi_geo_mesh::VERTEX_STRIDE;

/// Which mesher to use — the meshing toggle (default [`MeshingMode::Uniform`]).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum MeshingMode {
    /// Uniform grid: one vertex per heightfield sample.
    #[default]
    Uniform,
    /// Adaptive RTIN at the given max vertical error (metres). Larger error ⇒ fewer triangles.
    Tin { max_error_m: f32 },
}

/// A triangulated terrain tile in one coordinate space (ENU metres, or — after [`TerrainMesh::to_ue_cm`]
/// — Unreal cm). `vertices` is [`VERTEX_STRIDE`]-byte interleaved; `indices` are `u32` triangle lists.
#[derive(Clone, Debug, PartialEq)]
pub struct TerrainMesh {
    pub vertices: Vec<u8>,
    pub indices: Vec<u32>,
    pub bounds_center: [f32; 3],
    pub bounds_radius: f32,
}

impl TerrainMesh {
    pub fn vertex_count(&self) -> usize {
        self.vertices.len() / VERTEX_STRIDE
    }

    /// ENU metres → Unreal render space (cm, Z-up LH): scale ×100 and negate North (Y). The negation is a
    /// reflection that *also* flips triangle winding into Unreal's front-face convention, so `indices`
    /// pass through unchanged (the globe/building lanes' rule). Normals negate North too (no scale).
    ///
    /// In-place / move variant: rewrites `self.vertices` in place and **moves** `self.indices` (which are
    /// unchanged) — no second geometry allocation. Prefer this when the ENU mesh is discarded after the
    /// conversion (the world-build path); use [`Self::to_ue_cm`] only when the ENU mesh must survive.
    pub fn into_ue_cm(mut self) -> TerrainMesh {
        transform_enu_to_ue(&mut self.vertices);
        self.bounds_center = [
            self.bounds_center[0] * 100.0,
            -self.bounds_center[1] * 100.0,
            self.bounds_center[2] * 100.0,
        ];
        self.bounds_radius *= 100.0;
        self // indices moved untouched
    }

    /// Borrowing variant of [`Self::into_ue_cm`]: clones the mesh first (so the ENU original survives).
    /// When the ENU mesh is discarded, call [`Self::into_ue_cm`] instead to avoid the copy.
    pub fn to_ue_cm(&self) -> TerrainMesh {
        self.clone().into_ue_cm()
    }
}

/// Rewrite an interleaved [`VERTEX_STRIDE`]-byte vertex buffer from ENU metres to Unreal cm in place:
/// each position scales ×100 with North (Y) negated; each normal negates North (no scale). Shared by both
/// [`TerrainMesh::into_ue_cm`] and [`TerrainMesh::to_ue_cm`] so the conversion lives in exactly one place.
fn transform_enu_to_ue(vertices: &mut [u8]) {
    let read = |b: &[u8], o: usize| f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    for chunk in vertices.chunks_exact_mut(VERTEX_STRIDE) {
        let px = read(chunk, 0) * 100.0;
        let py = -read(chunk, 4) * 100.0;
        let pz = read(chunk, 8) * 100.0;
        let ny = -read(chunk, 16);
        chunk[0..4].copy_from_slice(&px.to_le_bytes());
        chunk[4..8].copy_from_slice(&py.to_le_bytes());
        chunk[8..12].copy_from_slice(&pz.to_le_bytes());
        chunk[16..20].copy_from_slice(&ny.to_le_bytes());
    }
}

/// An un-packed vertex carrying its grid coordinate (so skirts can find the perimeter).
#[derive(Clone, Copy)]
struct RawVert {
    pos: [f32; 3],
    normal: [f32; 3],
    uv: [f32; 2],
    gx: u32,
    gy: u32,
}

/// An un-packed mesh: vertices (with grid coords) + index triangles, before skirts/packing.
struct RawMesh {
    verts: Vec<RawVert>,
    indices: Vec<u32>,
}

#[inline]
fn push_vertex(buf: &mut Vec<u8>, p: [f32; 3], n: [f32; 3], uv: [f32; 2]) {
    for f in p {
        buf.extend_from_slice(&f.to_le_bytes());
    }
    for f in n {
        buf.extend_from_slice(&f.to_le_bytes());
    }
    for f in uv {
        buf.extend_from_slice(&f.to_le_bytes());
    }
}

/// Unit surface normal at grid sample `(x, y)` from a central difference of the heightfield
/// (forward/backward at edges). Shared by both meshers so normals match regardless of triangulation.
fn normal_at(heights: &[f32], size: usize, step: f32, x: usize, y: usize) -> [f32; 3] {
    let h = |i: usize, j: usize| heights[j * size + i];
    let xl = x.saturating_sub(1);
    let xr = (x + 1).min(size - 1);
    let yd = y.saturating_sub(1);
    let yu = (y + 1).min(size - 1);
    let de = ((xr - xl) as f32 * step).max(f32::EPSILON);
    let dn = ((yu - yd) as f32 * step).max(f32::EPSILON);
    let dz_de = (h(xr, y) - h(xl, y)) / de;
    let dz_dn = (h(x, yu) - h(x, yd)) / dn;
    let n = [-dz_de, -dz_dn, 1.0];
    let inv = 1.0 / (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    [n[0] * inv, n[1] * inv, n[2] * inv]
}

/// Triangulate a tile with the chosen [`MeshingMode`], optionally appending a `skirt_depth_m` skirt
/// (0 = none). `Tin` falls back to `Uniform` when the grid is not `2^k + 1`.
pub fn triangulate(tile: &SemanticTile, mode: MeshingMode, skirt_depth_m: f32) -> TerrainMesh {
    let size = tile.grid_n as usize;
    let raw = match mode {
        MeshingMode::Uniform => raw_uniform(tile),
        MeshingMode::Tin { max_error_m } => raw_tin(tile, max_error_m).unwrap_or_else(|| raw_uniform(tile)),
    };
    pack(raw, size, skirt_depth_m)
}

/// Uniform triangulation (no skirt). Vertices are the grid samples; triangles CCW-from-above (+Up).
pub fn triangulate_enu(tile: &SemanticTile) -> TerrainMesh {
    pack(raw_uniform(tile), tile.grid_n as usize, 0.0)
}

/// Adaptive **RTIN** triangulation at `max_error_m` (no skirt). `None` if the grid is not `2^k + 1`.
pub fn triangulate_tin(tile: &SemanticTile, max_error_m: f32) -> Option<TerrainMesh> {
    let size = tile.grid_n as usize;
    raw_tin(tile, max_error_m).map(|raw| pack(raw, size, 0.0))
}

fn raw_uniform(tile: &SemanticTile) -> RawMesh {
    let n = tile.grid_n as usize;
    let span = (n as f64 - 1.0).max(1.0);
    let step = (tile.size_m / span) as f32;
    let oe = tile.origin_e as f32;
    let on = tile.origin_n as f32;

    let mut verts = Vec::with_capacity(n * n);
    for j in 0..n {
        for i in 0..n {
            let e = oe + i as f32 * step;
            let north = on + j as f32 * step;
            let up = tile.heights[j * n + i];
            let normal = normal_at(&tile.heights, n, step, i, j);
            let uv = [i as f32 / span as f32, j as f32 / span as f32];
            verts.push(RawVert {
                pos: [e, north, up],
                normal,
                uv,
                gx: i as u32,
                gy: j as u32,
            });
        }
    }
    let mut indices = Vec::with_capacity((n - 1) * (n - 1) * 6);
    let idx = |i: usize, j: usize| (j * n + i) as u32;
    for j in 0..n - 1 {
        for i in 0..n - 1 {
            let a = idx(i, j);
            let b = idx(i + 1, j);
            let c = idx(i, j + 1);
            let d = idx(i + 1, j + 1);
            indices.extend_from_slice(&[a, b, c, b, d, c]); // two CCW-from-above tris
        }
    }
    RawMesh { verts, indices }
}

fn raw_tin(tile: &SemanticTile, max_error_m: f32) -> Option<RawMesh> {
    let size = tile.grid_n as usize;
    let tile_size = size.checked_sub(1)?;
    if tile_size < 2 || (tile_size & (tile_size - 1)) != 0 {
        return None; // RTIN needs grid_n = 2^k + 1 and a >= 2x2 cell tile
    }
    let coords = rtin_coords(size);
    let errors = rtin_errors(&tile.heights, size, &coords);
    let mut st = TinExtract {
        errors: &errors,
        size,
        max_error: max_error_m,
        index_of: vec![0u32; size * size],
        verts: Vec::new(),
        tris: Vec::new(),
    };
    let max = tile_size as i32;
    st.process(0, 0, max, max, max, 0);
    st.process(max, max, 0, 0, 0, max);

    let span = (size as f64 - 1.0).max(1.0);
    let step = (tile.size_m / span) as f32;
    let oe = tile.origin_e as f32;
    let on = tile.origin_n as f32;
    let mut verts = Vec::with_capacity(st.verts.len());
    for &(gx, gy) in &st.verts {
        let (xi, yi) = (gx as usize, gy as usize);
        let e = oe + gx as f32 * step;
        let north = on + gy as f32 * step;
        let up = tile.heights[yi * size + xi];
        let normal = normal_at(&tile.heights, size, step, xi, yi);
        let uv = [gx as f32 / span as f32, gy as f32 / span as f32];
        verts.push(RawVert {
            pos: [e, north, up],
            normal,
            uv,
            gx,
            gy,
        });
    }
    Some(RawMesh {
        verts,
        indices: st.tris,
    })
}

/// Append skirts (if `skirt_depth_m > 0`), compute bounds, and pack to the interleaved 32-byte buffer.
fn pack(mut raw: RawMesh, size: usize, skirt_depth_m: f32) -> TerrainMesh {
    if skirt_depth_m > 0.0 && raw.verts.len() >= 4 {
        append_skirt(&mut raw.verts, &mut raw.indices, size, skirt_depth_m);
    }
    let mut vertices = Vec::with_capacity(raw.verts.len() * VERTEX_STRIDE);
    let mut mn = [f32::MAX; 3];
    let mut mx = [f32::MIN; 3];
    for v in &raw.verts {
        push_vertex(&mut vertices, v.pos, v.normal, v.uv);
        for k in 0..3 {
            mn[k] = mn[k].min(v.pos[k]);
            mx[k] = mx[k].max(v.pos[k]);
        }
    }
    TerrainMesh {
        vertices,
        indices: raw.indices,
        bounds_center: bounds_center(mn, mx),
        bounds_radius: bounds_radius(mn, mx),
    }
}

/// The tile's boundary vertices as one **CCW-from-above** loop (SW→SE→NE→NW). Corners belong to the
/// south/north runs; the east/west runs hold only strictly-interior edge vertices, so the loop has no
/// duplicates. (TIN always emits the 4 corners — they're the base-triangle vertices.)
fn perimeter_loop(verts: &[RawVert], size: usize) -> Vec<u32> {
    let maxc = (size - 1) as u32;
    let mut south: Vec<(u32, u32)> = Vec::new(); // (gx, index), gy==0
    let mut east: Vec<(u32, u32)> = Vec::new(); // (gy, index), gx==max, interior
    let mut north: Vec<(u32, u32)> = Vec::new(); // (gx, index), gy==max
    let mut west: Vec<(u32, u32)> = Vec::new(); // (gy, index), gx==0, interior
    for (i, v) in verts.iter().enumerate() {
        let i = i as u32;
        if v.gy == 0 {
            south.push((v.gx, i));
        } else if v.gy == maxc {
            north.push((v.gx, i));
        } else if v.gx == maxc {
            east.push((v.gy, i));
        } else if v.gx == 0 {
            west.push((v.gy, i));
        }
    }
    south.sort_by_key(|&(g, _)| g); // SW → SE
    east.sort_by_key(|&(g, _)| g); // SE+1 → NE-1
    north.sort_by_key(|&(g, _)| std::cmp::Reverse(g)); // NE → NW
    west.sort_by_key(|&(g, _)| std::cmp::Reverse(g)); // NW-1 → SW+1
    let mut out = Vec::with_capacity(south.len() + east.len() + north.len() + west.len());
    out.extend(south.into_iter().map(|(_, i)| i));
    out.extend(east.into_iter().map(|(_, i)| i));
    out.extend(north.into_iter().map(|(_, i)| i));
    out.extend(west.into_iter().map(|(_, i)| i));
    out
}

/// Append a downward skirt around the tile perimeter. For each perimeter vertex it adds a twin dropped by
/// `depth`, then stitches outward-facing quads. Winding `(a, b', b) + (a, a', b')` over a CCW-from-above
/// loop yields normals pointing away from the tile centre (verified by `skirt_triangle_normals_point_outward`).
fn append_skirt(verts: &mut Vec<RawVert>, indices: &mut Vec<u32>, size: usize, depth: f32) {
    let loop_top = perimeter_loop(verts, size);
    let n = loop_top.len();
    if n < 2 {
        return;
    }
    let (mut ce, mut cn) = (0.0f32, 0.0f32);
    for &vi in &loop_top {
        ce += verts[vi as usize].pos[0];
        cn += verts[vi as usize].pos[1];
    }
    ce /= n as f32;
    cn /= n as f32;

    let base = verts.len() as u32;
    for &vi in &loop_top {
        let top = verts[vi as usize];
        let pos = [top.pos[0], top.pos[1], top.pos[2] - depth];
        let (dx, dy) = (top.pos[0] - ce, top.pos[1] - cn);
        let len = (dx * dx + dy * dy).sqrt().max(f32::EPSILON);
        let normal = [dx / len, dy / len, 0.0]; // outward horizontal
        verts.push(RawVert {
            pos,
            normal,
            uv: top.uv,
            gx: top.gx,
            gy: top.gy,
        });
    }
    for i in 0..n {
        let j = (i + 1) % n; // closed loop
        let a = loop_top[i];
        let b = loop_top[j];
        let ap = base + i as u32;
        let bp = base + j as u32;
        indices.extend_from_slice(&[a, bp, b, a, ap, bp]);
    }
}

// AABB→bounding-sphere lives in its one home, `logi_geo_mesh` (DUP-5, AGENTS.md §1A); these thin adapters
// keep the existing call sites. (`to_ue_cm`'s in-place byte transform stays local: reusing the scalar ENU
// fns would shift its f32 math to f64 — a needless rounding change in non-authoritative render data.)
#[inline]
fn bounds_center(mn: [f32; 3], mx: [f32; 3]) -> [f32; 3] {
    logi_geo_mesh::aabb_to_sphere(mn, mx).0
}
#[inline]
fn bounds_radius(mn: [f32; 3], mx: [f32; 3]) -> f32 {
    logi_geo_mesh::aabb_to_sphere(mn, mx).1
}

// ---- RTIN / martini -------------------------------------------------------------------------------
// Port of Mapbox's "martini" (RTIN): an implicit bin-tree of right triangles over a (2^k+1)² grid.

fn rtin_coords(size: usize) -> Vec<i32> {
    let tile_size = (size - 1) as i32;
    let num_triangles = (tile_size as usize) * (tile_size as usize) * 2 - 2;
    let mut coords = vec![0i32; num_triangles * 4];
    for i in 0..num_triangles {
        let mut id = i + 2;
        let (mut ax, mut ay, mut bx, mut by, mut cx, mut cy) = (0i32, 0i32, 0i32, 0i32, 0i32, 0i32);
        if id & 1 == 1 {
            bx = tile_size;
            by = tile_size;
            cx = tile_size;
        } else {
            ax = tile_size;
            ay = tile_size;
            cy = tile_size;
        }
        id >>= 1;
        while id > 1 {
            let mx = (ax + bx) >> 1;
            let my = (ay + by) >> 1;
            if id & 1 == 1 {
                bx = ax;
                by = ay;
                ax = cx;
                ay = cy;
            } else {
                ax = bx;
                ay = by;
                bx = cx;
                by = cy;
            }
            cx = mx;
            cy = my;
            id >>= 1;
        }
        let k = i * 4;
        coords[k] = ax;
        coords[k + 1] = ay;
        coords[k + 2] = bx;
        coords[k + 3] = by;
    }
    coords
}

fn rtin_errors(terrain: &[f32], size: usize, coords: &[i32]) -> Vec<f32> {
    let tile_size = size - 1;
    let num_triangles = tile_size * tile_size * 2 - 2;
    let num_parent = num_triangles - tile_size * tile_size;
    let mut errors = vec![0f32; size * size];
    for i in (0..num_triangles).rev() {
        let k = i * 4;
        let (ax, ay, bx, by) = (coords[k], coords[k + 1], coords[k + 2], coords[k + 3]);
        let mx = (ax + bx) >> 1;
        let my = (ay + by) >> 1;
        let cx = mx + my - ay; // right-angle apex
        let cy = my + ax - mx;
        let interp = (terrain[ay as usize * size + ax as usize] + terrain[by as usize * size + bx as usize]) * 0.5;
        let middle = my as usize * size + mx as usize;
        let middle_err = (interp - terrain[middle]).abs();
        if middle_err > errors[middle] {
            errors[middle] = middle_err;
        }
        if i < num_parent {
            let lci = (((ay + cy) >> 1) as usize) * size + (((ax + cx) >> 1) as usize);
            let rci = (((by + cy) >> 1) as usize) * size + (((bx + cx) >> 1) as usize);
            errors[middle] = errors[middle].max(errors[lci]).max(errors[rci]);
        }
    }
    errors
}

struct TinExtract<'a> {
    errors: &'a [f32],
    size: usize,
    max_error: f32,
    index_of: Vec<u32>, // grid index → vertex id + 1 (0 = unassigned)
    verts: Vec<(u32, u32)>,
    tris: Vec<u32>,
}

impl TinExtract<'_> {
    fn vid(&mut self, x: i32, y: i32) -> u32 {
        let p = y as usize * self.size + x as usize;
        let cur = self.index_of[p];
        if cur != 0 {
            return cur - 1;
        }
        let id = self.verts.len() as u32;
        self.verts.push((x as u32, y as u32));
        self.index_of[p] = id + 1;
        id
    }

    fn process(&mut self, ax: i32, ay: i32, bx: i32, by: i32, cx: i32, cy: i32) {
        let mx = (ax + bx) >> 1;
        let my = (ay + by) >> 1;
        if (ax - cx).abs() + (ay - cy).abs() > 1 && self.errors[my as usize * self.size + mx as usize] > self.max_error
        {
            self.process(cx, cy, ax, ay, mx, my);
            self.process(bx, by, cx, cy, mx, my);
        } else {
            let ia = self.vid(ax, ay);
            let ib = self.vid(bx, by);
            let ic = self.vid(cx, cy);
            // Flip martini's (a,b,c) to (a,c,b) so every tri is CCW-from-above (+Up), matching uniform.
            self.tris.extend_from_slice(&[ia, ic, ib]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{format::SemanticTile, TileId};

    fn read_f32(buf: &[u8], o: usize) -> f32 {
        f32::from_le_bytes(buf[o..o + 4].try_into().unwrap())
    }
    fn vpos(m: &TerrainMesh, vi: u32) -> [f32; 3] {
        let o = vi as usize * VERTEX_STRIDE;
        [
            read_f32(&m.vertices, o),
            read_f32(&m.vertices, o + 4),
            read_f32(&m.vertices, o + 8),
        ]
    }
    fn signed_area_en(m: &TerrainMesh, t: usize) -> f32 {
        let a = vpos(m, m.indices[t * 3]);
        let b = vpos(m, m.indices[t * 3 + 1]);
        let c = vpos(m, m.indices[t * 3 + 2]);
        0.5 * ((b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1]))
    }
    fn peak_tile(size: u32) -> SemanticTile {
        let n = size as usize;
        let c = (n as f32 - 1.0) * 0.5;
        let heights: Vec<f32> = (0..n * n)
            .map(|k| {
                let (i, j) = ((k % n) as f32, (k / n) as f32);
                let d = ((i - c).powi(2) + (j - c).powi(2)).sqrt();
                (20.0 - d).max(0.0)
            })
            .collect();
        SemanticTile::new(TileId::new(0, 0, 0), size, 0.0, 0.0, (size as f64 - 1.0) * 4.0, heights).unwrap()
    }

    #[test]
    fn flat_field_counts_and_up_normals() {
        let n = 5u32;
        let t = SemanticTile::new(TileId::new(0, 0, 0), n, 0.0, 0.0, 40.0, vec![10.0; (n * n) as usize]).unwrap();
        let m = triangulate_enu(&t);
        assert_eq!(m.vertex_count(), (n * n) as usize);
        assert_eq!(m.indices.len(), ((n - 1) * (n - 1) * 6) as usize);
        for v in 0..m.vertex_count() {
            let o = v * VERTEX_STRIDE;
            assert!(read_f32(&m.vertices, o + 12).abs() < 1e-6);
            assert!(read_f32(&m.vertices, o + 16).abs() < 1e-6);
            assert!((read_f32(&m.vertices, o + 20) - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn known_slope_normal_tilts_east() {
        let n = 3u32;
        let step = 10.0_f64;
        let heights: Vec<f32> = (0..n * n).map(|k| (k % n) as f32 * step as f32).collect();
        let t = SemanticTile::new(TileId::new(0, 0, 0), n, 0.0, 0.0, 20.0, heights).unwrap();
        let m = triangulate_enu(&t);
        let nx = read_f32(&m.vertices, 12);
        let ny = read_f32(&m.vertices, 16);
        let nz = read_f32(&m.vertices, 20);
        assert!(nx < 0.0, "normal tilts toward -East: {nx}");
        assert!(ny.abs() < 1e-6, "no North tilt: {ny}");
        assert!((nx * nx + ny * ny + nz * nz - 1.0).abs() < 1e-5, "unit normal");
    }

    #[test]
    fn to_ue_negates_north_and_scales() {
        let n = 2u32;
        let t = SemanticTile::new(TileId::new(0, 0, 0), n, 1.0, 2.0, 10.0, vec![0.0; 4]).unwrap();
        let ue = triangulate_enu(&t).to_ue_cm();
        let o = 3 * VERTEX_STRIDE;
        assert!((read_f32(&ue.vertices, o) - 1100.0).abs() < 1e-3);
        assert!((read_f32(&ue.vertices, o + 4) + 1200.0).abs() < 1e-3);
        assert!(read_f32(&ue.vertices, o + 8).abs() < 1e-3);
    }

    #[test]
    fn into_ue_cm_matches_borrowing_variant() {
        // The move variant (in-place rewrite + moved indices) must produce a byte-identical mesh to the
        // clone-first borrowing variant.
        let t = peak_tile(9);
        let enu = triangulate(&t, MeshingMode::Uniform, 2.0);
        let borrowed = enu.to_ue_cm();
        let moved = enu.into_ue_cm();
        assert_eq!(borrowed, moved);
    }

    #[test]
    fn triangulation_is_deterministic() {
        let t = peak_tile(9);
        assert_eq!(triangulate_enu(&t), triangulate_enu(&t));
        assert_eq!(triangulate_tin(&t, 0.5), triangulate_tin(&t, 0.5));
        assert_eq!(
            triangulate(&t, MeshingMode::Tin { max_error_m: 0.5 }, 2.0),
            triangulate(&t, MeshingMode::Tin { max_error_m: 0.5 }, 2.0)
        );
    }

    #[test]
    fn tin_flat_field_collapses_to_two_triangles() {
        let n = 9u32;
        let t = SemanticTile::new(TileId::new(0, 0, 0), n, 0.0, 0.0, 32.0, vec![7.0; (n * n) as usize]).unwrap();
        let m = triangulate_tin(&t, 0.1).expect("pow2 grid");
        assert_eq!(m.vertex_count(), 4, "flat tile → 4 corner verts");
        assert_eq!(m.indices.len(), 6, "flat tile → 2 triangles");
    }

    #[test]
    fn tin_error_threshold_is_monotone_and_below_uniform() {
        let t = peak_tile(33);
        let uniform = triangulate_enu(&t).vertex_count();
        let coarse = triangulate_tin(&t, 4.0).unwrap().vertex_count();
        let fine = triangulate_tin(&t, 0.25).unwrap().vertex_count();
        assert!(coarse <= fine, "higher error ⇒ fewer/equal verts ({coarse} <= {fine})");
        assert!(fine <= uniform, "TIN never exceeds uniform ({fine} <= {uniform})");
        assert!(coarse < uniform, "a coarse TIN saves verts ({coarse} < {uniform})");
    }

    #[test]
    fn tin_triangles_are_ccw_from_above() {
        let t = peak_tile(17);
        let m = triangulate_tin(&t, 1.0).unwrap();
        let tri_count = m.indices.len() / 3;
        assert!(tri_count > 2);
        for tri in 0..tri_count {
            assert!(signed_area_en(&m, tri) > 0.0, "tri {tri} must be CCW-from-above");
        }
    }

    #[test]
    fn tin_falls_back_for_non_power_of_two_grid() {
        let t = peak_tile(4);
        assert!(triangulate_tin(&t, 0.5).is_none());
        let m = triangulate(&t, MeshingMode::Tin { max_error_m: 0.5 }, 0.0);
        assert_eq!(m.vertex_count(), 16, "fell back to the uniform 4x4 mesh");
    }

    #[test]
    fn skirt_adds_walls_below_surface() {
        let t = peak_tile(5);
        let plain = triangulate(&t, MeshingMode::Uniform, 0.0);
        let skirted = triangulate(&t, MeshingMode::Uniform, 3.0);
        // 5x5 perimeter = 16 vertices → 16 skirt twins, 16 skirt quads (32 tris).
        assert_eq!(skirted.vertex_count(), plain.vertex_count() + 16);
        assert_eq!(skirted.indices.len(), plain.indices.len() + 16 * 6);
        // Skirt twins sit `depth` below the perimeter tops.
        let zof = |m: &TerrainMesh, vi: usize| read_f32(&m.vertices, vi * VERTEX_STRIDE + 8);
        let orig = plain.vertex_count();
        let min_top = (0..orig).map(|v| zof(&skirted, v)).fold(f32::MAX, f32::min);
        let min_skirt = (orig..skirted.vertex_count())
            .map(|v| zof(&skirted, v))
            .fold(f32::MAX, f32::min);
        assert!(min_skirt <= min_top - 3.0 + 1e-3, "skirt dropped by depth");
    }

    #[test]
    fn skirt_triangle_normals_point_outward() {
        // Flat tile → vertical skirt walls; every skirt face must point away from the tile centre.
        let n = 5u32;
        let t = SemanticTile::new(TileId::new(0, 0, 0), n, 0.0, 0.0, 40.0, vec![10.0; (n * n) as usize]).unwrap();
        let m = triangulate(&t, MeshingMode::Uniform, 2.0);
        let orig = (n * n) as usize;
        let center = [20.0f32, 20.0f32]; // origin 0, size 40 → centre (20, 20)
        let mut checked = 0;
        for tri in 0..m.indices.len() / 3 {
            let (i0, i1, i2) = (
                m.indices[tri * 3] as usize,
                m.indices[tri * 3 + 1] as usize,
                m.indices[tri * 3 + 2] as usize,
            );
            if i0 < orig && i1 < orig && i2 < orig {
                continue; // a surface triangle
            }
            let a = vpos(&m, m.indices[tri * 3]);
            let b = vpos(&m, m.indices[tri * 3 + 1]);
            let c = vpos(&m, m.indices[tri * 3 + 2]);
            let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
            let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
            let nrm = [
                u[1] * v[2] - u[2] * v[1],
                u[2] * v[0] - u[0] * v[2],
                u[0] * v[1] - u[1] * v[0],
            ];
            let horiz = (nrm[0] * nrm[0] + nrm[1] * nrm[1]).sqrt();
            assert!(horiz > nrm[2].abs(), "skirt normal mostly horizontal (tri {tri})");
            let cx = (a[0] + b[0] + c[0]) / 3.0 - center[0];
            let cy = (a[1] + b[1] + c[1]) / 3.0 - center[1];
            assert!(nrm[0] * cx + nrm[1] * cy > 0.0, "skirt tri {tri} must face outward");
            checked += 1;
        }
        assert!(checked >= 32, "expected 16 skirt quads = 32 tris, checked {checked}");
    }
}
