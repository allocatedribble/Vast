//! Heightfield → render mesh. Triangulates a [`SemanticTile`] into the engine-wide interleaved 32-byte
//! vertex contract (`pos f3 | normal f3 | uv f2`) + `u32` indices, in **ENU metres**, with a helper to
//! convert to **Unreal render space** (cm, Z-up LH) — the same negate-North reflection the globe/building
//! lanes use ([`super::format`] is south→north, west→east row-major).
//!
//! Two meshers, selected by [`MeshingMode`] (the meshing toggle):
//! - **Uniform** — every grid sample becomes a vertex (`grid_n²` verts, `2·(grid_n-1)²` tris). Simple,
//!   any `grid_n ≥ 2`.
//! - **TIN** — an adaptive **right-triangulated irregular network** (RTIN / "martini": dense on slopes,
//!   sparse on flats) driven by a max vertical error, cutting vertex count a lot at equal fidelity. Needs
//!   `grid_n = 2^k + 1`; falls back to Uniform otherwise. Deterministic, crack-free *within* a tile (cross-
//!   tile edges rely on the same skirt mitigation the uniform lane plans).

use crate::format::SemanticTile;

/// 32 bytes: `pos f3 | normal f3 | uv f2` — the engine-wide vertex contract.
pub const VERTEX_STRIDE: usize = 32;

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
    /// pass through unchanged (reversing them would double-flip and back-face-cull the tile — the rule the
    /// globe/building lanes already follow). Normals negate North too (direction; no scale).
    pub fn to_ue_cm(&self) -> TerrainMesh {
        let mut vertices = self.vertices.clone();
        for v in 0..self.vertex_count() {
            let o = v * VERTEX_STRIDE;
            let px = f32::from_le_bytes(vertices[o..o + 4].try_into().unwrap()) * 100.0;
            let py = -f32::from_le_bytes(vertices[o + 4..o + 8].try_into().unwrap()) * 100.0;
            let pz = f32::from_le_bytes(vertices[o + 8..o + 12].try_into().unwrap()) * 100.0;
            vertices[o..o + 4].copy_from_slice(&px.to_le_bytes());
            vertices[o + 4..o + 8].copy_from_slice(&py.to_le_bytes());
            vertices[o + 8..o + 12].copy_from_slice(&pz.to_le_bytes());
            let ny = -f32::from_le_bytes(vertices[o + 16..o + 20].try_into().unwrap());
            vertices[o + 16..o + 20].copy_from_slice(&ny.to_le_bytes());
        }
        let bounds_center = [
            self.bounds_center[0] * 100.0,
            -self.bounds_center[1] * 100.0,
            self.bounds_center[2] * 100.0,
        ];
        TerrainMesh {
            vertices,
            indices: self.indices.clone(),
            bounds_center,
            bounds_radius: self.bounds_radius * 100.0,
        }
    }
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
/// (forward/backward at edges). Shared by both meshers so normals are identical regardless of
/// triangulation. `step` is the metric spacing between samples.
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

/// Triangulate a tile with the chosen [`MeshingMode`]. `Tin` falls back to `Uniform` when the grid is not
/// `2^k + 1`.
pub fn triangulate(tile: &SemanticTile, mode: MeshingMode) -> TerrainMesh {
    match mode {
        MeshingMode::Uniform => triangulate_enu(tile),
        MeshingMode::Tin { max_error_m } => triangulate_tin(tile, max_error_m).unwrap_or_else(|| triangulate_enu(tile)),
    }
}

/// Uniform triangulation to ENU metres (East = +X, North = +Y, Up = +Z; right-handed). Vertices are the
/// grid samples; triangles are CCW-from-above (outward normal +Up) — [`TerrainMesh::to_ue_cm`] applies the
/// reflection that flips them into Unreal's convention.
pub fn triangulate_enu(tile: &SemanticTile) -> TerrainMesh {
    let n = tile.grid_n as usize;
    let span = (n as f64 - 1.0).max(1.0);
    let step = (tile.size_m / span) as f32;
    let oe = tile.origin_e as f32;
    let on = tile.origin_n as f32;

    let mut vertices = Vec::with_capacity(n * n * VERTEX_STRIDE);
    let mut mn = [f32::MAX; 3];
    let mut mx = [f32::MIN; 3];
    for j in 0..n {
        for i in 0..n {
            let e = oe + i as f32 * step;
            let north = on + j as f32 * step;
            let up = tile.heights[j * n + i];
            let normal = normal_at(&tile.heights, n, step, i, j);
            let uv = [i as f32 / span as f32, j as f32 / span as f32];
            push_vertex(&mut vertices, [e, north, up], normal, uv);
            for k in 0..3 {
                let val = [e, north, up][k];
                mn[k] = mn[k].min(val);
                mx[k] = mx[k].max(val);
            }
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
            // Two CCW-from-above triangles per quad (both with +Up normal).
            indices.extend_from_slice(&[a, b, c, b, d, c]);
        }
    }

    TerrainMesh {
        vertices,
        indices,
        bounds_center: bounds_center(mn, mx),
        bounds_radius: bounds_radius(mn, mx),
    }
}

/// Adaptive **RTIN** (right-triangulated irregular network, the "martini" scheme) at a max vertical
/// `max_error_m`. Returns `None` if the grid is not `2^k + 1` (so the caller falls back to uniform).
/// Deterministic + crack-free within the tile; triangles are CCW-from-above (matching the uniform lane).
pub fn triangulate_tin(tile: &SemanticTile, max_error_m: f32) -> Option<TerrainMesh> {
    let size = tile.grid_n as usize;
    let tile_size = size.checked_sub(1)?;
    // RTIN requires a power-of-two tile size (grid_n = 2^k + 1) and at least a 2x2 cell tile.
    if tile_size < 2 || (tile_size & (tile_size - 1)) != 0 {
        return None;
    }

    let coords = rtin_coords(size);
    let errors = rtin_errors(&tile.heights, size, &coords);

    // Extract the adaptive triangle set (grid-space vertices + index triangles).
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

    // Build the render mesh from the selected grid vertices (normals from the full heightfield).
    let span = (size as f64 - 1.0).max(1.0);
    let step = (tile.size_m / span) as f32;
    let oe = tile.origin_e as f32;
    let on = tile.origin_n as f32;
    let mut vertices = Vec::with_capacity(st.verts.len() * VERTEX_STRIDE);
    let mut mn = [f32::MAX; 3];
    let mut mx = [f32::MIN; 3];
    for &(gx, gy) in &st.verts {
        let (xi, yi) = (gx as usize, gy as usize);
        let e = oe + gx as f32 * step;
        let north = on + gy as f32 * step;
        let up = tile.heights[yi * size + xi];
        let normal = normal_at(&tile.heights, size, step, xi, yi);
        let uv = [gx as f32 / span as f32, gy as f32 / span as f32];
        push_vertex(&mut vertices, [e, north, up], normal, uv);
        for k in 0..3 {
            let val = [e, north, up][k];
            mn[k] = mn[k].min(val);
            mx[k] = mx[k].max(val);
        }
    }

    Some(TerrainMesh {
        vertices,
        indices: st.tris,
        bounds_center: bounds_center(mn, mx),
        bounds_radius: bounds_radius(mn, mx),
    })
}

#[inline]
fn bounds_center(mn: [f32; 3], mx: [f32; 3]) -> [f32; 3] {
    [(mn[0] + mx[0]) * 0.5, (mn[1] + mx[1]) * 0.5, (mn[2] + mx[2]) * 0.5]
}
#[inline]
fn bounds_radius(mn: [f32; 3], mx: [f32; 3]) -> f32 {
    0.5 * ((mx[0] - mn[0]).powi(2) + (mx[1] - mn[1]).powi(2) + (mx[2] - mn[2]).powi(2)).sqrt()
}

// ---- RTIN / martini -------------------------------------------------------------------------------
// Port of Mapbox's "martini" (RTIN) terrain mesher: an implicit bin-tree of right triangles over a
// (2^k+1)² grid. `rtin_coords` precomputes each triangle's hypotenuse endpoints; `rtin_errors` walks the
// tree smallest-first accumulating each triangle's mid-edge approximation error into a per-sample array;
// extraction descends from the two base triangles, subdividing only where the error exceeds the budget.

/// Hypotenuse endpoints `(ax, ay, bx, by)` for every triangle in the implicit bin-tree.
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

/// Per-sample max approximation error (metres), smallest triangles first so parents fold in their
/// children's error.
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
        let cx = mx + my - ay; // right-angle apex (90° rotation of the hypotenuse midpoint)
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
        // Subdivide while the hypotenuse is longer than one cell AND the mid-edge error exceeds budget.
        if (ax - cx).abs() + (ay - cy).abs() > 1 && self.errors[my as usize * self.size + mx as usize] > self.max_error
        {
            self.process(cx, cy, ax, ay, mx, my);
            self.process(bx, by, cx, cy, mx, my);
        } else {
            let ia = self.vid(ax, ay);
            let ib = self.vid(bx, by);
            let ic = self.vid(cx, cy);
            // martini emits (a, b, c) which is CW in our (East, North) frame; flip to (a, c, b) so every
            // tri is CCW-from-above (+Up), matching the uniform lane → to_ue_cm's reflection is correct.
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
    /// Signed area of a triangle in the (East, North) plane; > 0 ⇒ CCW-from-above.
    fn signed_area_en(m: &TerrainMesh, t: usize) -> f32 {
        let a = vpos(m, m.indices[t * 3]);
        let b = vpos(m, m.indices[t * 3 + 1]);
        let c = vpos(m, m.indices[t * 3 + 2]);
        0.5 * ((b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1]))
    }
    fn peak_tile(size: u32) -> SemanticTile {
        // A localized central bump so flat regions can merge under TIN.
        let n = size as usize;
        let c = (n as f32 - 1.0) * 0.5;
        let heights: Vec<f32> = (0..n * n)
            .map(|k| {
                let (i, j) = ((k % n) as f32, (k / n) as f32);
                let d = ((i - c).powi(2) + (j - c).powi(2)).sqrt();
                (20.0 - d).max(0.0) // a cone peak in the middle
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
    fn triangulation_is_deterministic() {
        let t = peak_tile(9);
        assert_eq!(triangulate_enu(&t), triangulate_enu(&t));
        assert_eq!(triangulate_tin(&t, 0.5), triangulate_tin(&t, 0.5));
    }

    #[test]
    fn tin_flat_field_collapses_to_two_triangles() {
        let n = 9u32; // 2^3 + 1
        let t = SemanticTile::new(TileId::new(0, 0, 0), n, 0.0, 0.0, 32.0, vec![7.0; (n * n) as usize]).unwrap();
        let m = triangulate_tin(&t, 0.1).expect("pow2 grid");
        assert_eq!(m.vertex_count(), 4, "flat tile → 4 corner verts");
        assert_eq!(m.indices.len(), 6, "flat tile → 2 triangles");
    }

    #[test]
    fn tin_error_threshold_is_monotone_and_below_uniform() {
        let t = peak_tile(33); // 2^5 + 1
        let uniform = triangulate_enu(&t).vertex_count();
        let coarse = triangulate_tin(&t, 4.0).unwrap().vertex_count();
        let fine = triangulate_tin(&t, 0.25).unwrap().vertex_count();
        assert!(coarse <= fine, "higher error ⇒ fewer/equal verts ({coarse} <= {fine})");
        assert!(
            fine <= uniform,
            "TIN never exceeds the uniform vertex count ({fine} <= {uniform})"
        );
        assert!(
            coarse < uniform,
            "a coarse TIN actually saves verts ({coarse} < {uniform})"
        );
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
        // grid_n = 4 → tile_size 3 (not 2^k): triangulate_tin returns None; the dispatcher uses uniform.
        let t = peak_tile(4);
        assert!(triangulate_tin(&t, 0.5).is_none());
        let m = triangulate(&t, MeshingMode::Tin { max_error_m: 0.5 });
        assert_eq!(m.vertex_count(), 16, "fell back to the uniform 4x4 mesh");
    }
}
