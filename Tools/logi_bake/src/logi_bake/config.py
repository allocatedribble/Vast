"""The bake configuration — a frozen, hashable record. Its `config_hash` (with the seed + tool version)
is what makes a bake reproducible and lets the determinism gate assert byte-identity."""

from __future__ import annotations

import hashlib
import json
from dataclasses import asdict, dataclass


@dataclass(frozen=True)
class BakeConfig:
    """All inputs that determine a bake's output. Same config + seed + tool version ⇒ identical store."""

    lat_deg: float
    lon_deg: float
    radius_m: float
    origin_height_m: float = 0.0
    leaf_level: int = 3
    grid_n: int = 65
    seed: int = 42
    generator: str = "synthetic"  # synthetic | terrarium (download) | dem (GeoTIFF) | lidar ([lidar] extra)
    # Real-world elevation paths:
    laz_path: str = ""  # LiDAR point cloud (LAZ) in the AOI ENU frame, for generator="lidar"
    dem_path: str = ""  # local DEM GeoTIFF (e.g. USGS 3DEP), for generator="dem"
    zoom: int = 14  # AWS terrarium tile zoom for generator="terrarium" (z14 ≈ 7.6 m/px)
    geoid_undulation_m: float = 0.0  # constant geoid N (m) for the orthometric→ellipsoid datum conversion
    geoid_gtx_path: str = ""  # NOAA GEOID18 .gtx grid (overrides the constant when set)

    def __post_init__(self) -> None:
        if self.grid_n < 2:
            raise ValueError("grid_n must be >= 2")
        if self.leaf_level < 0:
            raise ValueError("leaf_level must be >= 0")
        if self.radius_m <= 0:
            raise ValueError("radius_m must be > 0")
        if self.generator not in ("synthetic", "terrarium", "dem", "lidar"):
            raise ValueError(f"unknown generator {self.generator!r}")
        if self.generator == "lidar" and not self.laz_path:
            raise ValueError("generator='lidar' requires laz_path")
        if self.generator == "dem" and not self.dem_path:
            raise ValueError("generator='dem' requires dem_path")

    def config_hash(self) -> str:
        """Stable SHA-256 over the canonical (sorted) config — part of the manifest + the determinism gate."""
        payload = json.dumps(asdict(self), sort_keys=True, separators=(",", ":")).encode("utf-8")
        return hashlib.sha256(payload).hexdigest()
