"""Real-world elevation sources (plan §3.A acquire stage). Each maps an AOI **ENU** offset (east, north
metres from the AOI origin) to an ellipsoidal height, reprojecting from the source CRS via pyproj and
applying the geoid undulation (NAVD88/orthometric → WGS84 ellipsoidal).

Two runnable sources (need the [geo] deps: rasterio, pyproj, numpy, Pillow):
- `TerrariumDem` — downloads AWS Terrain-Tiles "terrarium" PNG tiles (global, ~public, ~7.6 m at z14) and
  decodes elevation (`R*256 + G + B/256 − 32768`). The self-contained "real bake" path.
- `GeoTiffDem` — samples a local DEM GeoTIFF (e.g. a USGS 3DEP 1 m DTM) via rasterio.

The LiDAR/LAZ path (PDAL) lives in `dtm.py` and needs the (Windows-unfriendly) `[lidar]` extra.
"""

from __future__ import annotations

import io
import math
import urllib.request


def _utm_epsg(lon_deg: float) -> int:
    """WGS84 UTM-north EPSG for a longitude (the metric frame used to turn ENU offsets into lat/lon)."""
    zone = int((lon_deg + 180.0) / 6.0) + 1
    return 32600 + zone


class _EnuToLatLon:
    """ENU (east, north metres from the AOI origin) → (lon, lat) via a local UTM frame."""

    def __init__(self, lat0: float, lon0: float) -> None:
        from pyproj import Transformer

        epsg = _utm_epsg(lon0)
        self._to_utm = Transformer.from_crs("EPSG:4326", f"EPSG:{epsg}", always_xy=True)
        self._to_ll = Transformer.from_crs(f"EPSG:{epsg}", "EPSG:4326", always_xy=True)
        self.epsg = epsg
        self.e0, self.n0 = self._to_utm.transform(lon0, lat0)

    def lonlat(self, east_m: float, north_m: float) -> tuple[float, float]:
        return self._to_ll.transform(self.e0 + east_m, self.n0 + north_m)

    def lonlat_array(self, east_m, north_m):
        """Batched ENU→(lon, lat): pyproj transforms accept arrays, so a whole grid is ONE PROJ call
        instead of one per sample (the bake's dominant cost on real sources)."""
        return self._to_ll.transform(self.e0 + east_m, self.n0 + north_m)


class TerrariumDem:
    """Real elevation from AWS Terrain-Tiles (terrarium PNG). Tiles are fetched lazily + cached."""

    URL = "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/{z}/{x}/{y}.png"

    def __init__(self, lat0: float, lon0: float, radius_m: float, zoom: int = 14, undulation_m: float = 0.0) -> None:
        self._frame = _EnuToLatLon(lat0, lon0)
        self._zoom = int(zoom)
        self._undulation = undulation_m
        self._tiles: dict[tuple[int, int], object] = {}

    def _lonlat_to_tilepix(self, lon: float, lat: float) -> tuple[float, float]:
        n = float(2**self._zoom)
        tx = (lon + 180.0) / 360.0 * n
        lat_r = math.radians(lat)
        ty = (1.0 - math.log(math.tan(lat_r) + 1.0 / math.cos(lat_r)) / math.pi) / 2.0 * n
        return tx, ty

    def _tile(self, tx: int, ty: int):
        key = (tx, ty)
        cached = self._tiles.get(key)
        if cached is not None:
            return cached
        import numpy as np
        from PIL import Image

        url = self.URL.format(z=self._zoom, x=tx, y=ty)
        req = urllib.request.Request(url, headers={"User-Agent": "logi_bake/0.1"})
        with urllib.request.urlopen(req, timeout=30) as resp:
            data = resp.read()
        img = np.asarray(Image.open(io.BytesIO(data)).convert("RGB")).astype(np.float64)
        elev = img[:, :, 0] * 256.0 + img[:, :, 1] + img[:, :, 2] / 256.0 - 32768.0  # metres
        self._tiles[key] = elev
        return elev

    def height(self, east_m: float, north_m: float) -> float:
        lon, lat = self._frame.lonlat(east_m, north_m)
        return self._sample_lonlat(lon, lat)

    def _sample_lonlat(self, lon: float, lat: float) -> float:
        """Terrarium bilinear at one (lon, lat) — the per-point lookup, factored out so the vectorized
        `heights_grid` can reuse it after a single batched reprojection."""
        tx, ty = self._lonlat_to_tilepix(lon, lat)
        itx, ity = int(tx), int(ty)
        px, py = (tx - itx) * 256.0, (ty - ity) * 256.0
        elev = self._tile(itx, ity)
        x0 = min(254, int(px))
        y0 = min(254, int(py))
        fx, fy = px - x0, py - y0
        top = elev[y0][x0] * (1 - fx) + elev[y0][x0 + 1] * fx
        bot = elev[y0 + 1][x0] * (1 - fx) + elev[y0 + 1][x0 + 1] * fx
        return float(top * (1 - fy) + bot * fy) + self._undulation

    def heights_grid(self, easts_2d, norths_2d):
        """Vectorized sampling: ONE batched ENU→lon/lat transform for the whole grid (instead of a pyproj
        call per sample — the bake's dominant cost), then per-point terrarium bilinear (which may straddle
        source tiles). Returns a 2-D array of ellipsoidal heights matching the input shape."""
        import numpy as np

        ee = np.asarray(easts_2d, dtype=float)
        nn = np.asarray(norths_2d, dtype=float)
        lon, lat = self._frame.lonlat_array(ee.reshape(-1), nn.reshape(-1))
        lon = np.atleast_1d(np.asarray(lon, dtype=float))
        lat = np.atleast_1d(np.asarray(lat, dtype=float))
        out = np.array([self._sample_lonlat(float(lon[k]), float(lat[k])) for k in range(lon.size)])
        return out.reshape(ee.shape)


class GeoTiffDem:
    """Real elevation from a local DEM GeoTIFF (e.g. USGS 3DEP) via rasterio (nearest sample)."""

    def __init__(self, dem_path: str, lat0: float, lon0: float, undulation_m: float = 0.0) -> None:
        import rasterio
        from pyproj import Transformer

        self._undulation = undulation_m
        self._frame = _EnuToLatLon(lat0, lon0)
        self._ds = rasterio.open(dem_path)
        self._to_dem = Transformer.from_crs(f"EPSG:{self._frame.epsg}", self._ds.crs, always_xy=True)
        self._band = self._ds.read(1)
        self._nodata = self._ds.nodata

    def height(self, east_m: float, north_m: float) -> float:
        dx, dy = self._to_dem.transform(self._frame.e0 + east_m, self._frame.n0 + north_m)
        row, col = self._ds.index(dx, dy)
        h = 0.0
        if 0 <= row < self._band.shape[0] and 0 <= col < self._band.shape[1]:
            v = float(self._band[row][col])
            if self._nodata is None or v != self._nodata:
                h = v
        return h + self._undulation

    def heights_grid(self, easts_2d, norths_2d):
        """Vectorized sampling: ONE batched ENU→DEM-CRS transform + array index for the whole grid
        (instead of a pyproj call per sample). Returns a 2-D array of ellipsoidal heights matching the
        input shape."""
        import numpy as np

        ee = np.asarray(easts_2d, dtype=float)
        nn = np.asarray(norths_2d, dtype=float)
        dx, dy = self._to_dem.transform(self._frame.e0 + ee.reshape(-1), self._frame.n0 + nn.reshape(-1))
        rows, cols = self._ds.index(np.asarray(dx), np.asarray(dy))
        rows = np.atleast_1d(np.asarray(rows)).astype(int)
        cols = np.atleast_1d(np.asarray(cols)).astype(int)
        h_rows, w_cols = self._band.shape
        out = np.zeros(ee.size, dtype=float)
        for k in range(ee.size):
            r, c = int(rows[k]), int(cols[k])
            if 0 <= r < h_rows and 0 <= c < w_cols:
                v = float(self._band[r, c])
                if self._nodata is None or v != self._nodata:
                    out[k] = v
        out += self._undulation
        return out.reshape(ee.shape)
