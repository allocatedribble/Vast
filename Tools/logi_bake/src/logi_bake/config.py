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
    generator: str = "synthetic"  # "synthetic" (HE-0) | "lidar" (needs the [lidar] extra)

    def __post_init__(self) -> None:
        if self.grid_n < 2:
            raise ValueError("grid_n must be >= 2")
        if self.leaf_level < 0:
            raise ValueError("leaf_level must be >= 0")
        if self.radius_m <= 0:
            raise ValueError("radius_m must be > 0")
        if self.generator not in ("synthetic", "lidar"):
            raise ValueError(f"unknown generator {self.generator!r}")

    def config_hash(self) -> str:
        """Stable SHA-256 over the canonical (sorted) config — part of the manifest + the determinism gate."""
        payload = json.dumps(asdict(self), sort_keys=True, separators=(",", ":")).encode("utf-8")
        return hashlib.sha256(payload).hexdigest()
