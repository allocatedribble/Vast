//! glTF 2.0 / GLB decoding for the actorless 3D-Tiles stack (ADR-0009, Milestone F). 3D Tiles 1.1 ships
//! tile content as glTF, so this turns a tile's GLB into our interleaved 32-byte vertex format
//! (`pos f3 | normal f3 | uv f2`) + indices + the base-color material/texture, ready for the kernel's
//! ECEF rebase + the C++ texture path.
//!
//! v1: uncompressed glTF (the `gltf` crate). **Deferred (the C++-vendoring / external boundary):** Draco
//! (`KHR_draco_mesh_compression` — a `cc`-built shim), KTX2/Basis transcode, meshopt. Textures are
//! returned as RAW bytes (JPEG/PNG/KTX2) — decoding/transcoding happens on the renderer side.

#![forbid(unsafe_code)]

/// What went wrong decoding a glTF/GLB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GltfError {
    Parse(String),
    /// A GLB primitive/image referenced binary data but the GLB had no embedded blob.
    MissingBlob,
    BadBufferView,
}

/// One decoded primitive: interleaved 32-byte vertices + u32 indices + its base-color material.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedPrimitive {
    /// Interleaved vertices, 32 bytes each: position `f32×3`, normal `f32×3`, uv `f32×2`.
    pub vertices: Vec<u8>,
    pub indices: Vec<u32>,
    pub base_color_factor: [f32; 4],
    /// Index into [`DecodedGltf::images`] of the base-color (albedo) texture, if any.
    pub base_color_texture: Option<usize>,
    /// The owning glTF node's WORLD transform (column-major 4×4), accumulated down the scene graph.
    /// glTF vertices are node-local; this places them in the glTF scene frame. Google P3DT bakes each
    /// tile's full **ECEF placement** (translation ≈ Earth radius) into this transform, so the renderer
    /// MUST apply it before any ECEF→local rebase — dropping it collapses every tile to the geocenter.
    pub node_transform: [[f32; 4]; 4],
}

/// A texture image as it sits in the glTF — raw encoded bytes (decode on the renderer side).
#[derive(Clone, Debug, PartialEq)]
pub struct EmbeddedImage {
    pub mime_type: String,
    /// Raw image bytes (e.g. JPEG/PNG/KTX2). Empty for an external/URI image (deferred).
    pub data: Vec<u8>,
}

/// A decoded glTF: all primitives across all meshes, plus the texture images.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DecodedGltf {
    pub primitives: Vec<DecodedPrimitive>,
    pub images: Vec<EmbeddedImage>,
}

/// Interleave attribute arrays into the 32-byte vertex layout. A missing normal defaults to `+Z` and a
/// missing uv to `(0,0)`.
pub fn interleave(positions: &[[f32; 3]], normals: Option<&[[f32; 3]]>, uvs: Option<&[[f32; 2]]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(positions.len() * 32);
    for (i, p) in positions.iter().enumerate() {
        let n = normals.and_then(|a| a.get(i)).copied().unwrap_or([0.0, 0.0, 1.0]);
        let uv = uvs.and_then(|a| a.get(i)).copied().unwrap_or([0.0, 0.0]);
        for f in p.iter().chain(n.iter()).chain(uv.iter()) {
            out.extend_from_slice(&f.to_le_bytes());
        }
    }
    out
}

/// Decode the primitives of one mesh under a node's world transform, appending to `out`.
fn decode_mesh_primitives(
    mesh: gltf::Mesh,
    node_transform: [[f32; 4]; 4],
    blob: Option<&[u8]>,
    out: &mut Vec<DecodedPrimitive>,
) {
    for prim in mesh.primitives() {
        // Skip a count==0 POSITION accessor (valid per glTF validation, but it underflows the `gltf`
        // reader's range math → a debug-build panic).
        if prim.get(&gltf::Semantic::Positions).is_none_or(|a| a.count() == 0) {
            continue;
        }
        // Resolve buffer data by source: a GLB keeps everything in the single BIN blob; external /
        // data-URI buffers (a multi-buffer glTF) aren't fetched here, so return None rather than
        // mis-slicing the blob at some other buffer's offsets.
        let reader = prim.reader(|buffer| match buffer.source() {
            gltf::buffer::Source::Bin => blob,
            gltf::buffer::Source::Uri(_) => None,
        });
        let positions: Vec<[f32; 3]> = match reader.read_positions() {
            Some(it) => it.collect(),
            None => continue, // a primitive with no positions isn't renderable
        };
        let normals: Option<Vec<[f32; 3]>> = reader.read_normals().map(|it| it.collect());
        let uvs: Option<Vec<[f32; 2]>> = reader.read_tex_coords(0).map(|tc| tc.into_f32().collect());
        let indices: Vec<u32> = match reader.read_indices() {
            Some(ri) => ri.into_u32().collect(),
            None => (0..positions.len() as u32).collect(), // non-indexed → sequential
        };
        let vertices = interleave(&positions, normals.as_deref(), uvs.as_deref());

        let pbr = prim.material().pbr_metallic_roughness();
        out.push(DecodedPrimitive {
            vertices,
            indices,
            base_color_factor: pbr.base_color_factor(),
            base_color_texture: pbr.base_color_texture().map(|info| info.texture().source().index()),
            node_transform,
        });
    }
}

/// Recursively walk a node, accumulating the world transform, and decode every mesh it carries.
fn walk_node(node: gltf::Node, parent_world: glam::Mat4, blob: Option<&[u8]>, out: &mut Vec<DecodedPrimitive>) {
    let world = parent_world * glam::Mat4::from_cols_array_2d(&node.transform().matrix());
    if let Some(mesh) = node.mesh() {
        decode_mesh_primitives(mesh, world.to_cols_array_2d(), blob, out);
    }
    for child in node.children() {
        walk_node(child, world, blob, out);
    }
}

/// Decode a GLB (binary glTF) — the form 3D Tiles content uses. Walks the scene graph so each primitive
/// carries its owning node's WORLD transform (3D Tiles / Google P3DT bake the tile's ECEF placement
/// there). A GLB with no scene/nodes (e.g. a bare mesh) falls back to identity-transform mesh iteration.
pub fn decode_glb(bytes: &[u8]) -> Result<DecodedGltf, GltfError> {
    let gltf = gltf::Gltf::from_slice(bytes).map_err(|e| GltfError::Parse(e.to_string()))?;
    let blob = gltf.blob.as_deref();

    let mut primitives = Vec::new();
    let scene = gltf.document.default_scene().or_else(|| gltf.document.scenes().next());
    if let Some(scene) = scene {
        for node in scene.nodes() {
            walk_node(node, glam::Mat4::IDENTITY, blob, &mut primitives);
        }
    } else {
        // No scene graph: decode every mesh with an identity transform (legacy / bare-mesh GLBs).
        for mesh in gltf.document.meshes() {
            decode_mesh_primitives(mesh, glam::Mat4::IDENTITY.to_cols_array_2d(), blob, &mut primitives);
        }
    }

    let mut images = Vec::new();
    for image in gltf.document.images() {
        let img = match image.source() {
            gltf::image::Source::View { view, mime_type } => {
                // Only slice the BIN blob when the image's bufferView actually lives in it; a view into a
                // different (URI) buffer isn't the blob, so defer it (empty) like the Uri arm below.
                match view.buffer().source() {
                    gltf::buffer::Source::Bin => {
                        let b = blob.ok_or(GltfError::MissingBlob)?;
                        let start = view.offset();
                        let end = start.checked_add(view.length()).ok_or(GltfError::BadBufferView)?;
                        EmbeddedImage {
                            mime_type: mime_type.to_string(),
                            data: b.get(start..end).ok_or(GltfError::BadBufferView)?.to_vec(),
                        }
                    }
                    gltf::buffer::Source::Uri(_) => EmbeddedImage {
                        mime_type: mime_type.to_string(),
                        data: Vec::new(),
                    },
                }
            }
            // External/data URIs are deferred (the IO layer would fetch/decode them).
            gltf::image::Source::Uri { mime_type, .. } => EmbeddedImage {
                mime_type: mime_type.unwrap_or_default().to_string(),
                data: Vec::new(),
            },
        };
        images.push(img);
    }

    Ok(DecodedGltf { primitives, images })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleave_packs_32_bytes_with_defaults() {
        let v = interleave(&[[1.0, 2.0, 3.0]], None, None);
        assert_eq!(v.len(), 32);
        let f = |o: usize| f32::from_le_bytes([v[o], v[o + 1], v[o + 2], v[o + 3]]);
        assert_eq!([f(0), f(4), f(8)], [1.0, 2.0, 3.0]);
        assert_eq!([f(12), f(16), f(20)], [0.0, 0.0, 1.0]); // default normal +Z
        assert_eq!([f(24), f(28)], [0.0, 0.0]); // default uv
    }

    /// Hand-assemble a minimal GLB: one triangle with POSITION/NORMAL/TEXCOORD_0 + indices + a material.
    fn build_test_glb() -> Vec<u8> {
        let positions: [[f32; 3]; 3] = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let normals: [[f32; 3]; 3] = [[0.0, 0.0, 1.0]; 3];
        let uvs: [[f32; 2]; 3] = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];
        let indices: [u16; 3] = [0, 1, 2];

        let mut bin = Vec::new();
        for p in positions {
            for v in p {
                bin.extend_from_slice(&v.to_le_bytes());
            }
        }
        let off_n = bin.len();
        for n in normals {
            for v in n {
                bin.extend_from_slice(&v.to_le_bytes());
            }
        }
        let off_uv = bin.len();
        for uv in uvs {
            for v in uv {
                bin.extend_from_slice(&v.to_le_bytes());
            }
        }
        let off_idx = bin.len();
        for i in indices {
            bin.extend_from_slice(&i.to_le_bytes());
        }
        while bin.len() % 4 != 0 {
            bin.push(0);
        }

        let json = format!(
            r#"{{"asset":{{"version":"2.0"}},
              "buffers":[{{"byteLength":{bl}}}],
              "bufferViews":[
                {{"buffer":0,"byteOffset":0,"byteLength":36}},
                {{"buffer":0,"byteOffset":{off_n},"byteLength":36}},
                {{"buffer":0,"byteOffset":{off_uv},"byteLength":24}},
                {{"buffer":0,"byteOffset":{off_idx},"byteLength":6}} ],
              "accessors":[
                {{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}},
                {{"bufferView":1,"componentType":5126,"count":3,"type":"VEC3"}},
                {{"bufferView":2,"componentType":5126,"count":3,"type":"VEC2"}},
                {{"bufferView":3,"componentType":5123,"count":3,"type":"SCALAR"}} ],
              "meshes":[{{"primitives":[{{"attributes":{{"POSITION":0,"NORMAL":1,"TEXCOORD_0":2}},"indices":3,"material":0}}]}}],
              "materials":[{{"pbrMetallicRoughness":{{"baseColorFactor":[1.0,0.5,0.25,1.0]}}}}]}}"#,
            bl = bin.len()
        );
        let mut json_bytes = json.into_bytes();
        while json_bytes.len() % 4 != 0 {
            json_bytes.push(b' ');
        }

        let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
        let mut glb = Vec::new();
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        glb.extend_from_slice(&0x4E4F_534Au32.to_le_bytes()); // "JSON"
        glb.extend_from_slice(&json_bytes);
        glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        glb.extend_from_slice(&0x004E_4942u32.to_le_bytes()); // "BIN\0"
        glb.extend_from_slice(&bin);
        glb
    }

    #[test]
    fn decodes_a_minimal_glb() {
        let d = decode_glb(&build_test_glb()).expect("decode");
        assert_eq!(d.primitives.len(), 1);
        let p = &d.primitives[0];
        assert_eq!(p.indices, vec![0, 1, 2]);
        assert_eq!(p.vertices.len(), 3 * 32);
        assert_eq!(p.base_color_factor, [1.0, 0.5, 0.25, 1.0]);
        assert_eq!(p.base_color_texture, None);

        let f = |v: &[u8], o: usize| f32::from_le_bytes([v[o], v[o + 1], v[o + 2], v[o + 3]]);
        let v0 = &p.vertices[0..32];
        assert_eq!([f(v0, 0), f(v0, 4), f(v0, 8)], [0.0, 0.0, 0.0]); // pos
        assert_eq!([f(v0, 12), f(v0, 16), f(v0, 20)], [0.0, 0.0, 1.0]); // normal
        assert_eq!([f(v0, 24), f(v0, 28)], [0.0, 0.0]); // uv
        let v1 = &p.vertices[32..64];
        assert_eq!([f(v1, 0), f(v1, 4), f(v1, 8)], [1.0, 0.0, 0.0]); // 2nd vertex pos
        let v2 = &p.vertices[64..96];
        assert_eq!([f(v2, 24), f(v2, 28)], [0.0, 1.0]); // 3rd vertex uv
    }
}
