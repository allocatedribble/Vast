"""AOI → local quadtree leaves. The AOI origin is the ENU frame origin; the bbox is the `2·radius`
square centred on it, split into `2^leaf_level` tiles per side. Each tile records its SW-corner ENU
offset (metres from the origin) and metric size. (Tiles cover the full bbox; circle-culling is a later
refinement.)"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class Tile:
    level: int
    x: int  # east index, 0..2^level
    y: int  # north index
    origin_e: float  # ENU east of the SW corner, metres from the AOI origin
    origin_n: float  # ENU north of the SW corner
    size_m: float  # square tile extent, metres


def leaf_tiles(radius_m: float, level: int) -> list[Tile]:
    """The `2^level × 2^level` tiles tiling the `2·radius` bbox at one level, in (y, x) row-major order
    (south→north, west→east) — matching the heightfield sample order."""
    per_side = 1 << level
    size = (2.0 * radius_m) / per_side
    tiles: list[Tile] = []
    for y in range(per_side):
        for x in range(per_side):
            origin_e = -radius_m + x * size
            origin_n = -radius_m + y * size
            tiles.append(Tile(level, x, y, origin_e, origin_n, size))
    return tiles


def pyramid_tiles(radius_m: float, leaf_level: int) -> list[Tile]:
    """Every tile of the quadtree pyramid (levels 0..leaf_level) — one coarse root through the fine leaves,
    coarse-first. Each level samples the same AOI at double the previous resolution; the runtime SSE
    selector picks the coarsest level that meets the screen-space-error target per region."""
    tiles: list[Tile] = []
    for level in range(leaf_level + 1):
        tiles.extend(leaf_tiles(radius_m, level))
    return tiles
