"""WGS84 geodesy — geodetic → geocentric ECEF.

A faithful mirror of `Rust/crates/logi_geospatial` (`cartographic_to_cartesian`) so the bake's manifest
origin ECEF agrees byte-for-purpose with the engine's geodesy. A Rust dev-test cross-checks the two.
"""

from __future__ import annotations

import math
import struct
from dataclasses import dataclass, field

WGS84_SEMI_MAJOR_AXIS = 6_378_137.0
WGS84_SEMI_MINOR_AXIS = 6_356_752.314_245_179


def geodetic_surface_normal(longitude: float, latitude: float) -> tuple[float, float, float]:
    """Unit geodetic surface normal (ECEF) at a lon/lat in **radians**."""
    cos_lat = math.cos(latitude)
    return (cos_lat * math.cos(longitude), cos_lat * math.sin(longitude), math.sin(latitude))


def cartographic_to_cartesian(lon_deg: float, lat_deg: float, height_m: float) -> tuple[float, float, float]:
    """Geodetic (degrees, metres) → geocentric ECEF metres (mirrors logi_geospatial)."""
    lon = math.radians(lon_deg)
    lat = math.radians(lat_deg)
    n = geodetic_surface_normal(lon, lat)
    a2 = WGS84_SEMI_MAJOR_AXIS * WGS84_SEMI_MAJOR_AXIS
    b2 = WGS84_SEMI_MINOR_AXIS * WGS84_SEMI_MINOR_AXIS
    radii_sq = (a2, a2, b2)
    k = (radii_sq[0] * n[0], radii_sq[1] * n[1], radii_sq[2] * n[2])
    gamma = math.sqrt(n[0] * k[0] + n[1] * k[1] + n[2] * k[2])
    return (
        k[0] / gamma + n[0] * height_m,
        k[1] / gamma + n[1] * height_m,
        k[2] / gamma + n[2] * height_m,
    )


# ---- Vertical datum (the HE-0 ≤5 cm correctness gate) ---------------------------------------------
# Real LiDAR (USGS 3DEP) reports NAVD88 *orthometric* heights; the globe is WGS84 *ellipsoidal*. The
# conversion is h = H + N, where N is the geoid undulation (GEOID18). Over a 1-mile AOI N varies < 1 cm,
# so the bake uses one undulation at the AOI centre. Getting this wrong floats/sinks the whole AOI.


@dataclass(frozen=True)
class GeoidModel:
    """Geoid undulation N (metres) = ellipsoidal − orthometric. Either a constant (small AOI) or a regular
    lat/lon grid (bilinear-interpolated). Load a real grid via `from_gtx` (NOAA GEOID18 `.gtx`)."""

    constant_m: float | None = None
    lat0: float = 0.0
    lon0: float = 0.0
    dlat: float = 0.0
    dlon: float = 0.0
    rows: int = 0
    cols: int = 0
    values: tuple[float, ...] = field(default=())

    @classmethod
    def constant(cls, undulation_m: float) -> GeoidModel:
        return cls(constant_m=undulation_m)

    @classmethod
    def from_grid(
        cls, lat0: float, lon0: float, dlat: float, dlon: float, rows: int, cols: int, values: list[float]
    ) -> GeoidModel:
        if len(values) != rows * cols:
            raise ValueError(f"grid size mismatch: {len(values)} != {rows * cols}")
        return cls(None, lat0, lon0, dlat, dlon, rows, cols, tuple(values))

    @classmethod
    def from_gtx(cls, path: str) -> GeoidModel:
        """Parse a NOAA `.gtx` geoid grid: big-endian header (lat0, lon0, dlat, dlon as f64; rows, cols as
        i32) then a row-major f32 grid. (Needs the grid file — not exercised by the synthetic path.)"""
        with open(path, "rb") as f:
            header = f.read(40)
            lat0, lon0, dlat, dlon = struct.unpack(">dddd", header[:32])
            rows, cols = struct.unpack(">ii", header[32:40])
            count = rows * cols
            values = list(struct.unpack(f">{count}f", f.read(4 * count)))
        return cls.from_grid(lat0, lon0, dlat, dlon, rows, cols, values)

    def undulation(self, lat_deg: float, lon_deg: float) -> float:
        if self.constant_m is not None:
            return self.constant_m
        fj = (lat_deg - self.lat0) / self.dlat
        fi = (lon_deg - self.lon0) / self.dlon
        j0 = max(0, min(self.rows - 2, int(math.floor(fj))))
        i0 = max(0, min(self.cols - 2, int(math.floor(fi))))
        tj, ti = fj - j0, fi - i0

        def g(j: int, i: int) -> float:
            return self.values[j * self.cols + i]

        top = g(j0, i0) * (1.0 - ti) + g(j0, i0 + 1) * ti
        bot = g(j0 + 1, i0) * (1.0 - ti) + g(j0 + 1, i0 + 1) * ti
        return top * (1.0 - tj) + bot * tj


def orthometric_to_ellipsoidal(orthometric_m: float, undulation_m: float) -> float:
    """NAVD88 orthometric height H → WGS84 ellipsoidal height h = H + N."""
    return orthometric_m + undulation_m


def datum_residual_cm(computed_ellipsoidal_m: float, benchmark_ellipsoidal_m: float) -> float:
    """Vertical residual (cm) between a computed and a benchmark ellipsoidal height — the HE-0 ≤5 cm gate."""
    return abs(computed_ellipsoidal_m - benchmark_ellipsoidal_m) * 100.0
