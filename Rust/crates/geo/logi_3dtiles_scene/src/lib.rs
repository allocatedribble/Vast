//! The 3D-Tiles **scene** crate (ADR-0009, Milestone F). M1 = the AGI_HQ sample building: decode the
//! `.dae`, transform it into Unreal render space (cm, Z-up left-handed), and pass the texture through for
//! the C++ side to upload. The geodetic ECEF origin is computed and exposed for globe placement (M2+);
//! M1 renders the building at the local origin to prove the texture + mesh path.
//!
//! This grows into the full streaming scene (select → io → decode → cache → frame packet, with the
//! camera-tracking RTC rebase) once the M1 vertical slice renders.

#![forbid(unsafe_code)]

pub mod streaming;
pub use streaming::{GlobeFrame, RenderTileInfo, StreamError, TileMesh, TilesetWorld};

use logi_collada::{decode_collada, ColladaError};
use logi_geospatial::{Cartographic, Ellipsoid};

/// Where a model sits on the globe (KML/COLLADA placement), in degrees + metres.
#[derive(Clone, Copy, Debug)]
pub struct Placement {
    pub longitude_deg: f64,
    pub latitude_deg: f64,
    pub height_m: f64,
    pub heading_deg: f64,
}

/// A building ready for the renderer: interleaved 32-byte vertices in **Unreal space** (cm, Z-up LH) +
/// indices, the raw texture bytes (decoded C++-side), a bounding sphere, and the WGS84 ECEF origin.
#[derive(Clone, Debug)]
pub struct BuildingMesh {
    pub vertices: Vec<u8>,
    pub indices: Vec<u32>,
    pub texture_jpeg: Vec<u8>,
    pub bounds_center: [f32; 3],
    pub bounds_radius: f32,
    /// The building's WGS84 ECEF position (metres) — for globe placement once the globe lane lands.
    pub ecef_origin: [f64; 3],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SceneError {
    Collada(ColladaError),
}

#[inline]
fn rd(b: &[u8], o: usize) -> f32 {
    f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
#[inline]
fn wr(b: &mut [u8], o: usize, v: f32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

/// Load the AGI_HQ-style building: decode the COLLADA `dae_xml`, scale to cm, convert COLLADA (Z-up
/// right-handed) → Unreal (Z-up left-handed) by negating Y + flipping triangle winding + flipping the V
/// texcoord, and carry the raw `jpg_bytes` through. `placement` gives the geodetic origin (used for the
/// ECEF, applied to the mesh once the globe lane exists).
pub fn load_building(dae_xml: &str, jpg_bytes: Vec<u8>, placement: &Placement) -> Result<BuildingMesh, SceneError> {
    let mesh = decode_collada(dae_xml).map_err(SceneError::Collada)?;
    let scale = mesh.unit_meters * 100.0; // document units → metres → cm

    let mut vertices = mesh.vertices;
    let vcount = vertices.len() / 32;
    let mut mn = [f32::MAX; 3];
    let mut mx = [f32::MIN; 3];
    for i in 0..vcount {
        let b = i * 32;
        // Read first (immutable borrow), then write back — Z-up preserved, Y negated for the handedness flip.
        let px = rd(&vertices, b) * scale;
        let py = -rd(&vertices, b + 4) * scale;
        let pz = rd(&vertices, b + 8) * scale;
        let ny = -rd(&vertices, b + 16); // normal Y
        let v = 1.0 - rd(&vertices, b + 28); // V texcoord (COLLADA bottom-left → UE top-left)
        wr(&mut vertices, b, px);
        wr(&mut vertices, b + 4, py);
        wr(&mut vertices, b + 8, pz);
        wr(&mut vertices, b + 16, ny);
        wr(&mut vertices, b + 28, v);
        for (k, val) in [px, py, pz].iter().enumerate() {
            mn[k] = mn[k].min(*val);
            mx[k] = mx[k].max(*val);
        }
    }

    // The Y-negation above is a reflection (negative-determinant), which ALREADY flips triangle winding
    // into Unreal's clockwise-from-front convention — exactly like the terrain lane's Y<->Z swap. Do NOT
    // also reverse the indices, or the double-flip makes every face back-facing and the whole mesh is
    // culled (renders nothing in solid mode, visible only in wireframe).
    let indices = mesh.indices;

    let bounds_center = [(mn[0] + mx[0]) * 0.5, (mn[1] + mx[1]) * 0.5, (mn[2] + mx[2]) * 0.5];
    let bounds_radius = if vcount == 0 {
        0.0
    } else {
        0.5 * ((mx[0] - mn[0]).powi(2) + (mx[1] - mn[1]).powi(2) + (mx[2] - mn[2]).powi(2)).sqrt()
    };

    let ecef = Ellipsoid::wgs84().cartographic_to_cartesian(Cartographic::from_degrees(
        placement.longitude_deg,
        placement.latitude_deg,
        placement.height_m,
    ));

    Ok(BuildingMesh {
        vertices,
        indices,
        texture_jpeg: jpg_bytes,
        bounds_center,
        bounds_radius,
        ecef_origin: [ecef.x, ecef.y, ecef.z],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRIANGLE_DAE: &str = r##"<?xml version="1.0" encoding="utf-8"?>
<COLLADA xmlns="http://www.collada.org/2005/11/COLLADASchema" version="1.4.1">
  <asset><created>1970-01-01T00:00:00Z</created><modified>1970-01-01T00:00:00Z</modified><unit name="meter" meter="1"/><up_axis>Z_UP</up_axis></asset>
  <library_geometries>
    <geometry id="g" name="g"><mesh>
      <source id="pos"><float_array id="pos-a" count="9">0 0 0 1 0 0 0 1 0</float_array>
        <technique_common><accessor source="#pos-a" count="3" stride="3">
          <param name="X" type="float"/><param name="Y" type="float"/><param name="Z" type="float"/>
        </accessor></technique_common></source>
      <source id="uv"><float_array id="uv-a" count="6">0 0 1 0 0 1</float_array>
        <technique_common><accessor source="#uv-a" count="3" stride="2">
          <param name="S" type="float"/><param name="T" type="float"/>
        </accessor></technique_common></source>
      <vertices id="v"><input semantic="POSITION" source="#pos"/></vertices>
      <triangles count="1" material="m">
        <input semantic="VERTEX" source="#v" offset="0"/>
        <input semantic="TEXCOORD" source="#uv" offset="1"/>
        <p>0 0 1 1 2 2</p>
      </triangles>
    </mesh></geometry>
  </library_geometries>
</COLLADA>"##;

    #[test]
    fn transforms_into_unreal_space() {
        let tex = vec![1u8, 2, 3, 4];
        let m = load_building(
            TRIANGLE_DAE,
            tex.clone(),
            &Placement {
                longitude_deg: -75.5967,
                latitude_deg: 40.0387,
                height_m: 50.0,
                heading_deg: 90.0,
            },
        )
        .expect("load");

        assert_eq!(m.texture_jpeg, tex); // passed through
        assert_eq!(m.vertices.len(), 3 * 32);
        // winding preserved (the Y-flip reflection handles UE's convention; no index reversal)
        assert_eq!(m.indices, vec![0, 1, 2]);

        let rd =
            |o: usize| f32::from_le_bytes([m.vertices[o], m.vertices[o + 1], m.vertices[o + 2], m.vertices[o + 3]]);
        // vertex 0 = (0,0,0) -> (0,0,0); V flips 0 -> 1
        assert_eq!([rd(0), rd(4), rd(8)], [0.0, 0.0, 0.0]);
        assert!((rd(28) - 1.0).abs() < 1e-6);
        // vertex 1 = (1,0,0) -> ×100, Y negated -> (100,0,0)
        assert_eq!([rd(32), rd(36), rd(40)], [100.0, 0.0, 0.0]);
        // vertex 2 = (0,1,0) -> (0,-100,0)
        assert_eq!([rd(64), rd(68), rd(72)], [0.0, -100.0, 0.0]);

        // ECEF origin is on the globe near the AGI HQ (~Earth radius from geocentre).
        let r = (m.ecef_origin[0].powi(2) + m.ecef_origin[1].powi(2) + m.ecef_origin[2].powi(2)).sqrt();
        assert!((r - 6_369_000.0).abs() < 30_000.0, "ecef radius {r}");
    }
}
