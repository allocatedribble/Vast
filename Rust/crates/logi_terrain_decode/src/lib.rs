//! Decode pages into renderable mesh data (rendering-strategy.md §5.1).
//!
//! `height_mesh` displaces a fixed grid template by a [`HeightPage`] and reconstructs normals from the
//! page's samples — edge vertices use the page's border samples, so **same-level** neighbors (which
//! share those samples by construction) join crack-free. At a LOD boundary a coarse tile's straight
//! edge and a finer neighbor's denser edge leave a T-junction gap; a perimeter **skirt** (a downward
//! wall around each tile) hides it. Phase 4 adds quantized-mesh / glTF decoders here.

use logi_terrain_geo::{MeshData, VERTEX_STRIDE};
use logi_terrain_pack::HeightPage;

/// Build a displaced grid mesh from a `HeightPage` spanning `world_size` x `world_size`, centered at the
/// origin in patch-local space (the renderer applies the patch's world position). Interleaved layout:
/// position float3, normal float3, uv float2 ([`VERTEX_STRIDE`] bytes).
pub fn height_mesh(page: &HeightPage, world_size: f32) -> MeshData {
    let w = page.width as usize;
    let h = page.height as usize;
    assert!(w >= 2 && h >= 2, "height page must be at least 2x2");
    // Guard the u32 vertex/index counts built below: the surface alone needs 6·(w-1)·(h-1) indices, so an
    // oversized page (width/height are u16, up to 65535²) would overflow `(indices.len()/4) as u32` and the
    // vertex buffer allocation. Cap w·h well under u32::MAX/6 — realistic pages are ≤513².
    assert!(
        (w as u64) * (h as u64) <= (u32::MAX as u64) / 8,
        "height page too large ({w}x{h}): u32 vertex/index counts would overflow"
    );

    let step_x = world_size / (w as f32 - 1.0);
    let step_z = world_size / (h as f32 - 1.0);
    let half = world_size * 0.5;

    let elev = |i: usize, j: usize| -> f32 { page.elevation(j * w + i) };

    // Surface grid (w*h verts). Track the elevation span (for the skirt depth) and the per-vertex
    // normals (reused on the skirt so the wall shades like the terrain it hides).
    let mut vertices = Vec::with_capacity(w * h * VERTEX_STRIDE);
    let mut normals: Vec<[f32; 3]> = Vec::with_capacity(w * h);
    let mut elev_lo = f32::INFINITY;
    let mut elev_hi = f32::NEG_INFINITY;
    for j in 0..h {
        for i in 0..w {
            let px = -half + i as f32 * step_x;
            let pz = -half + j as f32 * step_z;
            let py = elev(i, j);
            elev_lo = elev_lo.min(py);
            elev_hi = elev_hi.max(py);

            // Normal from central differences (edges clamp to border samples).
            let hl = elev(i.saturating_sub(1), j);
            let hr = elev((i + 1).min(w - 1), j);
            let hd = elev(i, j.saturating_sub(1));
            let hu = elev(i, (j + 1).min(h - 1));
            // FIX 10: true surface gradient — divide each height delta by its own world spacing
            // (was `ny = step_x + step_z`, dimensionally wrong and asymmetric for non-square grids).
            let nx = (hl - hr) / (2.0 * step_x);
            let nz = (hd - hu) / (2.0 * step_z);
            let ny = 1.0; // +Y up
            let inv = 1.0 / (nx * nx + ny * ny + nz * nz).max(1e-12).sqrt();
            let n = [nx * inv, ny * inv, nz * inv];
            normals.push(n);

            for v in [
                px,
                py,
                pz,
                n[0],
                n[1],
                n[2],
                i as f32 / (w as f32 - 1.0),
                j as f32 / (h as f32 - 1.0),
            ] {
                vertices.extend_from_slice(&v.to_le_bytes());
            }
        }
    }

    let mut indices: Vec<u8> = Vec::new();
    for j in 0..h - 1 {
        for i in 0..w - 1 {
            let i0 = (j * w + i) as u32;
            let i1 = i0 + 1;
            let i2 = i0 + w as u32;
            let i3 = i2 + 1;
            for idx in [i0, i2, i1, i1, i2, i3] {
                indices.extend_from_slice(&idx.to_le_bytes());
            }
        }
    }

    // ---- Skirts: hide T-junction cracks at LOD boundaries ----
    // A coarse neighbor's straight edge can sit above/below this tile's finer edge, leaving a gap the
    // sky shows through. Drop a vertical wall around the perimeter, deep enough that no crack exceeds
    // it (the tile's elevation relief bounds the worst case; +1 cell margin). The wall reuses each
    // edge vertex's normal + UV so it shades like the terrain, and is emitted DOUBLE-WOUND (both
    // windings) so it is visible from any view — back-face culling keeps the drawn cost to ~one face.
    let skirt_depth = (elev_hi - elev_lo).max(0.0) + step_x.max(step_z);

    // Perimeter ring (corners once → a continuous skirt).
    let mut perim: Vec<(usize, usize)> = Vec::with_capacity(2 * w + 2 * h);
    for i in 0..w {
        perim.push((i, 0));
    } // bottom edge
    for j in 1..h {
        perim.push((w - 1, j));
    } // right edge
    for i in (0..w - 1).rev() {
        perim.push((i, h - 1));
    } // top edge
    for j in (1..h - 1).rev() {
        perim.push((0, j));
    } // left edge

    let skirt_base = (w * h) as u32;
    for &(i, j) in &perim {
        let px = -half + i as f32 * step_x;
        let pz = -half + j as f32 * step_z;
        let py = elev(i, j) - skirt_depth;
        let n = normals[j * w + i];
        for v in [
            px,
            py,
            pz,
            n[0],
            n[1],
            n[2],
            i as f32 / (w as f32 - 1.0),
            j as f32 / (h as f32 - 1.0),
        ] {
            vertices.extend_from_slice(&v.to_le_bytes());
        }
    }
    let pn = perim.len();
    for t in 0..pn {
        let (i0, j0) = perim[t];
        let (i1, j1) = perim[(t + 1) % pn];
        let s0 = (j0 * w + i0) as u32; // surface verts (shared edge)
        let s1 = (j1 * w + i1) as u32;
        let k0 = skirt_base + t as u32; // skirt verts (dropped down)
        let k1 = skirt_base + ((t + 1) % pn) as u32;
        // quad (s0,s1,k1,k0) as two tris, then the reverse winding (double-sided).
        for tri in [[s0, s1, k0], [s1, k1, k0], [s0, k0, s1], [s1, k0, k1]] {
            for idx in tri {
                indices.extend_from_slice(&idx.to_le_bytes());
            }
        }
    }

    let vertex_count = skirt_base + pn as u32;
    let index_count = (indices.len() / 4) as u32;
    MeshData {
        vertex_count,
        index_count,
        vertices,
        indices,
    }
}
