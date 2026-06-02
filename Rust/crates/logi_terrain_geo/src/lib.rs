//! Mesh/geometry + vertex layout.
//!
//! Vertex layout (interleaved, 32 bytes): position `float3`, normal `float3`, uv `float2`.
//! Phases 1+ add decoded quantized-mesh patches behind the same byte contract.

/// Interleaved vertex stride in bytes: pos(12) + normal(12) + uv(8).
pub const VERTEX_STRIDE: usize = 32;

pub struct MeshData {
    pub vertices: Vec<u8>,
    pub indices: Vec<u8>,
    pub vertex_count: u32,
    pub index_count: u32,
}

/// Build a flat `cols x rows`-quad grid centered at the origin, spanning `width x depth` world units.
/// `u32` indices, wound to face +Y.
pub fn build_grid(cols: u32, rows: u32, width: f32, depth: f32) -> MeshData {
    let vcols = cols + 1;
    let vrows = rows + 1;

    let mut vertices: Vec<u8> = Vec::with_capacity((vcols * vrows) as usize * VERTEX_STRIDE);
    for z in 0..vrows {
        for x in 0..vcols {
            let u = x as f32 / cols as f32;
            let v = z as f32 / rows as f32;
            let px = (u - 0.5) * width;
            let pz = (v - 0.5) * depth;

            vertices.extend_from_slice(&px.to_le_bytes());
            vertices.extend_from_slice(&0.0f32.to_le_bytes());
            vertices.extend_from_slice(&pz.to_le_bytes());
            vertices.extend_from_slice(&0.0f32.to_le_bytes());
            vertices.extend_from_slice(&1.0f32.to_le_bytes());
            vertices.extend_from_slice(&0.0f32.to_le_bytes());
            vertices.extend_from_slice(&u.to_le_bytes());
            vertices.extend_from_slice(&v.to_le_bytes());
        }
    }

    let mut indices: Vec<u8> = Vec::with_capacity((cols * rows * 6) as usize * 4);
    for z in 0..rows {
        for x in 0..cols {
            let i0 = z * vcols + x;
            let i1 = i0 + 1;
            let i2 = i0 + vcols;
            let i3 = i2 + 1;
            for idx in [i0, i2, i1, i1, i2, i3] {
                indices.extend_from_slice(&idx.to_le_bytes());
            }
        }
    }

    MeshData {
        vertex_count: vcols * vrows,
        index_count: cols * rows * 6,
        vertices,
        indices,
    }
}
