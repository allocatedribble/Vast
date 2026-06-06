"""HE-0 bake tests (stdlib `unittest`): format round-trip, geodesy cross-check, tiling, and the two
production gates — golden structure + deterministic (byte-identical) re-bake."""

from __future__ import annotations

import tempfile
import unittest
import zlib
from pathlib import Path

from logi_bake.bake import bake
from logi_bake.config import BakeConfig
from logi_bake.dtm import make_dtm
from logi_bake.geo import (
    WGS84_SEMI_MAJOR_AXIS,
    WGS84_SEMI_MINOR_AXIS,
    GeoidModel,
    cartographic_to_cartesian,
    datum_residual_cm,
    orthometric_to_ellipsoidal,
)
from logi_bake.lst import LST_VERSION, decode_tile, encode_tile, f32
from logi_bake.tiling import leaf_tiles


class FormatTests(unittest.TestCase):
    def test_roundtrip_and_size(self) -> None:
        grid_n = 4
        heights = [f32(k * 0.5) for k in range(grid_n * grid_n)]
        data = encode_tile(2, 1, 3, grid_n, -100.0, 200.0, 64.0, heights)
        self.assertEqual(len(data), 58 + grid_n * grid_n * 4 + 4)
        self.assertEqual(data[:4], b"LSTL")
        d = decode_tile(data)
        self.assertEqual((d["level"], d["x"], d["y"], d["grid_n"]), (2, 1, 3, grid_n))
        self.assertEqual(d["heights"], heights)
        self.assertEqual(d["origin_e"], -100.0)

    def test_crc_is_zlib_over_body(self) -> None:
        data = encode_tile(0, 0, 0, 2, 0.0, 0.0, 1.0, [0.0, 1.0, 2.0, 3.0])
        body, crc = data[:-4], int.from_bytes(data[-4:], "little")
        self.assertEqual(crc, zlib.crc32(body) & 0xFFFFFFFF)

    def test_decode_detects_corruption(self) -> None:
        data = bytearray(encode_tile(0, 0, 0, 2, 0.0, 0.0, 1.0, [0.0, 0.0, 0.0, 0.0]))
        data[60] ^= 0xFF
        with self.assertRaises(ValueError):
            decode_tile(bytes(data))

    def test_version_constant(self) -> None:
        self.assertEqual(LST_VERSION, 1)


class GeoTests(unittest.TestCase):
    def test_equator_prime_meridian(self) -> None:
        x, y, z = cartographic_to_cartesian(0.0, 0.0, 0.0)
        self.assertAlmostEqual(x, WGS84_SEMI_MAJOR_AXIS, places=3)
        self.assertAlmostEqual(y, 0.0, places=3)
        self.assertAlmostEqual(z, 0.0, places=3)

    def test_north_pole(self) -> None:
        x, y, z = cartographic_to_cartesian(0.0, 90.0, 0.0)
        self.assertAlmostEqual(z, WGS84_SEMI_MINOR_AXIS, places=3)
        self.assertAlmostEqual(x, 0.0, places=3)
        self.assertAlmostEqual(y, 0.0, places=3)


class TilingTests(unittest.TestCase):
    def test_leaf_level_1_covers_bbox(self) -> None:
        tiles = leaf_tiles(100.0, 1)
        self.assertEqual(len(tiles), 4)
        sw = tiles[0]
        self.assertEqual((sw.x, sw.y), (0, 0))
        self.assertAlmostEqual(sw.origin_e, -100.0)
        self.assertAlmostEqual(sw.origin_n, -100.0)
        self.assertAlmostEqual(sw.size_m, 100.0)


class BakeGateTests(unittest.TestCase):
    CFG = BakeConfig(lat_deg=39.643178, lon_deg=-75.617567, radius_m=200.0, leaf_level=1, grid_n=9, seed=42)

    def test_structure_and_golden_shape(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            m = bake(self.CFG, Path(d))
            self.assertEqual(m["format_version"], 1)
            self.assertEqual(m["qa"]["tile_count"], 5)  # pyramid: level 0 (1) + level 1 (4)
            self.assertEqual(len(m["tiles"]), 5)
            self.assertEqual({t["level"] for t in m["tiles"]}, {0, 1}, "pyramid has both levels")
            self.assertTrue(all("geometric_error" in t for t in m["tiles"]))
            ge = {t["level"]: t["geometric_error"] for t in m["tiles"]}
            self.assertAlmostEqual(ge[1], ge[0] * 0.5, msg="geometric error halves per level")
            # origin ECEF agrees with the same geodesy the engine uses.
            ex, ey, ez = cartographic_to_cartesian(self.CFG.lon_deg, self.CFG.lat_deg, 0.0)
            self.assertAlmostEqual(m["origin"]["ecef"][0], ex, places=3)
            self.assertAlmostEqual(m["origin"]["ecef"][2], ez, places=3)
            # every tile file exists and decodes (CRC valid).
            for t in m["tiles"]:
                data = (Path(d) / t["file"]).read_bytes()
                dec = decode_tile(data)
                self.assertEqual(dec["grid_n"], 9)

    def test_rebake_is_byte_identical(self) -> None:
        with tempfile.TemporaryDirectory() as d1, tempfile.TemporaryDirectory() as d2:
            bake(self.CFG, Path(d1))
            bake(self.CFG, Path(d2))
            files = sorted(p.name for p in (Path(d1) / "tiles").iterdir())
            self.assertEqual(files, sorted(p.name for p in (Path(d2) / "tiles").iterdir()))
            for name in files:
                self.assertEqual(
                    (Path(d1) / "tiles" / name).read_bytes(),
                    (Path(d2) / "tiles" / name).read_bytes(),
                    f"tile {name} not reproducible",
                )
            self.assertEqual(
                (Path(d1) / "manifest.json").read_bytes(),
                (Path(d2) / "manifest.json").read_bytes(),
                "manifest not reproducible",
            )


class DatumTests(unittest.TestCase):
    """The HE-0 vertical-datum (NAVD88 orthometric → WGS84 ellipsoidal) conversion + ≤5 cm gate."""

    def test_constant_geoid_and_conversion(self) -> None:
        g = GeoidModel.constant(-34.0)
        self.assertEqual(g.undulation(39.643, -75.617), -34.0)
        # 50 m orthometric + N(-34) = 16 m ellipsoidal
        self.assertAlmostEqual(orthometric_to_ellipsoidal(50.0, g.undulation(39.6, -75.6)), 16.0)

    def test_grid_geoid_bilinear(self) -> None:
        # 2x2 grid over lat/lon [0..1]: corners g(0,0)=0 g(0,1)=10 g(1,0)=20 g(1,1)=30.
        g = GeoidModel.from_grid(0.0, 0.0, 1.0, 1.0, 2, 2, [0.0, 10.0, 20.0, 30.0])
        self.assertAlmostEqual(g.undulation(0.0, 0.0), 0.0)
        self.assertAlmostEqual(g.undulation(1.0, 1.0), 30.0)
        self.assertAlmostEqual(g.undulation(0.5, 0.5), 15.0)  # centre = mean
        self.assertAlmostEqual(g.undulation(0.0, 0.5), 5.0)

    def test_residual_gate(self) -> None:
        self.assertAlmostEqual(datum_residual_cm(16.00, 16.03), 3.0)
        self.assertTrue(datum_residual_cm(16.00, 16.03) <= 5.0)
        self.assertFalse(datum_residual_cm(16.00, 16.10) <= 5.0)

    def test_lidar_path_needs_deps(self) -> None:
        # PDAL/numpy aren't installed here → a clear NotImplementedError (not a silent bad bake).
        with self.assertRaises(NotImplementedError):
            make_dtm("lidar", 0, laz_path="nonexistent.laz", undulation_m=-34.0)

    def test_config_lidar_requires_laz(self) -> None:
        with self.assertRaises(ValueError):
            BakeConfig(lat_deg=39.6, lon_deg=-75.6, radius_m=100.0, generator="lidar")


def _has_geo_deps() -> bool:
    try:
        import numpy, PIL, pyproj  # noqa: F401

        return True
    except ImportError:
        return False


@unittest.skipUnless(_has_geo_deps(), "needs the [geo] extra (numpy/Pillow/pyproj)")
class TerrariumMosaicTests(unittest.TestCase):
    """The real-elevation `TerrariumDem` mosaic sampler, tested offline (no network): a fake source whose
    tiles form one continuous global ramp must sample back as a smooth, strictly-increasing, seam-free
    surface — the regression net for the vectorized mosaic rewrite (no per-point loop; correct across the
    source-tile seams a 1-mile AOI straddles)."""

    def test_mosaic_is_vectorized_and_seam_continuous(self) -> None:
        import numpy as np

        from logi_bake.acquire import TerrariumDem

        class RampTerrarium(TerrariumDem):
            # Offline: each source tile is an east ramp of its GLOBAL pixel-x, so the assembled mosaic is
            # one continuous global ramp. A seam/clamp bug would show as a discontinuity or a flat step.
            def _fetch_tile(self, tx: int, ty: int):  # type: ignore[override]
                return np.tile(tx * 256.0 + np.arange(256.0), (256, 1))

        dem = RampTerrarium(39.643178, -75.617567, 1609.34, zoom=14)
        easts = np.linspace(-1609.0, 1609.0, 400).reshape(1, -1)
        norths = np.zeros((1, 400))
        h = dem.heights_grid(easts, norths)
        self.assertEqual(h.shape, (1, 400))
        self.assertTrue(np.all(np.isfinite(h)))
        diffs = np.diff(h[0])
        self.assertTrue(np.all(diffs > 0.0), "east ramp must be strictly increasing")
        self.assertLess(float(diffs.max()), 5.0 * float(diffs.mean()), "no seam discontinuity across tiles")
        # The scalar height() wrapper agrees with the grid path at the same point.
        self.assertAlmostEqual(
            dem.height(12.0, -7.0),
            float(dem.heights_grid(np.array([[12.0]]), np.array([[-7.0]]))[0, 0]),
        )

    def test_mosaic_is_seam_continuous_along_north(self) -> None:
        # Companion to the east sweep: a NORTH ramp exercises the ty0 / row-stride mapping the east-only
        # ramp leaves untested (a flipped or mis-strided Y passes that test but would break here).
        import numpy as np

        from logi_bake.acquire import TerrariumDem

        class NorthRampTerrarium(TerrariumDem):
            # Each tile is a SOUTH ramp of its GLOBAL pixel-y (slippy-tile rows increase southward), so the
            # assembled mosaic is one continuous global vertical ramp.
            def _fetch_tile(self, tx: int, ty: int):  # type: ignore[override]
                col = (ty * 256.0 + np.arange(256.0)).reshape(256, 1)
                return np.tile(col, (1, 256))

        dem = NorthRampTerrarium(39.643178, -75.617567, 1609.34, zoom=14)
        norths = np.linspace(-1609.0, 1609.0, 400).reshape(-1, 1)
        easts = np.zeros((400, 1))
        h = dem.heights_grid(easts, norths)
        self.assertEqual(h.shape, (400, 1))
        self.assertTrue(np.all(np.isfinite(h)))
        diffs = np.diff(h[:, 0])
        # Going north decreases the global slippy-tile y, so the sampled south-ramp value strictly DECREASES.
        self.assertTrue(np.all(diffs < 0.0), "north sweep must be strictly monotonic (Y mapping correct)")
        self.assertLess(
            float(np.abs(diffs).max()),
            5.0 * float(np.abs(diffs).mean()),
            "no seam discontinuity across tiles north–south",
        )


if __name__ == "__main__":
    unittest.main()
