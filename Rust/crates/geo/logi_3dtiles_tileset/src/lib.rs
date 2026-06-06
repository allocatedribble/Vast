//! The OGC **3D Tiles** `tileset.json` data model + parsing, for the actorless 3D-Tiles stack
//! (ADR-0009, Milestone F). Pure data + `serde` — the traversal (`logi_3dtiles_select`) walks this tree;
//! geometry (bounding-volume distance, the transform chain) lives there with `logi_geospatial`.
//!
//! Targets 3D Tiles 1.1 (native glTF content + implicit tiling) and tolerates legacy 1.0 spellings
//! (`content.url`, `maximumLevel`). Unknown fields (metadata schema, extensions, S2 bounding volumes)
//! are ignored for v1.

#![forbid(unsafe_code)]

use glam::DMat4;
use serde::Deserialize;

/// A parsed tileset.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tileset {
    pub asset: Asset,
    /// The error, in metres, of *not* refining past the root.
    pub geometric_error: f64,
    pub root: Tile,
    #[serde(default)]
    pub extensions_used: Vec<String>,
    #[serde(default)]
    pub extensions_required: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    pub version: String,
    #[serde(default)]
    pub tileset_version: Option<String>,
}

/// One node of the tile tree.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tile {
    pub bounding_volume: BoundingVolume,
    /// The error, in metres, of rendering this tile instead of refining to its children.
    pub geometric_error: f64,
    #[serde(default)]
    pub refine: Option<Refine>,
    /// Column-major 4×4; composes onto the parent's transform. Absent ⇒ identity (see [`Self::matrix`]).
    #[serde(default)]
    pub transform: Option<[f64; 16]>,
    #[serde(default)]
    pub content: Option<Content>,
    /// 3D Tiles 1.1 multi-content.
    #[serde(default)]
    pub contents: Vec<Content>,
    #[serde(default)]
    pub children: Vec<Tile>,
    #[serde(default)]
    pub implicit_tiling: Option<ImplicitTiling>,
}

impl Tile {
    /// The tile's local transform as a `glam` matrix (identity if absent).
    pub fn matrix(&self) -> DMat4 {
        self.transform
            .map(|a| DMat4::from_cols_array(&a))
            .unwrap_or(DMat4::IDENTITY)
    }

    /// Resolve refinement, inheriting the parent's when this tile doesn't specify one. (The spec requires
    /// `refine` on the root; the `parent` default covers a malformed root.)
    pub fn refine_or(&self, parent: Refine) -> Refine {
        self.refine.unwrap_or(parent)
    }

    /// All content URIs of this tile (the single `content` plus any 1.1 `contents`).
    pub fn content_uris(&self) -> impl Iterator<Item = &str> {
        self.content.iter().chain(self.contents.iter()).map(|c| c.uri.as_str())
    }
}

/// A tile bounding volume — externally tagged exactly as the JSON (`{"region": [...]}`, `{"box": [...]}`,
/// `{"sphere": [...]}`).
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum BoundingVolume {
    /// `[west, south, east, north, minHeight, maxHeight]` — lon/lat radians, heights metres.
    Region([f64; 6]),
    /// `[cx,cy,cz, ux,uy,uz, vx,vy,vz, wx,wy,wz]` — center + three half-axis vectors (oriented box).
    Box([f64; 12]),
    /// `[cx, cy, cz, radius]` (ECEF metres).
    Sphere([f64; 4]),
}

/// Refinement strategy.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum Refine {
    /// Children render *in addition to* the parent.
    Add,
    /// Children render *in place of* the parent.
    Replace,
}

/// A tile's renderable content (glTF/GLB, or — when the URI ends in `.json` — an external tileset).
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Content {
    #[serde(alias = "url")] // legacy 1.0 spelled it `url`
    pub uri: String,
    #[serde(default)]
    pub bounding_volume: Option<BoundingVolume>,
}

impl Content {
    /// Whether this content points at another `tileset.json` (an external tileset to graft in).
    pub fn is_external_tileset(&self) -> bool {
        let path = self.uri.split(['?', '#']).next().unwrap_or(&self.uri);
        path.rsplit('.')
            .next()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
    }
}

/// Implicit tiling (3D Tiles 1.1): the tile is the root of a procedurally-subdivided quadtree/octree
/// whose tile/content availability lives in the `subtrees` files.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ImplicitTiling {
    pub subdivision_scheme: SubdivisionScheme,
    pub subtree_levels: u32,
    #[serde(alias = "maximumLevel")]
    pub available_levels: u32,
    pub subtrees: TemplateUri,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum SubdivisionScheme {
    Quadtree,
    Octree,
}

impl SubdivisionScheme {
    /// Children per node: 4 (quadtree) or 8 (octree).
    pub fn child_count(self) -> u32 {
        match self {
            Self::Quadtree => 4,
            Self::Octree => 8,
        }
    }
}

/// A `{level}/{x}/{y}[/{z}]`-templated URI (for content + subtree files under implicit tiling).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct TemplateUri {
    pub uri: String,
}

/// Parse a `tileset.json` document.
pub fn parse_tileset(json: &[u8]) -> Result<Tileset, serde_json::Error> {
    serde_json::from_slice(json)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
    {
      "asset": { "version": "1.1" },
      "geometricError": 500.0,
      "extensionsUsed": ["3DTILES_content_gltf"],
      "root": {
        "boundingVolume": { "region": [-1.3, 0.6, -1.2, 0.7, 0.0, 88.0] },
        "geometricError": 268.0,
        "refine": "REPLACE",
        "transform": [1,0,0,0, 0,1,0,0, 0,0,1,0, 10,20,30,1],
        "content": { "uri": "0/0/0.glb" },
        "children": [
          {
            "boundingVolume": { "box": [0,0,0, 100,0,0, 0,100,0, 0,0,50] },
            "geometricError": 134.0,
            "content": { "url": "1/0/0.b3dm" }
          },
          {
            "boundingVolume": { "sphere": [0,0,0, 141.0] },
            "geometricError": 0.0,
            "content": { "uri": "sub/tileset.json?v=2" }
          }
        ]
      }
    }"#;

    #[test]
    fn parses_a_representative_tileset() {
        let ts = parse_tileset(SAMPLE.as_bytes()).expect("parse");
        assert_eq!(ts.asset.version, "1.1");
        assert_eq!(ts.geometric_error, 500.0);
        assert_eq!(ts.extensions_used, vec!["3DTILES_content_gltf"]);
        assert_eq!(ts.root.geometric_error, 268.0);
        assert_eq!(ts.root.refine, Some(Refine::Replace));
        assert!(matches!(ts.root.bounding_volume, BoundingVolume::Region(_)));
        // transform → glam (column-major): the translation is the 4th column.
        assert_eq!(ts.root.matrix().w_axis.truncate(), glam::DVec3::new(10.0, 20.0, 30.0));
        assert_eq!(ts.root.children.len(), 2);
        assert!(matches!(ts.root.children[0].bounding_volume, BoundingVolume::Box(_)));
        assert!(matches!(ts.root.children[1].bounding_volume, BoundingVolume::Sphere(_)));
        // legacy `url` alias resolves to `uri`.
        assert_eq!(ts.root.children[0].content.as_ref().unwrap().uri, "1/0/0.b3dm");
        // external-tileset detection (ignoring the query string).
        assert!(ts.root.children[1].content.as_ref().unwrap().is_external_tileset());
        assert!(!ts.root.children[0].content.as_ref().unwrap().is_external_tileset());
    }

    #[test]
    fn refine_inherits_from_parent() {
        let ts = parse_tileset(SAMPLE.as_bytes()).unwrap();
        assert_eq!(ts.root.children[0].refine, None); // no explicit refine
        assert_eq!(ts.root.children[0].refine_or(Refine::Replace), Refine::Replace);
        assert_eq!(ts.root.children[0].refine_or(Refine::Add), Refine::Add);
    }

    #[test]
    fn parses_implicit_tiling_with_legacy_alias() {
        let json = r#"
        {
          "asset": { "version": "1.1" },
          "geometricError": 10000.0,
          "root": {
            "boundingVolume": { "region": [-3.14, -1.57, 3.14, 1.57, 0, 9000] },
            "geometricError": 5000.0,
            "refine": "REPLACE",
            "implicitTiling": {
              "subdivisionScheme": "QUADTREE",
              "subtreeLevels": 7,
              "maximumLevel": 21,
              "subtrees": { "uri": "subtrees/{level}/{x}/{y}.subtree" }
            },
            "content": { "uri": "content/{level}/{x}/{y}.glb" }
          }
        }"#;
        let ts = parse_tileset(json.as_bytes()).unwrap();
        let it = ts.root.implicit_tiling.as_ref().expect("implicit");
        assert_eq!(it.subdivision_scheme, SubdivisionScheme::Quadtree);
        assert_eq!(it.subdivision_scheme.child_count(), 4);
        assert_eq!(it.subtree_levels, 7);
        assert_eq!(it.available_levels, 21); // via the `maximumLevel` alias
        assert_eq!(it.subtrees.uri, "subtrees/{level}/{x}/{y}.subtree");
    }

    #[test]
    fn content_uris_iterates_single_and_multi() {
        let json = r#"
        { "asset": {"version":"1.1"}, "geometricError": 1.0,
          "root": { "boundingVolume": {"sphere":[0,0,0,1]}, "geometricError": 0.0,
                    "contents": [ {"uri":"a.glb"}, {"uri":"b.glb"} ] } }"#;
        let ts = parse_tileset(json.as_bytes()).unwrap();
        assert_eq!(ts.root.content_uris().collect::<Vec<_>>(), vec!["a.glb", "b.glb"]);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // metadata/schema/extensions a v1 model doesn't map must not break parsing.
        let json = r#"
        { "asset": {"version":"1.1"}, "geometricError": 1.0, "schema": {"classes": {}},
          "root": { "boundingVolume": {"sphere":[0,0,0,1]}, "geometricError": 0.0,
                    "metadata": {"class":"tile"}, "extensions": {"FOO": {}} } }"#;
        assert!(parse_tileset(json.as_bytes()).is_ok());
    }
}
