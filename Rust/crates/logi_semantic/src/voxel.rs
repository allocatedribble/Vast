//! WIP / TODO — the **voxel terrain lane** (NOT built; behind the off-by-default `voxel` cargo feature).
//!
//! # Why this exists (and why it's empty)
//!
//! LogiUE's terrain is **heightfield** by deliberate decision (real-world DEM/LiDAR is 2.5D; surface
//! logistics needs cheap heightfield collision; planet scale needs 2D memory; the actorless Rust kernel +
//! Nanite lane fit heightfields). See the terrain-representation research / ADR-0012 (to be written).
//!
//! Heightfields **cannot** represent caves, overhangs, undercut cliffs, tunnels, or *destructible /
//! dig-and-build* terrain. If a future gameplay need demands that — and ONLY then — this lane gets built.
//! It is a **Rust-owned** lane that ships its meshed output through the same actorless frame packet as a
//! normal tile mesh (`TerrainMesh`). It is explicitly **NOT** the Voxel Plugin: that is `AVoxelWorld`
//! actor-based, targets UE 5.6/5.7 (we are 5.8-only, ADR-0004), and would own terrain truth in C++
//! (violating ADR-0005/0007). We borrow its *techniques* (dual contouring, octree LOD), never the plugin.
//!
//! # Intended design (when implemented)
//!
//! 1. **Representation** — a sparse octree of voxel chunks, each holding a **signed distance field** (SDF)
//!    sampled on a grid (negative inside, positive outside). Sourced from: an analytic SDF (procedural),
//!    CSG of brushes (editing), or a voxelized mesh. Only chunks near the surface/edits are resident.
//! 2. **Meshing** — **dual contouring** (DC): one vertex per cell that contains a sign change, placed by
//!    minimizing the QEF of the hermite (position+normal) edge intersections — DC preserves sharp features
//!    and handles octree LOD across differently-sized leaves **without** transition stitching (unlike
//!    Marching Cubes, which needs the **Transvoxel** algorithm to seam LODs). Output is the engine's
//!    32-byte vertex contract → [`TerrainMesh`], so the render/FFI path is unchanged.
//! 3. **LOD** — per-chunk by camera distance over the octree; DC's adaptive leaves give seamless joins.
//! 4. **Edits** — CSG (sphere/box/mesh brushes) mutate the SDF of touched chunks; only those re-mesh
//!    (the dig/build loop). Deterministic + seeded, like the rest of the bake.
//! 5. **Collision / gameplay** — promote to the coarse deterministic representation (ADR-0007): the sim
//!    never reads raw voxel geometry across the FFI; a coarse surface/occupancy proxy is resolved first.
//!
//! # References
//! - Dual Contouring of Hermite Data (Ju, Losasso, Schaefer, Warren).
//! - The Transvoxel Algorithm (Lengyel) — MC LOD seams, the alternative if DC is dropped.
//! - Voxel Plugin source — technique reference only (octree, chunk streaming, GPU meshing).

#![allow(dead_code, unused_variables)]

use crate::mesh::TerrainMesh;

/// Status of the voxel lane. Returned by every entry point until the lane is implemented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoxelError {
    /// The voxel lane is a WIP skeleton — see the module docs / ADR-0012.
    NotImplemented,
}

/// A signed-distance field: negative inside the surface, positive outside, ~unit-gradient near it.
pub trait SignedDistanceField {
    /// SDF value at a world position (metres).
    fn distance(&self, x: f32, y: f32, z: f32) -> f32;
}

/// One resident voxel chunk: a cubic SDF grid (`res³` samples) anchored at `origin` (ENU metres) with the
/// given metric `size`. `lod` is its octree depth.
pub struct VoxelChunk {
    pub origin: [f32; 3],
    pub size_m: f32,
    pub res: u32,
    pub lod: u8,
    /// `res³` signed distances (metres), z-major then y then x. Empty in the skeleton.
    pub sdf: Vec<f32>,
}

impl VoxelChunk {
    /// Allocate an empty chunk (no SDF sampled yet).
    pub fn empty(origin: [f32; 3], size_m: f32, res: u32, lod: u8) -> Self {
        Self {
            origin,
            size_m,
            res,
            lod,
            sdf: Vec::new(),
        }
    }

    /// TODO: sample `field` into `self.sdf` over the chunk grid.
    pub fn sample_from(&mut self, _field: &dyn SignedDistanceField) -> Result<(), VoxelError> {
        Err(VoxelError::NotImplemented)
    }
}

/// TODO: dual-contour a chunk's SDF into the engine vertex contract (ENU metres), ready for `to_ue_cm` +
/// the frame packet — exactly like a heightfield [`TerrainMesh`].
pub fn dual_contour(_chunk: &VoxelChunk) -> Result<TerrainMesh, VoxelError> {
    // Per-cell QEF vertex placement + quad emission across sign-changing edges. See module docs.
    Err(VoxelError::NotImplemented)
}

/// TODO: apply a CSG brush edit to the chunks it touches (the dig/build loop) and return the dirty set.
pub fn apply_csg_edit(_chunks: &mut [VoxelChunk], _brush: &dyn SignedDistanceField) -> Result<(), VoxelError> {
    Err(VoxelError::NotImplemented)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_compiles_and_reports_not_implemented() {
        let mut chunk = VoxelChunk::empty([0.0; 3], 64.0, 16, 0);
        assert!(chunk.sdf.is_empty());
        assert_eq!(chunk.sample_from(&FlatGround), Err(VoxelError::NotImplemented));
        assert_eq!(dual_contour(&chunk).err(), Some(VoxelError::NotImplemented));
    }

    struct FlatGround;
    impl SignedDistanceField for FlatGround {
        fn distance(&self, _x: f32, _y: f32, z: f32) -> f32 {
            z // ground plane at z = 0
        }
    }
}
