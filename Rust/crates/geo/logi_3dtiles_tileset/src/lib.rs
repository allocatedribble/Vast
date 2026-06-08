//! The OGC **3D Tiles** `tileset.json` data model + `serde` parsing, for the actorless 3D-Tiles stack
//! (ADR-0009, Milestone F). A tileset data model with a frame-invariant region-sphere memo
//! (`OnceCell`) on [`Tile`]; the traversal (`logi_3dtiles_select`) walks this tree, and geometry
//! (bounding-volume distance, the transform chain) lives there with `logi_geospatial`.
//!
//! Targets 3D Tiles 1.1 (native glTF content + implicit tiling). Unknown fields (metadata schema,
//! extensions, S2 bounding volumes) are ignored for v1.

#![forbid(unsafe_code)]

use glam::DMat4;
use serde::Deserialize;
use std::cell::OnceCell;

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
    /// 3D Tiles 1.1 `viewerRequestVolume`: the viewer MUST be inside this volume before the tile's content is
    /// requested or refined (a separate, optional volume from `boundingVolume` — the canonical use is gating a
    /// heavy interior point cloud to proximity). `None` ⇒ no restriction (the common case; Google P3DT never
    /// sets it). The selector skips the whole subtree (like a cull) while the viewer is outside it.
    #[serde(default, rename = "viewerRequestVolume")]
    pub viewer_request_volume: Option<BoundingVolume>,
    /// **Frame-invariant ECEF bounding-sphere memo (S3)** — populated lazily by [`Self::region_sphere_ecef`]
    /// for `region` volumes only. A `region` is geodetic and (per the 3D Tiles spec) transform-independent, so
    /// its `(center, radius)` enclosing sphere depends solely on the immutable `region` array — never on the
    /// per-frame `world` transform — and is therefore safe to compute once and reuse every frame. (`box`/
    /// `sphere` volumes ride `world`, which can change, and are cheap to recompute, so they are NOT memoized
    /// here.) Skipped by serde and excluded from `Clone`-equality concerns (it carries no semantic state — a
    /// clone simply starts cold). The 8 `cartographic_to_cartesian` calls (24 trig ops) per region tile were
    /// previously redone every selection pass; this collapses them to once per tile for the planet's lifetime.
    #[serde(skip)]
    pub bounds_memo: OnceCell<([f64; 3], f64)>,
}

impl Tile {
    /// The tile's local transform as a `glam` matrix (identity if absent).
    pub fn matrix(&self) -> DMat4 {
        self.transform
            .map(|a| DMat4::from_cols_array(&a))
            .unwrap_or(DMat4::IDENTITY)
    }

    /// The memoized ECEF enclosing sphere `(center, radius)` of a `region` bounding volume — computed once
    /// (via `compute`) and cached in [`Self::bounds_memo`] for every subsequent frame (S3). `compute` is the
    /// 8-corner-on-the-ellipsoid fold the selection traversal does; it is invoked at most once per tile because
    /// a region's sphere is transform-independent and its inputs never change. Returns `None` for a non-region
    /// volume (the caller computes those directly — they depend on the per-frame transform). Stored as plain
    /// arrays/scalars (not `glam` types) so the tileset crate keeps zero extra deps for the cache.
    #[inline]
    pub fn region_sphere_ecef(&self, compute: impl FnOnce() -> ([f64; 3], f64)) -> ([f64; 3], f64) {
        *self.bounds_memo.get_or_init(compute)
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
    pub uri: String,
    #[serde(default)]
    pub bounding_volume: Option<BoundingVolume>,
}

impl Content {
    /// Whether this content points at another `tileset.json` (an external tileset to graft in). Delegates to
    /// the free [`is_tileset_uri`] so the rule has ONE home for the whole 3D-Tiles stack (ADR-0004).
    pub fn is_external_tileset(&self) -> bool {
        is_tileset_uri(&self.uri)
    }
}

/// Best-effort *hint* that a content URI names an external `.json` tileset (vs a `.glb`/`.b3dm` geometry
/// tile), judged by the PATH extension after stripping the query/fragment — so a signed tile URL whose
/// token happens to contain ".json" isn't misclassified. The single home of this rule: [`Content::is_external_tileset`]
/// calls it, and the streaming layer re-exports it (was a byte-for-byte duplicate there). Only a
/// request-time hint — the authoritative classification sniffs the fetched bytes (glTF magic vs a `{` opener).
pub fn is_tileset_uri(uri: &str) -> bool {
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    path.rsplit('.')
        .next()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

/// Implicit tiling (3D Tiles 1.1): the tile is the root of a procedurally-subdivided quadtree/octree
/// whose tile/content availability lives in the `subtrees` files.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ImplicitTiling {
    pub subdivision_scheme: SubdivisionScheme,
    pub subtree_levels: u32,
    /// 3D Tiles 1.1 `availableLevels`: a **count** of levels (level 0 = the implicit root; the deepest
    /// valid level is `available_levels - 1`). The selector treats it as a count.
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

impl TemplateUri {
    /// Substitute the implicit-tiling placeholders `{level}`, `{x}`, `{y}`, `{z}` in this template with a
    /// concrete tile's global coordinates (3D Tiles 1.1 `ImplicitTiling`: the `subtrees` URI and a tile's
    /// templated `content.uri` are filled from the tile's level + Morton-decoded x/y[/z]). A quadtree template
    /// simply has no `{z}` to fill (the `z` we pass is then ignored). Single-allocation chained `replace`; the
    /// placeholder set is fixed and tiny, so this is not a hot-path concern (it runs once per synthesized tile,
    /// and only for tiles the traversal actually keeps). See [`substitute_template`] for the shared impl.
    #[inline]
    pub fn substitute(&self, level: u32, x: u32, y: u32, z: u32) -> String {
        substitute_template(&self.uri, level, x, y, z)
    }
}

/// Fill the implicit-tiling placeholders (`{level}/{x}/{y}/{z}`) of `template` with concrete coordinates. The
/// single home of the rule (shared by [`TemplateUri::substitute`] and the traversal's content-URL synthesis) so
/// the substitution is defined once for the whole 3D-Tiles stack. Replaces each token exactly; a quadtree
/// template that lacks `{z}` is unaffected by `z`.
pub fn substitute_template(template: &str, level: u32, x: u32, y: u32, z: u32) -> String {
    template
        .replace("{level}", &level.to_string())
        .replace("{x}", &x.to_string())
        .replace("{y}", &y.to_string())
        .replace("{z}", &z.to_string())
}

/// Subdivide an implicit-tiling **root** bounding volume into the volume of the descendant tile at global
/// `(level, x, y, z)` (Morton-decoded coordinates relative to the implicit root; `level == 0 ⇒ the root
/// itself). This is the geometric half of 3D Tiles implicit tiling: a tile's bounds are NOT stored — they are
/// derived by uniformly subdividing the root. Per the spec's subdivision schemes:
///
/// - **Quadtree** halves the two horizontal axes each level (longitude/latitude for a `region`; the `u`/`v`
///   half-axes for a `box`), leaving the vertical axis (height / `w`) spanning the full root extent.
/// - **Octree** halves all three axes each level (adds height for a `region`, the `w` half-axis for a `box`).
///
/// At `level` there are `n = 2^level` cells per subdivided axis; cell index `i ∈ [0, n)` along an axis selects
/// the sub-interval `[i/n, (i+1)/n]` of the root's extent on that axis. A `sphere` root has no axes to
/// subdivide spatially, so it is returned unchanged (a conservative bound for every descendant — correct, if
/// loose; real implicit tilesets use `region`/`box` roots). `level` is assumed within the codec's range
/// (`2^level` must not overflow — the traversal caps depth well below that); a pathological `level ≥ 32`
/// saturates the shift to a degenerate full-extent child rather than wrapping.
pub fn subdivide_bounds(
    root: &BoundingVolume,
    scheme: SubdivisionScheme,
    level: u32,
    x: u32,
    y: u32,
    z: u32,
) -> BoundingVolume {
    // n = 2^level cells per subdivided axis (level 0 ⇒ n = 1 ⇒ the root unchanged). Saturate a hostile
    // out-of-range level to the full extent rather than overflowing the shift.
    let n = 1u64.checked_shl(level).unwrap_or(0).max(1) as f64;
    let octree = matches!(scheme, SubdivisionScheme::Octree);
    match root {
        BoundingVolume::Region([west, south, east, north, min_h, max_h]) => {
            // Longitude ← x, latitude ← y, height ← z (octree only). Each axis: pick cell i of n equal slices.
            let lon_w = (east - west) / n;
            let lat_w = (north - south) / n;
            let w = west + lon_w * x as f64;
            let s = south + lat_w * y as f64;
            let (mn, mx) = if octree {
                let h_w = (max_h - min_h) / n;
                (min_h + h_w * z as f64, min_h + h_w * (z as f64 + 1.0))
            } else {
                (*min_h, *max_h) // quadtree: height spans the full root extent
            };
            BoundingVolume::Region([w, s, w + lon_w, s + lat_w, mn, mx])
        }
        BoundingVolume::Box(b) => {
            let center = [b[0], b[1], b[2]];
            let u = [b[3], b[4], b[5]]; // half-axis ← x
            let v = [b[6], b[7], b[8]]; // half-axis ← y
            let w = [b[9], b[10], b[11]]; // half-axis ← z (octree only)
                                          // Cell i of n along a half-axis: the child center shifts by full_axis · (2i + 1 − n)/(2n) (i.e.
                                          // half_axis · (2i + 1 − n)/n), and the child half-axis shrinks to half_axis/n. (At n = 1, i = 0:
                                          // shift 0, axis unchanged.) Quadtree leaves `w` (and its shift) at full extent.
            let off = |i: u32| (2.0 * i as f64 + 1.0 - n) / n;
            let (su, sv, sw) = (off(x), off(y), if octree { off(z) } else { 0.0 });
            let mut c = center;
            for k in 0..3 {
                c[k] += u[k] * su + v[k] * sv + w[k] * sw;
            }
            let scale_axis = |axis: [f64; 3], do_it: bool| {
                if do_it {
                    [axis[0] / n, axis[1] / n, axis[2] / n]
                } else {
                    axis
                }
            };
            let nu = scale_axis(u, true);
            let nv = scale_axis(v, true);
            let nw = scale_axis(w, octree);
            BoundingVolume::Box([
                c[0], c[1], c[2], nu[0], nu[1], nu[2], nv[0], nv[1], nv[2], nw[0], nw[1], nw[2],
            ])
        }
        // A sphere root: no axes to subdivide — return it unchanged (a conservative enclosing bound for every
        // descendant). Real implicit tilesets root at a region or box; this keeps the function total.
        BoundingVolume::Sphere(s) => BoundingVolume::Sphere(*s),
    }
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
            "content": { "uri": "1/0/0.b3dm" }
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
    fn parses_implicit_tiling() {
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
              "availableLevels": 21,
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
        assert_eq!(it.available_levels, 21); // a COUNT of levels (deepest valid level = 20)
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

    #[test]
    fn template_substitutes_all_placeholders() {
        let t = TemplateUri {
            uri: "content/{level}/{x}/{y}/{z}.glb".to_string(),
        };
        assert_eq!(t.substitute(3, 5, 6, 7), "content/3/5/6/7.glb");
        // A quadtree template (no {z}) is unaffected by z.
        let q = TemplateUri {
            uri: "subtrees/{level}/{x}/{y}.subtree".to_string(),
        };
        assert_eq!(q.substitute(2, 1, 0, 99), "subtrees/2/1/0.subtree");
    }

    #[test]
    fn subdivide_region_quadtree_halves_lon_lat_keeps_height() {
        // Root region 0..1 lon, 0..1 lat, 0..100 height.
        let root = BoundingVolume::Region([0.0, 0.0, 1.0, 1.0, 0.0, 100.0]);
        // Level 0 ⇒ the root unchanged.
        assert_eq!(subdivide_bounds(&root, SubdivisionScheme::Quadtree, 0, 0, 0, 0), root);
        // Level 1, child (1,1): the NE quadrant lon 0.5..1, lat 0.5..1, height still 0..100 (quadtree).
        let c = subdivide_bounds(&root, SubdivisionScheme::Quadtree, 1, 1, 1, 0);
        assert_eq!(c, BoundingVolume::Region([0.5, 0.5, 1.0, 1.0, 0.0, 100.0]));
        // Level 2, child (3,0): lon 0.75..1.0, lat 0.0..0.25, height unchanged.
        let c2 = subdivide_bounds(&root, SubdivisionScheme::Quadtree, 2, 3, 0, 0);
        if let BoundingVolume::Region(r) = c2 {
            assert!((r[0] - 0.75).abs() < 1e-12 && (r[2] - 1.0).abs() < 1e-12);
            assert!((r[1] - 0.0).abs() < 1e-12 && (r[3] - 0.25).abs() < 1e-12);
            assert_eq!((r[4], r[5]), (0.0, 100.0));
        } else {
            panic!("expected region");
        }
    }

    #[test]
    fn subdivide_region_octree_also_halves_height() {
        let root = BoundingVolume::Region([0.0, 0.0, 1.0, 1.0, 0.0, 100.0]);
        // Level 1, child (0,0,1): SW horizontally, UPPER height slab 50..100.
        let c = subdivide_bounds(&root, SubdivisionScheme::Octree, 1, 0, 0, 1);
        assert_eq!(c, BoundingVolume::Region([0.0, 0.0, 0.5, 0.5, 50.0, 100.0]));
    }

    #[test]
    fn subdivide_box_quadtree_shrinks_uv_keeps_w() {
        // Axis-aligned root box: center origin, half-axes u=4(x), v=8(y), w=12(z).
        let root = BoundingVolume::Box([0.0, 0.0, 0.0, 4.0, 0.0, 0.0, 0.0, 8.0, 0.0, 0.0, 0.0, 12.0]);
        // Level 1 has 2 cells/axis. Child (1,1): the +u,+v cell. Center shifts to +u/2,+v/2 = (2,4,0); the
        // u/v half-axes halve to 2 and 4; w stays 12 (quadtree).
        let c = subdivide_bounds(&root, SubdivisionScheme::Quadtree, 1, 1, 1, 0);
        assert_eq!(
            c,
            BoundingVolume::Box([2.0, 4.0, 0.0, 2.0, 0.0, 0.0, 0.0, 4.0, 0.0, 0.0, 0.0, 12.0])
        );
        // Child (0,0): the −u,−v cell, center (−2,−4,0), same shrunk axes, full w.
        let c0 = subdivide_bounds(&root, SubdivisionScheme::Quadtree, 1, 0, 0, 0);
        assert_eq!(
            c0,
            BoundingVolume::Box([-2.0, -4.0, 0.0, 2.0, 0.0, 0.0, 0.0, 4.0, 0.0, 0.0, 0.0, 12.0])
        );
    }

    #[test]
    fn subdivide_box_octree_shrinks_all_three_axes() {
        let root = BoundingVolume::Box([0.0, 0.0, 0.0, 4.0, 0.0, 0.0, 0.0, 8.0, 0.0, 0.0, 0.0, 12.0]);
        // Octree level 1 child (1,1,1): the +u+v+w corner cell → center (2,4,6), all half-axes halved.
        let c = subdivide_bounds(&root, SubdivisionScheme::Octree, 1, 1, 1, 1);
        assert_eq!(
            c,
            BoundingVolume::Box([2.0, 4.0, 6.0, 2.0, 0.0, 0.0, 0.0, 4.0, 0.0, 0.0, 0.0, 6.0])
        );
    }

    #[test]
    fn region_sphere_memo_computes_once() {
        use std::cell::Cell;
        let tile = Tile {
            bounding_volume: BoundingVolume::Region([0.0, 0.0, 0.01, 0.01, 0.0, 10.0]),
            geometric_error: 1.0,
            refine: None,
            transform: None,
            content: None,
            contents: Vec::new(),
            children: Vec::new(),
            implicit_tiling: None,
            viewer_request_volume: None,
            bounds_memo: OnceCell::new(),
        };
        let calls = Cell::new(0);
        let compute = || {
            calls.set(calls.get() + 1);
            ([1.0, 2.0, 3.0], 4.0)
        };
        let a = tile.region_sphere_ecef(compute);
        let b = tile.region_sphere_ecef(|| {
            calls.set(calls.get() + 1);
            ([9.0, 9.0, 9.0], 9.0) // a different value — must be ignored (cache hit)
        });
        assert_eq!(a, ([1.0, 2.0, 3.0], 4.0));
        assert_eq!(b, a, "second call returns the memoized value");
        assert_eq!(calls.get(), 1, "compute ran exactly once");
    }

    #[test]
    fn is_tileset_uri_judges_by_path_extension_not_the_query() {
        // The single home of the external-tileset URI hint (the streaming layer re-exports this).
        assert!(is_tileset_uri("https://h/root.json"));
        assert!(is_tileset_uri("https://h/root.JSON?access_token=T")); // case-insensitive, query stripped
                                                                       // A .glb whose signed query merely contains ".json" must NOT be classed as a tileset.
        assert!(!is_tileset_uri("https://h/x.glb?session=a.json.b"));
        // Extensionless endpoints carry no usable hint — byte sniffing is authoritative there.
        assert!(!is_tileset_uri("https://h/v1/3dtiles?key=K"));
    }
}
