"""HE-0 bake tests (stdlib `unittest`): format round-trip, geodesy cross-check, tiling, and the two
production gates — golden structure + deterministic (byte-identical) re-bake."""

from __future__ import annotations

import tempfile
import unittest
import zlib
from pathlib import Path

from logi_bake.bake import bake
from logi_bake.config import BakeConfig
from logi_bake.geo import WGS84_SEMI_MAJOR_AXIS, WGS84_SEMI_MINOR_AXIS, cartographic_to_cartesian
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
            self.assertEqual(m["qa"]["tile_count"], 4)  # 2^1 squared
            self.assertEqual(len(m["tiles"]), 4)
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


if __name__ == "__main__":
    unittest.main()
