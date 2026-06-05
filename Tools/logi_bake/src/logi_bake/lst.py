"""The Logi Semantic Tile (`.lst`) binary writer — the cross-language contract with
`Rust/crates/logi_semantic/src/format.rs` (they MUST agree byte-for-byte).

Layout (little-endian, no implicit padding): a 58-byte header, then `grid_n²` `f32` heights
(row-major south→north, west→east), then a trailing zlib CRC-32 over the header+heights.
"""

from __future__ import annotations

import struct
import zlib

LST_MAGIC = b"LSTL"
LST_VERSION = 1

# 58-byte header: magic, version, flags, level, _pad0, x, y, grid_n, _pad1, origin_e/n, size_m, h_min/max.
_HEADER = struct.Struct("<4sHHBBIIIIdddff")
assert _HEADER.size == 58, _HEADER.size


def f32(x: float) -> float:
    """Round a Python float to the nearest IEEE-754 f32 value (so the grid bytes are canonical and the
    in-file h_min/h_max match the stored heights — i.e. exactly what Rust reads)."""
    return struct.unpack("<f", struct.pack("<f", x))[0]


def encode_tile(
    level: int,
    x: int,
    y: int,
    grid_n: int,
    origin_e: float,
    origin_n: float,
    size_m: float,
    heights: list[float],
) -> bytes:
    """Encode one tile to the `.lst` byte layout. `heights` must be `grid_n²` f32-valued floats."""
    count = grid_n * grid_n
    if len(heights) != count:
        raise ValueError(f"expected {count} heights, got {len(heights)}")
    h_min = min(heights)
    h_max = max(heights)
    header = _HEADER.pack(
        LST_MAGIC, LST_VERSION, 0, level, 0, x, y, grid_n, 0, float(origin_e), float(origin_n), float(size_m), h_min, h_max
    )
    body = header + struct.pack(f"<{count}f", *heights)
    crc = zlib.crc32(body) & 0xFFFFFFFF
    return body + struct.pack("<I", crc)


def decode_tile(data: bytes) -> dict[str, object]:
    """Decode + validate a `.lst` (mirrors the Rust decoder) — used by the tests for round-trip checks."""
    if len(data) < _HEADER.size + 4:
        raise ValueError("truncated tile")
    (magic, version, _flags, level, _pad0, x, y, grid_n, _pad1, origin_e, origin_n, size_m, h_min, h_max) = (
        _HEADER.unpack_from(data, 0)
    )
    if magic != LST_MAGIC:
        raise ValueError("bad magic")
    if version != LST_VERSION:
        raise ValueError(f"unsupported version {version}")
    count = grid_n * grid_n
    body_len = _HEADER.size + count * 4
    if len(data) != body_len + 4:
        raise ValueError("length mismatch")
    (stored_crc,) = struct.unpack_from("<I", data, body_len)
    if (zlib.crc32(data[:body_len]) & 0xFFFFFFFF) != stored_crc:
        raise ValueError("crc mismatch")
    heights = list(struct.unpack_from(f"<{count}f", data, _HEADER.size))
    return {
        "level": level,
        "x": x,
        "y": y,
        "grid_n": grid_n,
        "origin_e": origin_e,
        "origin_n": origin_n,
        "size_m": size_m,
        "h_min": h_min,
        "h_max": h_max,
        "heights": heights,
    }
