// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Ids/LogiIds.h"

// The EXPLICIT INTERFACE that decouples Mass simulation from the Rust terrain
// kernel (acceptance criterion: "Mass code is isolated from terrain Rust FFI
// code except through explicit interfaces").
//
//   - LogiTerrain implements this and wraps the Rust FFI (FFI types stay private
//     to LogiTerrain; ADR-0002).
//   - LogiMass *consumes* this via FLogiServiceRegistry and never includes a
//     LogiTerrain or FFI header.
//
// All types crossing this boundary are LogiCore PODs — no Mass types, no Rust types.

/** A plain-data terrain query result. */
struct FLogiTerrainSample
{
	/** Surface height in Unreal world units (cm). */
	double HeightCm = 0.0;

	/** Up-facing surface normal. */
	FVector Normal = FVector::UpVector;

	/** Whether the sample came from resident source data (vs. a fallback/estimate). */
	bool bResident = false;
};

/**
 * Read/stream interface over authoritative terrain. Truth lives in the Rust
 * kernel; this is the contract Unreal-side consumers use to query it.
 */
class ILogiTerrainService
{
public:
	virtual ~ILogiTerrainService() = default;

	/** True if the tile's source data is currently resident and queryable. */
	virtual bool IsTileResident(const FLogiTileId& Tile) const = 0;

	/** Sample the surface at a tile-local position. Deterministic for a resident tile. */
	virtual FLogiTerrainSample SampleSurface(const FLogiTileId& Tile, double LocalX, double LocalY) const = 0;

	/** Request that a tile be made resident (streaming decision is owned by the kernel). */
	virtual void RequestTile(const FLogiTileId& Tile) = 0;
};
