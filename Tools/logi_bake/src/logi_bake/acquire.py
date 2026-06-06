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
from pathlib import Path


def _utm_epsg(lon_deg: float, lat_deg: float) -> int:
    """WGS84 UTM EPSG for a lon/lat (the metric frame used to turn ENU offsets into lat/lon). Picks the
    south-hemisphere zone (327xx) below the equator, north (326xx) on or above it — a north-only frame
    puts a ~10,000 km false-northing offset on a southern AOI."""
    zone = int((lon_deg + 180.0) / 6.0) + 1
    return (32700 if lat_deg < 0.0 else 32600) + zone


class _EnuToLatLon:
    """ENU (east, north metres from the AOI origin) → (lon, lat) via a local UTM frame."""

    def __init__(self, lat0: float, lon0: float) -> None:
        from pyproj import Transformer

        epsg = _utm_epsg(lon0, lat0)
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
    """Real elevation from AWS Terrain-Tiles (terrarium PNG; public, global, ~7.6 m at z14). Builds a tile
    **mosaic** covering the AOI once, then samples it **fully vectorized** — correct across source-tile
    seams (a 1-mile AOI spans several z14 tiles, which the old per-point edge clamp got wrong) and fast (no
    per-sample Python loop). Downloaded tiles are cached on disk (content-addressed by z/x/y; plan §3.A),
    so a re-bake is offline + byte-reproducible."""

    URL = "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/{z}/{x}/{y}.png"

    def __init__(
        self,
        lat0: float,
        lon0: float,
        radius_m: float,
        zoom: int = 14,
        undulation_m: float = 0.0,
        cache_dir: "Path | None" = None,
    ) -> None:
        self._frame = _EnuToLatLon(lat0, lon0)
        self._zoom = int(zoom)
        self._n = float(2**self._zoom)
        self._undulation = undulation_m
        self._cache_dir = Path(cache_dir) if cache_dir else None
        self._build_mosaic(radius_m)

    def _lonlat_to_tile(self, lon: float, lat: float) -> tuple[float, float]:
        """(lon, lat) → fractional slippy-tile coords (x east, y south) at this zoom."""
        tx = (lon + 180.0) / 360.0 * self._n
        lat_r = math.radians(lat)
        ty = (1.0 - math.log(math.tan(lat_r) + 1.0 / math.cos(lat_r)) / math.pi) / 2.0 * self._n
        return tx, ty

    def _build_mosaic(self, radius_m: float) -> None:
        """Fetch every z-tile overlapping the AOI bbox (+ a bilinear margin) into one contiguous height
        mosaic, recording its NW tile so a global pixel maps to a mosaic pixel."""
        import numpy as np

        r = radius_m * 1.05  # margin so edge samples + their bilinear neighbours stay inside the mosaic
        ce = np.array([-r, r, -r, r])
        cn = np.array([-r, -r, r, r])
        lons, lats = self._frame.lonlat_array(ce, cn)
        lons = np.atleast_1d(np.asarray(lons, dtype=float))
        lats = np.atleast_1d(np.asarray(lats, dtype=float))
        tx_nw, ty_nw = self._lonlat_to_tile(float(lons.min()), float(lats.max()))
        tx_se, ty_se = self._lonlat_to_tile(float(lons.max()), float(lats.min()))
        self._tx0 = int(math.floor(tx_nw))
        self._ty0 = int(math.floor(ty_nw))
        nx = int(math.floor(tx_se)) - self._tx0 + 1
        ny = int(math.floor(ty_se)) - self._ty0 + 1
        if nx < 1 or ny < 1:
            # An antimeridian-straddling AOI wraps the corner longitudes (tx_se lands west of tx0), and a
            # pole-adjacent / UTM-zone-edge AOI can project corners whose min/max don't bound the true tile
            # extent. Either way the span is degenerate — fail loud instead of sampling empty/garbage.
            raise ValueError(
                f"degenerate tile span (nx={nx}, ny={ny}): the AOI straddles the antimeridian or a "
                f"projection edge, which the contiguous-mosaic sampler does not support."
            )
        mosaic = np.empty((ny * 256, nx * 256), dtype=np.float64)
        for j in range(ny):
            for i in range(nx):
                tile = self._fetch_tile(self._tx0 + i, self._ty0 + j)
                mosaic[j * 256 : (j + 1) * 256, i * 256 : (i + 1) * 256] = tile
        self._mosaic = mosaic

    def _fetch_tile(self, tx: int, ty: int):
        """Decoded terrarium height tile (256×256, metres) from the on-disk cache if present, else AWS."""
        import numpy as np
        from PIL import Image

        png: "bytes | None" = None
        cpath = None
        if self._cache_dir is not None:
            cpath = self._cache_dir / f"terrarium_{self._zoom}_{tx}_{ty}.png"
            if cpath.exists():
                png = cpath.read_bytes()
        from_cache = png is not None
        if png is None:
            url = self.URL.format(z=self._zoom, x=tx, y=ty)
            req = urllib.request.Request(url, headers={"User-Agent": "logi_bake/0.1"})
            with urllib.request.urlopen(req, timeout=30) as resp:
                png = resp.read()
        # Decode (which VALIDATES the bytes are a real image) BEFORE trusting or caching them: an S3 200
        # with an XML/HTML error body, or a truncated read, raises here instead of poisoning the bake.
        img = np.asarray(Image.open(io.BytesIO(png)).convert("RGB")).astype(np.float64)
        # Cache only a freshly-downloaded, successfully-decoded tile, via a temp file + atomic rename so a
        # partial write can never become a poisoned "offline reproducible" cache entry on a later re-bake.
        if not from_cache and cpath is not None:
            self._cache_dir.mkdir(parents=True, exist_ok=True)
            tmp = cpath.with_name(cpath.name + ".tmp")
            tmp.write_bytes(png)
            tmp.replace(cpath)
        return img[:, :, 0] * 256.0 + img[:, :, 1] + img[:, :, 2] / 256.0 - 32768.0  # metres

    def height(self, east_m: float, north_m: float) -> float:
        import numpy as np

        return float(self.heights_grid(np.array([[east_m]]), np.array([[north_m]]))[0, 0])

    def heights_grid(self, easts_2d, norths_2d):
        """Fully vectorized bilinear sample of the AOI mosaic: one batched ENU→lon/lat, one batched
        lon/lat→mosaic-pixel, then numpy bilinear — no per-sample Python loop, and correct across
        source-tile seams (the mosaic is contiguous). Returns heights matching the input shape."""
        import numpy as np

        ee = np.asarray(easts_2d, dtype=float)
        nn = np.asarray(norths_2d, dtype=float)
        lon, lat = self._frame.lonlat_array(ee.reshape(-1), nn.reshape(-1))
        lon = np.atleast_1d(np.asarray(lon, dtype=float))
        lat = np.atleast_1d(np.asarray(lat, dtype=float))
        gx = (lon + 180.0) / 360.0 * self._n
        lat_r = np.radians(lat)
        gy = (1.0 - np.log(np.tan(lat_r) + 1.0 / np.cos(lat_r)) / np.pi) / 2.0 * self._n
        px = (gx - self._tx0) * 256.0
        py = (gy - self._ty0) * 256.0
        h, w = self._mosaic.shape
        x0 = np.clip(np.floor(px).astype(int), 0, w - 2)
        y0 = np.clip(np.floor(py).astype(int), 0, h - 2)
        fx = px - x0
        fy = py - y0
        m = self._mosaic
        top = m[y0, x0] * (1 - fx) + m[y0, x0 + 1] * fx
        bot = m[y0 + 1, x0] * (1 - fx) + m[y0 + 1, x0 + 1] * fx
        out = (top * (1 - fy) + bot * fy) + self._undulation
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
            # Reject non-finite samples (a float32 DEM's NaN nodata satisfies `v != nodata`, so a plain
            # nodata compare would leak NaN through — mirror of the heights_grid guard below).
            if math.isfinite(v) and (self._nodata is None or v != self._nodata):
                h = v
        return h + self._undulation

    def heights_grid(self, easts_2d, norths_2d):
        """Fully vectorized: one batched ENU→DEM-CRS transform + array index for the whole grid (no
        per-sample Python loop). Out-of-bounds / nodata samples read 0 before the geoid offset."""
        import numpy as np

        ee = np.asarray(easts_2d, dtype=float)
        nn = np.asarray(norths_2d, dtype=float)
        dx, dy = self._to_dem.transform(self._frame.e0 + ee.reshape(-1), self._frame.n0 + nn.reshape(-1))
        rows, cols = self._ds.index(np.asarray(dx), np.asarray(dy))
        rows = np.atleast_1d(np.asarray(rows)).astype(int)
        cols = np.atleast_1d(np.asarray(cols)).astype(int)
        h_rows, w_cols = self._band.shape
        valid = (rows >= 0) & (rows < h_rows) & (cols >= 0) & (cols < w_cols)
        vals = self._band[np.clip(rows, 0, h_rows - 1), np.clip(cols, 0, w_cols - 1)].astype(float)
        # Drop non-finite samples FIRST: a float32 DEM often uses NaN as nodata, and `NaN != NaN` is True,
        # so a bare nodata compare would treat NaN as valid and leak it into the heights / manifest min-max.
        valid &= np.isfinite(vals)
        if self._nodata is not None and math.isfinite(self._nodata):
            valid &= vals != self._nodata
        out = np.where(valid, vals, 0.0) + self._undulation
        return out.reshape(ee.shape)
