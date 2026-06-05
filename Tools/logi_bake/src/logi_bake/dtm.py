"""Digital terrain model sources. HE-0 ships a **deterministic synthetic** DTM (a smooth procedural
surface seeded from the config) so the whole bake→store→render loop is provable without multi-GB LiDAR.
The real LiDAR path (`LidarDtm`) is a guarded stub — it needs the `[lidar]` extra (PDAL) and the
acquire/ground-filter stages of plan §3.B; it raises a clear error until that lands."""

from __future__ import annotations

import math
from typing import Protocol

MASK64 = (1 << 64) - 1


def splitmix64(x: int) -> int:
    """Deterministic 64-bit mixer (same constants as the kernel's `logi_3dtiles_io::splitmix64`)."""
    x = (x + 0x9E3779B97F4A7C15) & MASK64
    x = ((x ^ (x >> 30)) * 0xBF58476D1CE4E5B9) & MASK64
    x = ((x ^ (x >> 27)) * 0x94D049BB133111EB) & MASK64
    return x ^ (x >> 31)


class Dtm(Protocol):
    """Height (ellipsoidal up, metres) at an ENU (east, north) offset from the AOI origin."""

    def height(self, east_m: float, north_m: float) -> float: ...


class SyntheticDtm:
    """A smooth, deterministic procedural surface: a gentle base + slope + two sinusoids whose
    wavelengths/phase derive from the seed. Pure, stdlib-only, reproducible."""

    def __init__(self, seed: int, base_m: float = 20.0, amplitude_m: float = 8.0) -> None:
        import math

        h = splitmix64(seed)
        self._base = base_m
        self._amp = amplitude_m
        self._kx = 2.0 * math.pi / (300.0 + float(h & 0xFF))
        self._ky = 2.0 * math.pi / (350.0 + float((h >> 8) & 0xFF))
        self._phase = float((h >> 16) & 0xFFFF) / 65535.0 * 2.0 * math.pi
        self._slope = float((h >> 32) & 0xFF) / 255.0 * 0.01  # <= 1% grade

    def height(self, east_m: float, north_m: float) -> float:
        import math

        return (
            self._base
            + self._slope * east_m
            + self._amp * math.sin(self._kx * east_m + self._phase) * math.cos(self._ky * north_m)
        )


class LidarDtm:
    """Real DTM from a LiDAR point cloud (LAZ) via **PDAL** (plan §3.B): SMRF ground classification → a
    regular DTM grid (min-Z per cell ≈ bare earth) → bilinear sample, converting NAVD88 orthometric →
    WGS84 ellipsoidal with `undulation_m` (the geoid, §geo). Needs the `[lidar]` extra (pdal, numpy).

    Prerequisite: the point cloud must already be in the **AOI ENU frame** (metres, East/North, origin at
    the AOI centre) — the acquire stage (plan §3.A: reproject from the source CRS + recentre) arranges
    this. Until that stage + real data exist, instantiating without PDAL raises a clear error."""

    def __init__(self, laz_path: str, undulation_m: float = 0.0, cell_m: float = 1.0) -> None:
        try:
            import numpy  # noqa: F401
            import pdal  # noqa: F401
        except ImportError as exc:
            raise NotImplementedError(
                "The LiDAR DTM path needs the [lidar] extra (`pip install -e .[lidar]`); PDAL/numpy not "
                "found. Use --generator synthetic for the HE-0 vertical slice."
            ) from exc
        self._undulation = undulation_m
        self._cell = cell_m
        self._build(laz_path)

    def _build(self, laz_path: str) -> None:
        import json

        import numpy as np
        import pdal

        # SMRF (Simple Morphological Filter) classifies ground; keep class 2 (ground) only.
        pipeline = {
            "pipeline": [
                str(laz_path),
                {"type": "filters.smrf"},
                {"type": "filters.range", "limits": "Classification[2:2]"},
            ]
        }
        p = pdal.Pipeline(json.dumps(pipeline))
        p.execute()
        pts = p.arrays[0]
        x = np.asarray(pts["X"], dtype=float)
        y = np.asarray(pts["Y"], dtype=float)
        z = np.asarray(pts["Z"], dtype=float)
        if x.size == 0:
            raise ValueError("LiDAR ground filter returned no points")
        self._x0, self._y0 = float(x.min()), float(y.min())
        self._cols = max(2, int(math.ceil((x.max() - self._x0) / self._cell)) + 1)
        self._rows = max(2, int(math.ceil((y.max() - self._y0) / self._cell)) + 1)
        grid = np.full((self._rows, self._cols), np.nan)
        ci = np.clip(((x - self._x0) / self._cell).astype(int), 0, self._cols - 1)
        ri = np.clip(((y - self._y0) / self._cell).astype(int), 0, self._rows - 1)
        for r, c, zz in zip(ri, ci, z):
            if np.isnan(grid[r, c]) or zz < grid[r, c]:
                grid[r, c] = zz  # min-Z per cell ≈ bare earth
        # Fill empty cells with the global mean (production: rasterio.fill.fillnodata / IDW).
        mean_z = float(np.nanmean(grid))
        grid[np.isnan(grid)] = mean_z
        self._grid = grid

    def height(self, east_m: float, north_m: float) -> float:
        fc = (east_m - self._x0) / self._cell
        fr = (north_m - self._y0) / self._cell
        c0 = max(0, min(self._cols - 2, int(math.floor(fc))))
        r0 = max(0, min(self._rows - 2, int(math.floor(fr))))
        tc, tr = fc - c0, fr - r0
        g = self._grid
        top = g[r0][c0] * (1 - tc) + g[r0][c0 + 1] * tc
        bot = g[r0 + 1][c0] * (1 - tc) + g[r0 + 1][c0 + 1] * tc
        ortho = float(top * (1 - tr) + bot * tr)
        return ortho + self._undulation  # NAVD88 orthometric → WGS84 ellipsoidal


def make_dtm(generator: str, seed: int, laz_path: str = "", undulation_m: float = 0.0) -> Dtm:
    if generator == "synthetic":
        return SyntheticDtm(seed)  # already ellipsoidal; undulation not applied
    if generator == "lidar":
        return LidarDtm(laz_path, undulation_m)
    raise ValueError(f"unknown generator {generator!r}")
