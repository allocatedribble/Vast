"""logi_bake — the offline semantic-tile bake (ADR-0011 / Milestone HE).

The *producer* side of the enrichment pipeline: it reads real-world data (HE-0: a deterministic synthetic
DTM stands in for LiDAR) and writes a versioned, content-addressed **tile store** (`manifest.json` + per-tile
`.lst` files) that the Rust `logi_semantic` crate consumes. The `.lst` byte layout is the cross-language
contract — see `logi_bake.lst` and `Rust/crates/logi_semantic/src/format.rs` (they must agree byte-for-byte).

HE-0 scope = terrain heightfields only. Heavy ML stages (LiDAR/PDAL, trees, buildings, AI materials) are
optional extras and stay off the engine runtime entirely (the bake is offline content authoring).
"""

__version__ = "0.1.0"

BAKE_TOOL = f"logi_bake {__version__}"

# Format versions — bumped only on a layout change; the runtime refuses an unknown major.
MANIFEST_FORMAT_VERSION = 1
LST_VERSION = 1
