# logi_bake — offline semantic-tile bake (ADR-0011, Milestone HE)

The **producer** side of the data-driven enrichment pipeline. It reads real-world data and writes a
versioned, content-addressed **tile store** (`manifest.json` + per-tile `.lst` files) that the Rust
`logi_semantic` crate consumes and the engine renders as *our own* assets.

The heavy ML stages stay here, **off the engine runtime** (the cdylib links no Python/torch) — see
[ADR-0011](../../Docs/Architecture/ADR-0011-Semantic-Tile-Enrichment.md) and the
[plan](../../Docs/Plans/Hero-Location-Semantic-Enrichment.md).

## HE-0 scope

Terrain heightfields only, from a **deterministic synthetic DTM** (a stand-in for LiDAR so the whole
bake → store → render loop is provable without multi-GB downloads or PDAL). Trees/foliage, water,
buildings, and AI materials are later HE milestones; the format is versioned so the runtime refuses an
unknown major.

## Run it

No install needed for the synthetic path (stdlib only). From this directory:

```powershell
$env:PYTHONPATH = "$PWD/src"
# Default hero AOI (39.643178, -75.617567), 1-mile radius, into the engine's store location:
python -m logi_bake bake --out ../../Saved/LogiTiles/hero
# A tiny store (4 tiles) — e.g. to regenerate the Rust cross-language fixture:
python -m logi_bake bake --radius 200 --leaf-level 1 --grid-n 9 --seed 42 `
  --out ../../Rust/crates/logi_semantic/tests/fixtures/hero_tiny
```

Then in the editor: **Project Settings → Plugins → LogiBevy Simulation → Semantic Tiles (HE)** → tick
**Show Baked Terrain** (and set the store dir if not the default `LogiTiles/hero`).

## Tests

```powershell
$env:PYTHONPATH = "$PWD/src"; python tests/test_logi_bake.py -v
```

Gates: format round-trip, geodesy cross-check, tiling, golden structure, and **deterministic (byte-identical)
re-bake**. The Rust side has the matching decoder + a cross-language fixture test
(`cargo test -p logi_semantic`).

## The `.lst` format contract

The `.lst` byte layout (see `src/logi_bake/lst.py`) is shared **byte-for-byte** with
`Rust/crates/logi_semantic/src/format.rs`. Changing one without the other breaks the cross-language
fixture test by design. Little-endian: a 58-byte header, `grid_n²` `f32` heights (row-major
south→north, west→east), then a trailing zlib CRC-32.

## Real-data path (later HE)

Install the extras and implement plan §3.B–E:

```powershell
pip install -e .[lidar,vector,trees,materials,dev]
```

`--generator lidar` is a guarded stub until the PDAL acquire/ground-filter stages land.
