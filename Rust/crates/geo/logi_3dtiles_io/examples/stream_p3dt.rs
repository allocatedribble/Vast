//! Live end-to-end smoke test of the streaming data path against Google Photorealistic 3D Tiles:
//! resolve ion → fetch root.json → parse the tileset → find a `.glb` tile → resolve its URL → fetch it →
//! attempt a (non-Draco) decode. Token from the environment, never hardcoded:
//! `$env:CESIUM_ION_TOKEN="<token>"; cargo run -p logi_3dtiles_io --example stream_p3dt --release`

use logi_3dtiles_tileset::Tile;

/// Depth-first search for the first tile whose content is a `.glb`.
fn find_glb(tile: &Tile, out: &mut Option<String>) {
    if out.is_some() {
        return;
    }
    for uri in tile.content_uris() {
        if uri.contains(".glb") {
            *out = Some(uri.to_string());
            return;
        }
    }
    for child in &tile.children {
        find_glb(child, out);
    }
}

/// Borrow the JSON chunk of a GLB (the 12-byte header + a chunk header at offset 12: u32 length, u32
/// type, then the payload at offset 20). Returns `None` for anything too short or not a `glTF` container —
/// every access is bounds-checked, so a malformed/truncated download can't panic the example.
fn glb_json_chunk(glb: &[u8]) -> Option<&[u8]> {
    if glb.get(0..4)? != b"glTF" {
        return None;
    }
    let len_bytes: [u8; 4] = glb.get(12..16)?.try_into().ok()?;
    let json_len = u32::from_le_bytes(len_bytes) as usize;
    let end = 20usize.checked_add(json_len)?;
    glb.get(20..end)
}

fn main() {
    let token = std::env::var("CESIUM_ION_TOKEN").expect("set CESIUM_ION_TOKEN");

    let asset = logi_3dtiles_ion::resolve_asset(2275207, &token).expect("ion resolve");
    println!(
        "1. ion resolved → tileset root ({} attributions)",
        asset.attributions.len()
    );

    let root_bytes = logi_3dtiles_io::http_get(&asset.tileset_url).expect("fetch root.json");
    println!("2. root.json fetched: {} bytes", root_bytes.len());

    let tileset = logi_3dtiles_tileset::parse_tileset(&root_bytes).expect("parse tileset");
    println!(
        "3. tileset parsed: root geometricError={:e}, refine={:?}, {} children",
        tileset.geometric_error,
        tileset.root.refine,
        tileset.root.children.len()
    );

    let mut glb_uri = None;
    find_glb(&tileset.root, &mut glb_uri);
    let glb_uri = glb_uri.expect("no .glb content in root.json");
    let full = logi_3dtiles_io::resolve_url(&asset.tileset_url, &glb_uri).expect("resolve tile url");
    println!("4. found a .glb tile, URL resolved (auth carried)");

    let glb = logi_3dtiles_io::http_get(&full).expect("fetch tile");
    println!(
        "5. tile fetched: {} bytes, GLB magic 'glTF' = {}",
        glb.len(),
        glb.starts_with(b"glTF")
    );

    // Inspect the GLB JSON chunk for the extensions that gate decoding. All reads go through `.get(..)` so
    // a short/truncated GLB can never panic this smoke test (the decoder below is the real validation).
    if let Some(json) = glb_json_chunk(&glb) {
        let s = String::from_utf8_lossy(json);
        println!(
            "   extensions present → unlit={}, draco={}, ktx2/basisu={}, webp={}",
            s.contains("KHR_materials_unlit"),
            s.contains("KHR_draco_mesh_compression"),
            s.contains("KHR_texture_basisu"),
            s.contains("EXT_texture_webp"),
        );
    }

    match logi_gltf::decode_glb(&glb) {
        Ok(d) => println!(
            "6. gltf decode: {} primitives, {} images{}",
            d.primitives.len(),
            d.images.len(),
            if d.primitives.is_empty() {
                "  (0 primitives ⇒ geometry is Draco-compressed — the decoder shim is the next piece)"
            } else {
                ""
            }
        ),
        Err(e) => println!("6. gltf decode error: {e:?}"),
    }
}
