//! Live ion resolution against the real API. The token comes from the environment (never hardcoded):
//! `$env:CESIUM_ION_TOKEN="<token>"; cargo run -p logi_3dtiles_ion --example resolve --release -- 2275207`

fn main() {
    let token = std::env::var("CESIUM_ION_TOKEN").expect("set CESIUM_ION_TOKEN");
    let asset_id: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(2275207);

    match logi_3dtiles_ion::resolve_asset(asset_id, &token) {
        Ok(a) => {
            // Mask the auth query so the token/key never lands in logs.
            let host_path = a.tileset_url.split('?').next().unwrap_or("");
            println!("OK asset {asset_id}");
            println!("    tileset_url (host/path) = {host_path}");
            println!("    url carries query auth   = {}", a.tileset_url.contains('?'));
            println!("    access_token present     = {}", !a.access_token.is_empty());
            println!("    attributions             = {:?}", a.attributions);
        }
        Err(e) => {
            eprintln!("ERROR: {e:?}");
            std::process::exit(1);
        }
    }
}
