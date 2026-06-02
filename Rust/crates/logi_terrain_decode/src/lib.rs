//! Decode pages into renderable mesh data (rendering-strategy.md §5.1).
//!
//! `height_mesh` displaces a fixed grid template by a [`HeightPage`] and reconstructs normals from the
//! page's samples — edge vertices use the page's border samples, so neighboring patches (which share
//! those samples by construction) join crack-free. Phase 4 adds quantized-mesh / glTF decoders here.

use logi_terrain_geo::{MeshData, VERTEX_STRIDE};
use logi_terrain_pack::HeightPage;

/// Build a displaced grid mesh from a `HeightPage` spanning `world_size` x `world_size`, centered at the
/// origin in patch-local space (the renderer applies the patch's world position). Interleaved layout:
/// position float3, normal float3, uv float2 ([`VERTEX_STRIDE`] bytes).
pub fn height_mesh(page: &HeightPage, world_size: f32) -> MeshData {
    let w = page.width as usize;
    let h = page.height as usize;
    assert!(w >= 2 && h >= 2, "height page must be at least 2x2");

    let step_x = world_size / (w as f32 - 1.0);
    let step_z = world_size / (h as f32 - 1.0);
    let half = world_size * 0.5;

    let elev = |i: usize, j: usize| -> f32 { page.elevation(j * w + i) };

    let mut vertices = Vec::with_capacity(w * h * VERTEX_STRIDE);
    for j in 0..h {
        for i in 0..w {
            let px = -half + i as f32 * step_x;
            let pz = -half + j as f32 * step_z;
            let py = elev(i, j);

            // Normal from central differences (edges clamp to border samples).
            let hl = elev(i.saturating_sub(1), j);
            let hr = elev((i + 1).min(w - 1), j);
            let hd = elev(i, j.saturating_sub(1));
            let hu = elev(i, (j + 1).min(h - 1));
            let nx = hl - hr;
            let nz = hd - hu;
            let ny = step_x + step_z; // keep the up-component proportional to spacing
            let inv = 1.0 / (nx * nx + ny * ny + nz * nz).sqrt().max(1e-6);

            for v in [px, py, pz, nx * inv, ny * inv, nz * inv,
                      i as f32 / (w as f32 - 1.0), j as f32 / (h as f32 - 1.0)] {
                vertices.extend_from_slice(&v.to_le_bytes());
            }
        }
    }

    let mut indices = Vec::with_capacity((w - 1) * (h - 1) * 6 * 4);
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

    MeshData {
        vertex_count: (w * h) as u32,
        index_count: ((w - 1) * (h - 1) * 6) as u32,
        vertices,
        indices,
    }
}
