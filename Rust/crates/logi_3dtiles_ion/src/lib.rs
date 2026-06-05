//! Cesium **ion asset-endpoint resolver** (ADR-0009, Milestone F). Given an ion asset id + an ion access
//! token, resolves the streamable 3D-Tiles root URL (+ any auth) the rest of the stack then fetches.
//!
//! Two response shapes, both handled:
//! - **Direct ion asset:** `{ "type":"3DTILES", "url":"https://…/tileset.json", "accessToken":"…",
//!   "attributions":[…] }` — the `accessToken` is appended to subsequent tile requests.
//! - **External asset (e.g. Google Photorealistic 3D Tiles, asset 2275207):** `{ "type":"3DTILES",
//!   "externalType":"3DTILES", "options":{ "url":"https://tile.googleapis.com/v1/3dtiles/root.json?key=…"
//!   }, "attributions":[…] }` — auth (the Google key) is already baked into the URL; per-tile auth is the
//!   `?session=…` the tileset responses hand back (the IO layer just appends `&key=…`).
//!
//! Network I/O lives in [`resolve_asset`]; the response parsing is the pure, unit-tested [`parse_endpoint`].

#![forbid(unsafe_code)]

use serde::Deserialize;

/// What went wrong resolving an ion asset.
#[derive(thiserror::Error, Clone, Debug, PartialEq, Eq)]
pub enum IonError {
    #[error("ion HTTP error: {0}")]
    Http(String),
    #[error("ion endpoint parse error: {0}")]
    Parse(String),
    /// The endpoint response carried neither a direct `url` nor an `options.url`.
    #[error("ion endpoint response had no tileset url")]
    NoTilesetUrl,
}

/// A resolved ion asset, ready to stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IonAsset {
    /// The 3D-Tiles root URL (already carries any auth query, e.g. Google's `?key=…`).
    pub tileset_url: String,
    /// Access token to append to tile requests for a DIRECT ion asset; empty for external assets whose
    /// auth is already in the URLs (Google P3DT).
    pub access_token: String,
    /// Required attribution strings (HTML) to surface in the UI.
    pub attributions: Vec<String>,
}

#[derive(Deserialize)]
struct EndpointResponse {
    #[serde(default)]
    url: Option<String>,
    #[serde(default, rename = "accessToken")]
    access_token: Option<String>,
    #[serde(default)]
    options: Option<EndpointOptions>,
    #[serde(default)]
    attributions: Vec<Attribution>,
}

#[derive(Deserialize)]
struct EndpointOptions {
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize)]
struct Attribution {
    #[serde(default)]
    html: String,
}

/// Parse an ion `/v1/assets/{id}/endpoint` JSON response into an [`IonAsset`]. Pure (no I/O) — the
/// network half is [`resolve_asset`].
pub fn parse_endpoint(json: &str) -> Result<IonAsset, IonError> {
    let er: EndpointResponse = serde_json::from_str(json).map_err(|e| IonError::Parse(e.to_string()))?;
    let tileset_url = er
        .url
        .or_else(|| er.options.and_then(|o| o.url))
        .ok_or(IonError::NoTilesetUrl)?;
    let attributions = er
        .attributions
        .into_iter()
        .map(|a| a.html)
        .filter(|h| !h.is_empty())
        .collect();
    Ok(IonAsset {
        tileset_url,
        access_token: er.access_token.unwrap_or_default(),
        attributions,
    })
}

/// Resolve an ion asset endpoint over HTTPS: `GET https://api.cesium.com/v1/assets/{asset_id}/endpoint`
/// with `Authorization: Bearer {ion_token}`. Blocking — call it off the kernel's owner thread.
pub fn resolve_asset(asset_id: u64, ion_token: &str) -> Result<IonAsset, IonError> {
    let endpoint = format!("https://api.cesium.com/v1/assets/{asset_id}/endpoint");
    let resp = ureq::get(&endpoint)
        .set("Authorization", &format!("Bearer {ion_token}"))
        .call()
        .map_err(|e| IonError::Http(e.to_string()))?;
    let body = resp.into_string().map_err(|e| IonError::Http(e.to_string()))?;
    parse_endpoint(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_direct_ion_asset() {
        let json = r#"{"type":"3DTILES","url":"https://assets.cesium.com/1/tileset.json","accessToken":"TKN","attributions":[{"html":"<a>Cesium</a>"}]}"#;
        let a = parse_endpoint(json).unwrap();
        assert_eq!(a.tileset_url, "https://assets.cesium.com/1/tileset.json");
        assert_eq!(a.access_token, "TKN");
        assert_eq!(a.attributions, vec!["<a>Cesium</a>".to_string()]);
    }

    #[test]
    fn parses_an_external_google_asset() {
        // The Google P3DT shape: externalType + options.url, no accessToken, the key in the URL.
        let json = r#"{"type":"3DTILES","externalType":"3DTILES","options":{"url":"https://tile.googleapis.com/v1/3dtiles/root.json?key=ABC"},"attributions":[{"html":"Google"},{"html":""}]}"#;
        let a = parse_endpoint(json).unwrap();
        assert_eq!(
            a.tileset_url,
            "https://tile.googleapis.com/v1/3dtiles/root.json?key=ABC"
        );
        assert!(a.access_token.is_empty());
        assert_eq!(a.attributions, vec!["Google".to_string()]); // empty html filtered out
    }

    #[test]
    fn missing_url_errors() {
        assert_eq!(parse_endpoint(r#"{"type":"3DTILES"}"#), Err(IonError::NoTilesetUrl));
    }
}
