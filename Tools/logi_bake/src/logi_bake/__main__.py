"""CLI: `python -m logi_bake bake --out DIR [...]`. Defaults to the HE-0 hero AOI (New Castle County, DE)."""

from __future__ import annotations

import argparse
from pathlib import Path

from .bake import bake
from .config import BakeConfig


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="logi_bake", description="Offline semantic-tile bake (HE-0: terrain).")
    sub = parser.add_subparsers(dest="cmd", required=True)

    b = sub.add_parser("bake", help="bake an AOI into a tile store")
    b.add_argument("--lat", type=float, default=39.643178, help="AOI centre latitude (deg)")
    b.add_argument("--lon", type=float, default=-75.617567, help="AOI centre longitude (deg)")
    b.add_argument("--radius", type=float, default=1609.34, help="AOI radius (m); default 1 mile")
    b.add_argument("--origin-height", type=float, default=0.0, help="origin ellipsoidal height (m)")
    b.add_argument("--leaf-level", type=int, default=3, help="quadtree leaf level (2^level tiles/side)")
    b.add_argument("--grid-n", type=int, default=65, help="heightfield samples per tile side")
    b.add_argument("--seed", type=int, default=42, help="deterministic bake seed")
    b.add_argument("--generator", choices=["synthetic", "terrarium", "dem", "lidar"], default="synthetic")
    b.add_argument("--zoom", type=int, default=14, help="terrarium tile zoom (generator=terrarium)")
    b.add_argument("--dem", type=str, default="", help="DEM GeoTIFF path (generator=dem)")
    b.add_argument("--laz", type=str, default="", help="LiDAR LAZ path (generator=lidar)")
    b.add_argument("--geoid-undulation", type=float, default=0.0, help="constant geoid N (m) for real DEMs")
    b.add_argument("--geoid-gtx", type=str, default="", help="NOAA GEOID18 .gtx grid (overrides constant)")
    b.add_argument("--out", type=Path, required=True, help="output store directory")

    args = parser.parse_args(argv)
    if args.cmd == "bake":
        cfg = BakeConfig(
            lat_deg=args.lat,
            lon_deg=args.lon,
            radius_m=args.radius,
            origin_height_m=args.origin_height,
            leaf_level=args.leaf_level,
            grid_n=args.grid_n,
            seed=args.seed,
            generator=args.generator,
            laz_path=args.laz,
            dem_path=args.dem,
            zoom=args.zoom,
            geoid_undulation_m=args.geoid_undulation,
            geoid_gtx_path=args.geoid_gtx,
        )
        manifest = bake(cfg, args.out)
        qa = manifest["qa"]
        print(
            f"baked {qa['tile_count']} tiles -> {args.out}  "
            f"(config {manifest['config_hash'][:12]}, h {qa['height_min_m']:.1f}..{qa['height_max_m']:.1f} m)"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
