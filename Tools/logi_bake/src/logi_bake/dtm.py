"""Digital terrain model sources. HE-0 ships a **deterministic synthetic** DTM (a smooth procedural
surface seeded from the config) so the whole bake→store→render loop is provable without multi-GB LiDAR.
The real LiDAR path (`LidarDtm`) is a guarded stub — it needs the `[lidar]` extra (PDAL) and the
acquire/ground-filter stages of plan §3.B; it raises a clear error until that lands."""

from __future__ import annotations

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
    """Real DTM from USGS 3DEP QL2 LiDAR (plan §3.B). Guarded stub: requires the `[lidar]` extra + the
    acquire/ground-filter implementation."""

    def __init__(self, *_args: object, **_kwargs: object) -> None:
        raise NotImplementedError(
            "The LiDAR DTM path is not implemented for HE-0. Install the real-data deps "
            "(`pip install -e .[lidar]`) and implement plan §3.B (PDAL ground filter → DTM). "
            "Use --generator synthetic for the HE-0 vertical slice."
        )

    def height(self, east_m: float, north_m: float) -> float:  # pragma: no cover - stub
        raise NotImplementedError


def make_dtm(generator: str, seed: int) -> Dtm:
    if generator == "synthetic":
        return SyntheticDtm(seed)
    if generator == "lidar":
        return LidarDtm()
    raise ValueError(f"unknown generator {generator!r}")
