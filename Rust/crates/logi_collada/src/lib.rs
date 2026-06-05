//! Runtime **COLLADA (.dae)** decoding for the actorless 3D-Tiles stack (ADR-0009, Milestone F) — the
//! AGI_HQ sample building (user decision #3: a runtime loader, not an offline bake). Resolves the mesh
//! sources, de-indexes the triangle primitives, and interleaves them into our 32-byte vertex format
//! (`pos f3 | normal f3 | uv f2`), reported in **COLLADA space** plus the `up_axis` + `unit` metadata —
//! the geodetic/handedness mapping to ECEF happens at placement time, not here.
//!
//! v1 decodes geometry; the texture atlas is a known single file (`AGI_HQ_TextureAtlas0.jpg` from the
//! KMZ) handled at placement. Multiple `<triangles>`/geometries are concatenated; vertices are
//! de-indexed (no dedup) so indices are sequential.

#![forbid(unsafe_code)]

use dae_parser::{ArrayElement, Document, Geometry, Semantic, Source, Vertices};
use std::str::FromStr;

/// What went wrong decoding a COLLADA document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColladaError {
    Parse(String),
    /// No geometry with a triangulated mesh was found.
    NoGeometry,
    /// A `<triangles>` referenced a source/vertices that couldn't be resolved.
    BadSource,
}

/// COLLADA's up axis (right-handed). Placement maps this (+ handedness) into ECEF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpAxis {
    X,
    Y,
    Z,
}

/// A decoded COLLADA mesh in COLLADA coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct ColladaMesh {
    /// Interleaved vertices, 32 bytes each: position `f32×3`, normal `f32×3`, uv `f32×2`.
    pub vertices: Vec<u8>,
    pub indices: Vec<u32>,
    pub up_axis: UpAxis,
    /// Metres per document unit (COLLADA `<unit meter=…>`).
    pub unit_meters: f32,
}

fn interleave(positions: &[[f32; 3]], normals: &[[f32; 3]], uvs: &[[f32; 2]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(positions.len() * 32);
    for (i, p) in positions.iter().enumerate() {
        let n = normals.get(i).copied().unwrap_or([0.0, 0.0, 1.0]);
        let uv = uvs.get(i).copied().unwrap_or([0.0, 0.0]);
        for f in p.iter().chain(n.iter()).chain(uv.iter()) {
            out.extend_from_slice(&f.to_le_bytes());
        }
    }
    out
}

fn read_n(source: &Source, idx: usize, n: usize) -> [f32; 3] {
    let stride = source.accessor.stride;
    let vals: &[f32] = match &source.array {
        Some(ArrayElement::Float(fa)) => &fa.val,
        _ => &[],
    };
    // Include the accessor's `offset` (index of the first component within each stride block): a tight
    // XYZ/ST source has offset 0, but an interleaved source doesn't. (We still take the first `n` params
    // after the offset — the standard exporter layout that AGI_HQ uses.)
    // Overflow-safe: `idx`/`stride`/`offset` come from attacker-controllable XML and could wrap `usize`
    // in release (silently reading a valid-but-wrong float). Treat any overflow as out-of-range → zeros,
    // matching the `.get()` miss path.
    let base = match idx.checked_mul(stride).and_then(|b| b.checked_add(source.accessor.offset)) {
        Some(b) => b,
        None => return [0.0; 3],
    };
    let mut out = [0.0f32; 3];
    for (k, slot) in out.iter_mut().enumerate().take(n) {
        *slot = base.checked_add(k).and_then(|i| vals.get(i)).copied().unwrap_or(0.0);
    }
    out
}

/// Decode a COLLADA document (its XML text) into a single concatenated mesh.
pub fn decode_collada(xml: &str) -> Result<ColladaMesh, ColladaError> {
    let doc = Document::from_str(xml).map_err(|e| ColladaError::Parse(format!("{e:?}")))?;
    let sources = doc.local_map::<Source>().map_err(|e| ColladaError::Parse(format!("{e:?}")))?;
    let vertices = doc.local_map::<Vertices>().map_err(|e| ColladaError::Parse(format!("{e:?}")))?;

    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();

    for geom in doc.iter::<Geometry>() {
        let mesh = match geom.element.as_mesh() {
            Some(m) => m,
            None => continue,
        };
        for prim in &mesh.elements {
            let tris = match prim.as_triangles() {
                Some(t) => t,
                None => continue, // polylist/lines/etc. unsupported in v1
            };
            let data = match tris.data.prim.as_deref() {
                Some(d) if !d.is_empty() => d,
                _ => continue,
            };
            let stride = tris.inputs.stride;

            // Locate the inputs we care about (VERTEX → position, plus optional NORMAL / TEXCOORD).
            let mut v_off = None;
            let mut n_src_off: Option<(&Source, usize)> = None;
            let mut t_src_off: Option<(&Source, usize)> = None;
            let mut pos_source: Option<&Source> = None;
            for input in tris.inputs.iter() {
                let off = input.offset as usize;
                match input.semantic {
                    Semantic::Vertex => {
                        let verts = vertices.get_raw(&input.source).ok_or(ColladaError::BadSource)?;
                        let pos_input = verts.position_input();
                        pos_source =
                            Some(sources.get_raw(&pos_input.source).ok_or(ColladaError::BadSource)?);
                        v_off = Some(off);
                    }
                    Semantic::Normal => {
                        let s = sources.get_raw(&input.source).ok_or(ColladaError::BadSource)?;
                        n_src_off = Some((s, off));
                    }
                    Semantic::TexCoord => {
                        let s = sources.get_raw(&input.source).ok_or(ColladaError::BadSource)?;
                        t_src_off = Some((s, off));
                    }
                    _ => {}
                }
            }
            let (v_off, pos_source) = match (v_off, pos_source) {
                (Some(o), Some(s)) => (o, s),
                _ => continue,
            };

            for tuple in 0..(data.len() / stride) {
                let base = tuple * stride;
                let p_idx = data[base + v_off] as usize;
                positions.push(read_n(pos_source, p_idx, 3));
                if let Some((s, off)) = n_src_off {
                    let i = data[base + off] as usize;
                    normals.push(read_n(s, i, 3));
                } else {
                    normals.push([0.0, 0.0, 1.0]);
                }
                if let Some((s, off)) = t_src_off {
                    let i = data[base + off] as usize;
                    let uv = read_n(s, i, 2);
                    uvs.push([uv[0], uv[1]]);
                } else {
                    uvs.push([0.0, 0.0]);
                }
            }
        }
    }

    if positions.is_empty() {
        return Err(ColladaError::NoGeometry);
    }

    let up_axis = match doc.asset.up_axis {
        dae_parser::UpAxis::XUp => UpAxis::X,
        dae_parser::UpAxis::YUp => UpAxis::Y,
        dae_parser::UpAxis::ZUp => UpAxis::Z,
    };
    let unit_meters = doc.asset.unit.meter;
    let indices = (0..positions.len() as u32).collect();
    let vertices_buf = interleave(&positions, &normals, &uvs);

    Ok(ColladaMesh { vertices: vertices_buf, indices, up_axis, unit_meters })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRIANGLE_DAE: &str = r##"<?xml version="1.0" encoding="utf-8"?>
<COLLADA xmlns="http://www.collada.org/2005/11/COLLADASchema" version="1.4.1">
  <asset><created>1970-01-01T00:00:00Z</created><modified>1970-01-01T00:00:00Z</modified><unit name="meter" meter="1"/><up_axis>Z_UP</up_axis></asset>
  <library_geometries>
    <geometry id="g" name="g">
      <mesh>
        <source id="pos">
          <float_array id="pos-a" count="9">0 0 0 1 0 0 0 1 0</float_array>
          <technique_common><accessor source="#pos-a" count="3" stride="3">
            <param name="X" type="float"/><param name="Y" type="float"/><param name="Z" type="float"/>
          </accessor></technique_common>
        </source>
        <source id="nrm">
          <float_array id="nrm-a" count="9">0 0 1 0 0 1 0 0 1</float_array>
          <technique_common><accessor source="#nrm-a" count="3" stride="3">
            <param name="X" type="float"/><param name="Y" type="float"/><param name="Z" type="float"/>
          </accessor></technique_common>
        </source>
        <source id="uv">
          <float_array id="uv-a" count="6">0 0 1 0 0 1</float_array>
          <technique_common><accessor source="#uv-a" count="3" stride="2">
            <param name="S" type="float"/><param name="T" type="float"/>
          </accessor></technique_common>
        </source>
        <vertices id="v"><input semantic="POSITION" source="#pos"/></vertices>
        <triangles count="1" material="m">
          <input semantic="VERTEX" source="#v" offset="0"/>
          <input semantic="NORMAL" source="#nrm" offset="1"/>
          <input semantic="TEXCOORD" source="#uv" offset="2"/>
          <p>0 0 0 1 1 1 2 2 2</p>
        </triangles>
      </mesh>
    </geometry>
  </library_geometries>
</COLLADA>"##;

    #[test]
    fn decodes_triangle_with_vertex_indirection() {
        let m = decode_collada(TRIANGLE_DAE).expect("decode");
        assert_eq!(m.up_axis, UpAxis::Z);
        assert!((m.unit_meters - 1.0).abs() < 1e-6);
        assert_eq!(m.indices, vec![0, 1, 2]);
        assert_eq!(m.vertices.len(), 3 * 32);

        let f = |v: &[u8], o: usize| f32::from_le_bytes([v[o], v[o + 1], v[o + 2], v[o + 3]]);
        let v0 = &m.vertices[0..32];
        assert_eq!([f(v0, 0), f(v0, 4), f(v0, 8)], [0.0, 0.0, 0.0]); // pos via VERTEX→POSITION
        assert_eq!([f(v0, 12), f(v0, 16), f(v0, 20)], [0.0, 0.0, 1.0]); // normal
        assert_eq!([f(v0, 24), f(v0, 28)], [0.0, 0.0]); // uv
        let v1 = &m.vertices[32..64];
        assert_eq!([f(v1, 0), f(v1, 4), f(v1, 8)], [1.0, 0.0, 0.0]); // 2nd pos
        let v2 = &m.vertices[64..96];
        assert_eq!([f(v2, 24), f(v2, 28)], [0.0, 1.0]); // 3rd uv
    }

    #[test]
    fn empty_document_errors() {
        let empty = r#"<?xml version="1.0"?><COLLADA xmlns="http://www.collada.org/2005/11/COLLADASchema" version="1.4.1"><asset><created>1970-01-01T00:00:00Z</created><modified>1970-01-01T00:00:00Z</modified><up_axis>Y_UP</up_axis></asset></COLLADA>"#;
        assert!(matches!(decode_collada(empty), Err(ColladaError::NoGeometry)));
    }
}
