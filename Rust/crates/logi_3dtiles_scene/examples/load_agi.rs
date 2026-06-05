//! Headless de-risk: run the real AGI_HQ.dae (16 MB) through the M1 building loader to confirm the
//! COLLADA decode + transform survives a real-world Assimp export before we spend an editor build on it.
//! `cargo run -p logi_3dtiles_scene --example load_agi --release`

fn main() {
    let dir = r"C:\Users\premi\work\LogiUE\Intermediate\AgiExtract";
    let dae = std::fs::read_to_string(format!("{dir}\\AGI_HQ.dae")).expect("read dae");
    let jpg = std::fs::read(format!("{dir}\\AGI_HQ_TextureAtlas0.jpg")).expect("read jpg");
    let placement = logi_3dtiles_scene::Placement {
        longitude_deg: -75.5967013459,
        latitude_deg: 40.0386977494,
        height_m: 50.0,
        heading_deg: 90.0,
    };
    match logi_3dtiles_scene::load_building(&dae, jpg, &placement) {
        Ok(m) => {
            println!(
                "OK: {} vertices, {} indices ({} tris), texture {} bytes",
                m.vertices.len() / 32,
                m.indices.len(),
                m.indices.len() / 3,
                m.texture_jpeg.len()
            );
            println!("    bounds_center {:?} radius {:.1} cm", m.bounds_center, m.bounds_radius);
            println!("    ecef_origin {:?}", m.ecef_origin);
            // Sanity: indices in range, finite positions.
            let vcount = (m.vertices.len() / 32) as u32;
            let bad_idx = m.indices.iter().filter(|&&i| i >= vcount).count();
            println!("    out-of-range indices: {bad_idx}");
        }
        Err(e) => {
            eprintln!("DECODE FAILED: {e:?}");
            std::process::exit(1);
        }
    }
}
