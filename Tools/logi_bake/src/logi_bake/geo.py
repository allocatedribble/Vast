"""WGS84 geodesy — geodetic → geocentric ECEF.

A faithful mirror of `Rust/crates/logi_geospatial` (`cartographic_to_cartesian`) so the bake's manifest
origin ECEF agrees byte-for-purpose with the engine's geodesy. A Rust dev-test cross-checks the two.
"""

from __future__ import annotations

import math

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
