//! glTF 2.0 / GLB decoding for the actorless 3D-Tiles stack (ADR-0009, Milestone F). 3D Tiles 1.1 ships
//! tile content as glTF, so this turns a tile's GLB into our interleaved 32-byte vertex format
//! (`pos f3 | normal f3 | uv f2`) + indices + the base-color material/texture, ready for the kernel's
//! ECEF rebase + the C++ texture path.
//!
//! Geometry decode covers both uncompressed glTF (the `gltf` crate) and Draco-compressed primitives
//! (`KHR_draco_mesh_compression`, via the vendored decode-only `logi_draco` shim) — Google P3DT and
//! Cesium fine-LOD tiles ship Draco bitstreams, so this is the path that yields their geometry.
//! **Deferred (the remaining C++-vendoring / external boundary):** KTX2/Basis transcode and meshopt.
//! Textures are returned as RAW bytes (JPEG/PNG/KTX2) — decoding/transcoding happens on the renderer side.

#![forbid(unsafe_code)]

/// What went wrong decoding a glTF/GLB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GltfError {
    Parse(String),
    /// A GLB primitive/image referenced binary data but the GLB had no embedded blob.
    MissingBlob,
    BadBufferView,
}

/// One decoded primitive: raw vertex attribute arrays + u32 indices + its base-color material.
///
/// Attributes are kept **un-interleaved** (audit D2): the 32-byte interleave is FUSED into the ENU rebase
/// (`logi_3dtiles_scene::rebase_gltf`) so each vertex is interleaved exactly once — *after* its transform —
/// instead of being interleaved here and then read back byte-by-byte there. A missing normal defaults to `+Z`
/// and a missing uv to `(0, 0)`, per-vertex (matching `logi_geo_mesh::interleave`'s prior behavior).
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedPrimitive {
    pub positions: Vec<[f32; 3]>,
    pub normals: Option<Vec<[f32; 3]>>,
    pub uvs: Option<Vec<[f32; 2]>>,
    pub indices: Vec<u32>,
    pub base_color_factor: [f32; 4],
    /// Index into [`DecodedGltf::images`] of the base-color (albedo) texture, if any.
    pub base_color_texture: Option<usize>,
    /// `KHR_materials_unlit`: the material bakes its own lighting (no engine shading). Google P3DT bakes
    /// real-world sun/shadow into the albedo and marks every material unlit (it is in P3DT's
    /// `extensionsRequired`), so the renderer must shade these surfaces emissive/unlit — applying scene
    /// lighting on top double-lights them. Read from `material.unlit()` (the `KHR_materials_unlit` glTF
    /// feature). False for an ordinary lit PBR material.
    pub unlit: bool,
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

/// The 32-byte interleave lives in its one home, [`logi_geo_mesh`] (AGENTS.md §1A); re-exported so
/// `logi_gltf::interleave` stays a stable entry point for callers.
pub use logi_geo_mesh::interleave;

/// Hostile-input bounds. Tile content arrives from untrusted network sources (ion / Google P3DT), and the
/// release build is `panic = "abort"`, so a panic/overflow/stack-overflow here would abort the whole UE
/// process — these caps turn "absurd input" into a dropped primitive instead.
/// Max scene-graph recursion depth (real glTF node trees are shallow; a deep hostile chain must not
/// overflow the worker stack — uncatchable by the FFI guards).
const MAX_NODE_DEPTH: u32 = 256;
/// Per-primitive vertex/index ceilings. The fetch layer caps a tile at 64 MiB, so a legitimate primitive is
/// far under these; anything larger is corrupt/hostile (OOM guard).
const MAX_VERTICES_PER_PRIMITIVE: usize = 1 << 23; // ~8.4M (> a 64 MiB all-positions tile)
const MAX_INDICES_PER_PRIMITIVE: usize = 1 << 24; // ~16.8M

/// Decode the primitives of one mesh under a node's world transform, appending to `out`.
fn decode_mesh_primitives(
    mesh: gltf::Mesh,
    node_transform: [[f32; 4]; 4],
    blob: Option<&[u8]>,
    doc: &gltf::Document,
    out: &mut Vec<DecodedPrimitive>,
) {
    for prim in mesh.primitives() {
        // KHR_draco_mesh_compression (audit D1 / ADR-0009): Google P3DT + Cesium fine-LOD tiles ship their
        // geometry as a Draco bitstream — the standard accessor reader can't see it (the attributes carry no
        // bufferView), so without this branch the primitive decodes to nothing and the planet stays coarse.
        // Decode the Draco buffer into the SAME attribute arrays the uncompressed path produces.
        if let Some(ext) = prim.extension_value("KHR_draco_mesh_compression") {
            // A closure so `?` short-circuits a malformed extension to "drop this primitive" (like the
            // uncompressed drop paths below) without panicking on hostile bytes.
            let decoded = (|| {
                let attrs = ext.get("attributes")?;
                let draco_id = |n: &str| attrs.get(n).and_then(|v| v.as_u64()).map(|u| u as u32);
                let ids = logi_draco::DracoAttributeIds {
                    position: draco_id("POSITION")?, // required; no POSITION ⇒ not renderable
                    normal: draco_id("NORMAL"),
                    texcoord: draco_id("TEXCOORD_0"),
                };
                // Slice the Draco bitstream from the GLB blob via the extension's bufferView (BIN buffer only,
                // like the image path — a view into an external/URI buffer isn't the blob).
                let bv = ext.get("bufferView").and_then(|v| v.as_u64())? as usize;
                let view = doc.views().nth(bv)?;
                if !matches!(view.buffer().source(), gltf::buffer::Source::Bin) {
                    return None;
                }
                let start = view.offset();
                let end = start.checked_add(view.length())?;
                let drc = blob?.get(start..end)?;
                // `decode_mesh` bounds + range-checks (DecodeLimits = our per-primitive caps); never panics.
                let m = logi_draco::decode_mesh(drc, &ids, logi_draco::DecodeLimits::default()).ok()?;
                let mat = prim.material();
                let pbr = mat.pbr_metallic_roughness();
                Some(DecodedPrimitive {
                    positions: m.positions,
                    normals: m.normals,
                    uvs: m.texcoords,
                    indices: m.indices,
                    base_color_factor: pbr.base_color_factor(),
                    base_color_texture: pbr.base_color_texture().map(|info| info.texture().source().index()),
                    unlit: mat.unlit(),
                    node_transform,
                })
            })();
            if let Some(p) = decoded {
                out.push(p);
            }
            continue; // Draco primitive handled (decoded or dropped) — never fall through to the accessor reader
        }
        // Drop a primitive whose POSITION is absent, empty, or absurdly large: a count==0 accessor
        // underflows the `gltf` reader's range math (debug panic), and an absurd count is an OOM vector.
        match prim.get(&gltf::Semantic::Positions) {
            Some(a) if (1..=MAX_VERTICES_PER_PRIMITIVE).contains(&a.count()) => {}
            _ => continue,
        }
        // A zero-count NORMAL/TEXCOORD/index accessor underflows the same reader range math — treat such a
        // primitive as malformed (generalises the POSITION guard to every accessor we actually read).
        let zero_count = |s| prim.get(s).is_some_and(|a| a.count() == 0);
        // Bound the index accessor's DECLARED count too, before it drives an allocation. `count()` reads
        // only the accessor JSON (no buffer touch), so an absurd `count` is rejected here rather than after
        // `into_u32().collect()` has already allocated a ~count-sized `Vec<u32>` — closing that OOM window
        // for a crafted index count. The realized-length `v.len() > MAX` check below stays as defence (it
        // also covers any reader/sparse expansion past the declared count).
        let indices_count_bad = prim
            .indices()
            .is_some_and(|a| a.count() == 0 || a.count() > MAX_INDICES_PER_PRIMITIVE);
        if zero_count(&gltf::Semantic::Normals) || zero_count(&gltf::Semantic::TexCoords(0)) || indices_count_bad {
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
            Some(ri) => {
                let v: Vec<u32> = ri.into_u32().collect();
                // Reject out-of-range indices (a hostile tile's OOB index → out-of-bounds GPU vertex fetch
                // / driver UB) and absurd index counts (OOM). Drop the whole primitive rather than hand the
                // renderer a corrupt index buffer. O(indices) — the pass we already do.
                if v.len() > MAX_INDICES_PER_PRIMITIVE || v.iter().any(|&i| i as usize >= positions.len()) {
                    continue;
                }
                v
            }
            None => (0..positions.len() as u32).collect(), // non-indexed → sequential (in-range by construction)
        };
        let mat = prim.material();
        let pbr = mat.pbr_metallic_roughness();
        out.push(DecodedPrimitive {
            positions,
            normals,
            uvs,
            indices,
            base_color_factor: pbr.base_color_factor(),
            base_color_texture: pbr.base_color_texture().map(|info| info.texture().source().index()),
            unlit: mat.unlit(),
            node_transform,
        });
    }
}

/// Recursively walk a node, accumulating the world transform, and decode every mesh it carries. `depth`
/// bounds the recursion: a hostile glTF with a deeply nested node chain must not overflow the worker stack
/// (a stack overflow aborts the process — the FFI panic guards can't catch it).
fn walk_node(
    node: gltf::Node,
    parent_world: glam::Mat4,
    blob: Option<&[u8]>,
    doc: &gltf::Document,
    out: &mut Vec<DecodedPrimitive>,
    depth: u32,
) {
    if depth > MAX_NODE_DEPTH {
        return;
    }
    let world = parent_world * glam::Mat4::from_cols_array_2d(&node.transform().matrix());
    if let Some(mesh) = node.mesh() {
        decode_mesh_primitives(mesh, world.to_cols_array_2d(), blob, doc, out);
    }
    for child in node.children() {
        walk_node(child, world, blob, doc, out, depth + 1);
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
            walk_node(node, glam::Mat4::IDENTITY, blob, &gltf.document, &mut primitives, 0);
        }
    } else {
        // No scene graph: decode every mesh with an identity transform (legacy / bare-mesh GLBs).
        for mesh in gltf.document.meshes() {
            decode_mesh_primitives(
                mesh,
                glam::Mat4::IDENTITY.to_cols_array_2d(),
                blob,
                &gltf.document,
                &mut primitives,
            );
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
                        // NOTE: deliberate copy. The blob is owned by the local `gltf` value (it took
                        // ownership in `from_slice`) and is dropped when this fn returns, so the returned
                        // `EmbeddedImage` cannot borrow it — the bytes must be copied out to outlive the
                        // decode. The zero-copy alternative (M11) is structural: hold the blob as
                        // `Arc<[u8]>` and have `EmbeddedImage` store an offset+len view, which changes the
                        // public type and every downstream consumer (e.g. `scene::rebase_decoded` moves
                        // `image.data`). Left as a single copy until that shared-buffer refactor lands.
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

    /// Hand-assemble a minimal GLB: one triangle with POSITION/NORMAL/TEXCOORD_0 + a material, indexed by
    /// the given `indices` (3 positions exist, so an index ≥ 3 is hostile/out-of-range).
    fn build_test_glb() -> Vec<u8> {
        build_test_glb_with_indices(&[0, 1, 2])
    }

    fn build_test_glb_with_indices(indices: &[u16]) -> Vec<u8> {
        let positions: [[f32; 3]; 3] = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let normals: [[f32; 3]; 3] = [[0.0, 0.0, 1.0]; 3];
        let uvs: [[f32; 2]; 3] = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];

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
        for &i in indices {
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
                {{"buffer":0,"byteOffset":{off_idx},"byteLength":{idx_bytes}}} ],
              "accessors":[
                {{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}},
                {{"bufferView":1,"componentType":5126,"count":3,"type":"VEC3"}},
                {{"bufferView":2,"componentType":5126,"count":3,"type":"VEC2"}},
                {{"bufferView":3,"componentType":5123,"count":{idx_count},"type":"SCALAR"}} ],
              "meshes":[{{"primitives":[{{"attributes":{{"POSITION":0,"NORMAL":1,"TEXCOORD_0":2}},"indices":3,"material":0}}]}}],
              "materials":[{{"pbrMetallicRoughness":{{"baseColorFactor":[1.0,0.5,0.25,1.0]}}}}]}}"#,
            bl = bin.len(),
            idx_bytes = indices.len() * 2,
            idx_count = indices.len()
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
        assert_eq!(p.positions.len(), 3);
        assert_eq!(p.base_color_factor, [1.0, 0.5, 0.25, 1.0]);
        assert_eq!(p.base_color_texture, None);

        // Raw attribute arrays (un-interleaved — the interleave is fused into the rebase, audit D2).
        assert_eq!(p.positions[0], [0.0, 0.0, 0.0]); // pos
        assert_eq!(p.positions[1], [1.0, 0.0, 0.0]); // 2nd vertex pos
        let uvs = p.uvs.as_ref().expect("the test GLB declares TEXCOORD_0");
        assert_eq!(uvs[0], [0.0, 0.0]);
        assert_eq!(uvs[2], [0.0, 1.0]); // 3rd vertex uv
    }

    /// Hostile input: an index references a vertex that doesn't exist (9 ≥ 3). The whole primitive is
    /// dropped rather than emitted with an out-of-range index that would later panic an array index in the
    /// mesh consumer (H3). A clean decode of a valid sibling is unaffected.
    #[test]
    fn out_of_range_index_drops_the_primitive() {
        let d = decode_glb(&build_test_glb_with_indices(&[0, 1, 9])).expect("decode should not error");
        assert!(
            d.primitives.is_empty(),
            "primitive with OOB index must be dropped, got {}",
            d.primitives.len()
        );
    }

    /// Hostile input: a node chain far deeper than `MAX_NODE_DEPTH` must not overflow the worker stack
    /// (a stack overflow aborts the whole process — the FFI panic guard can't catch it). Decoding returns
    /// (bounded), it does not crash; the over-depth tail is simply not walked (C1).
    #[test]
    fn deeply_nested_nodes_are_bounded_not_overflowing() {
        // Build a glTF with a single chain of (MAX_NODE_DEPTH + 50) empty nodes, no meshes.
        let n = MAX_NODE_DEPTH as usize + 50;
        let nodes: String = (0..n)
            .map(|i| {
                if i + 1 < n {
                    format!(r#"{{"children":[{}]}}"#, i + 1)
                } else {
                    "{}".to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(r#"{{"asset":{{"version":"2.0"}},"scenes":[{{"nodes":[0]}}],"nodes":[{nodes}]}}"#);
        let mut json_bytes = json.into_bytes();
        while json_bytes.len() % 4 != 0 {
            json_bytes.push(b' ');
        }
        let total = 12 + 8 + json_bytes.len();
        let mut glb = Vec::new();
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        glb.extend_from_slice(&0x4E4F_534Au32.to_le_bytes()); // "JSON"
        glb.extend_from_slice(&json_bytes);

        let d = decode_glb(&glb).expect("decode should not error");
        assert!(d.primitives.is_empty()); // no meshes anywhere in the chain
    }
}
