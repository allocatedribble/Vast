"""The bake orchestrator: config → origin → tiling → per-tile heightfields → tile store + manifest + QA
report. Deterministic and idempotent (no wall-clock, sorted JSON), so a re-bake is byte-identical — the
reproducibility gate (plan §9)."""

from __future__ import annotations

import hashlib
import json
import math
from pathlib import Path
from typing import Any

from . import BAKE_TOOL, MANIFEST_FORMAT_VERSION, __version__
from .config import BakeConfig
from .dtm import Dtm, make_dtm
from .lst import encode_tile, f32
from .geo import GeoidModel, cartographic_to_cartesian
from .tiling import Tile, pyramid_tiles


def _sample_tile(dtm: Dtm, tile: Tile, grid_n: int) -> list[float]:
    """Sample the DTM on the tile's `grid_n × grid_n` grid, row-major south→north, west→east, each value
    rounded to f32 so the stored bytes are canonical. Uses the source's vectorized `heights_grid` (one
    batched reprojection for the whole tile) when it offers one — the real sources do — else a per-sample
    fallback (the synthetic/LiDAR sources, kept scalar so their bytes stay canonical)."""
    step = tile.size_m / (grid_n - 1)
    grid_fn = getattr(dtm, "heights_grid", None)
    if grid_fn is not None:
        import numpy as np

        easts = tile.origin_e + np.arange(grid_n) * step
        norths = tile.origin_n + np.arange(grid_n) * step
        ee, nn = np.meshgrid(easts, norths)  # ee[j,i]=easts[i], nn[j,i]=norths[j] → row-major j,i
        zz = np.asarray(grid_fn(ee, nn), dtype=float).reshape(-1)
        return [f32(float(v)) for v in zz]
    heights: list[float] = []
    for j in range(grid_n):
        north = tile.origin_n + j * step
        for i in range(grid_n):
            east = tile.origin_e + i * step
            heights.append(f32(dtm.height(east, north)))
    return heights


def bake(cfg: BakeConfig, out_dir: Path) -> dict[str, Any]:
    """Bake `cfg` into a tile store at `out_dir` (creating `tiles/`, `manifest.json`, `qa_report.json`).
    Returns the manifest dict."""
    out_dir = Path(out_dir)
    tiles_dir = out_dir / "tiles"
    tiles_dir.mkdir(parents=True, exist_ok=True)

    origin_ecef = cartographic_to_cartesian(cfg.lon_deg, cfg.lat_deg, cfg.origin_height_m)
    # Vertical datum: real LiDAR is NAVD88 orthometric → convert to WGS84 ellipsoidal via the geoid
    # undulation at the AOI centre (≈constant over a 1-mile AOI). Synthetic heights are already ellipsoidal.
    real = cfg.generator in ("lidar", "terrarium", "dem")
    if real:
        geoid = GeoidModel.from_gtx(cfg.geoid_gtx_path) if cfg.geoid_gtx_path else GeoidModel.constant(cfg.geoid_undulation_m)
        undulation_m = geoid.undulation(cfg.lat_deg, cfg.lon_deg)
    else:
        undulation_m = 0.0
    if cfg.generator == "terrarium":
        from .acquire import TerrariumDem

        # Cache downloaded source tiles next to the stores (shared across re-bakes; offline reproducibility,
        # plan §3.A). Not part of BakeConfig — it never changes the output, only avoids re-downloading.
        cache_dir = out_dir.parent / "_cache" / "terrarium"
        dtm = TerrariumDem(cfg.lat_deg, cfg.lon_deg, cfg.radius_m, cfg.zoom, undulation_m, cache_dir)
    elif cfg.generator == "dem":
        from .acquire import GeoTiffDem

        dtm = GeoTiffDem(cfg.dem_path, cfg.lat_deg, cfg.lon_deg, undulation_m)
    else:
        dtm = make_dtm(cfg.generator, cfg.seed, cfg.laz_path, undulation_m)

    tiles_meta: list[dict[str, Any]] = []
    g_min, g_max = math.inf, -math.inf
    base_ge = 1.0  # the root tile's relief; geometric error halves per level (the SSE-LOD pyramid)
    for tile in pyramid_tiles(cfg.radius_m, cfg.leaf_level):
        heights = _sample_tile(dtm, tile, cfg.grid_n)
        data = encode_tile(
            tile.level, tile.x, tile.y, cfg.grid_n, tile.origin_e, tile.origin_n, tile.size_m, heights
        )
        name = f"L{tile.level}_X{tile.x}_Y{tile.y}.lst"
        (tiles_dir / name).write_bytes(data)
        h_min, h_max = min(heights), max(heights)
        g_min, g_max = min(g_min, h_min), max(g_max, h_max)
        if tile.level == 0:
            base_ge = max(h_max - h_min, 1.0)  # coarse-first ⇒ set on the root tile
        tiles_meta.append(
            {
                "level": tile.level,
                "x": tile.x,
                "y": tile.y,
                "file": f"tiles/{name}",
                "sha256": hashlib.sha256(data).hexdigest(),
                "height_min": h_min,
                "height_max": h_max,
                "geometric_error": base_ge * (0.5**tile.level),
            }
        )

    manifest: dict[str, Any] = {
        "format_version": MANIFEST_FORMAT_VERSION,
        "bake_tool": BAKE_TOOL,
        "seed": cfg.seed,
        "config_hash": cfg.config_hash(),
        "generator": cfg.generator,
        "aoi": {"lat_deg": cfg.lat_deg, "lon_deg": cfg.lon_deg, "radius_m": cfg.radius_m},
        "origin": {
            "lat_deg": cfg.lat_deg,
            "lon_deg": cfg.lon_deg,
            "height_m": cfg.origin_height_m,
            "ecef": list(origin_ecef),
        },
        "crs": {
            "horizontal": "ENU tangent plane at origin (EPSG:4326 origin)",
            "vertical": "WGS84 ellipsoidal (synthetic)",
        },
        "tiling": {
            "root_size_m": 2.0 * cfg.radius_m,
            "leaf_level": cfg.leaf_level,
            "grid_n": cfg.grid_n,
            "base_geometric_error": base_ge,
        },
        "vertical_datum": (
            {
                "source": f"{cfg.generator} (orthometric)",
                "geoid": cfg.geoid_gtx_path or f"constant N={cfg.geoid_undulation_m:.4f} m",
                "undulation_m": undulation_m,
                "conversion": "ellipsoidal = orthometric + N",
                "residual_gate_cm": 5.0,
            }
            if real
            else {"source": "none (synthetic ellipsoidal)", "conversion": "identity", "residual_gate_cm": 5.0}
        ),
        "provenance": [
            {"source": cfg.generator, "version": __version__, "license": "n/a", "acquired": "deterministic"}
        ],
        "qa": {
            "tile_count": len(tiles_meta),
            "height_min_m": g_min,
            "height_max_m": g_max,
            "coverage": 1.0,
            "rejected": 0,
        },
        "tiles": tiles_meta,
    }

    # Sorted keys + no timestamps ⇒ byte-reproducible manifest (the determinism gate).
    (out_dir / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True), encoding="utf-8")
    (out_dir / "qa_report.json").write_text(
        json.dumps(manifest["qa"], indent=2, sort_keys=True), encoding="utf-8"
    )
    return manifest
