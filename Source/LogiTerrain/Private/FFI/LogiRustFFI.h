// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"

// =====================================================================
//  Narrow C-ABI boundary to the native Rust terrain kernel (ADR-0002).
//
//  Plain data in, plain data out. No Unreal types and no Rust types cross
//  this line. Rust never calls back into Unreal. This header is PRIVATE to
//  LogiTerrain — no other module includes it, so the FFI cannot leak.
// =====================================================================

extern "C"
{
	/** POD mirror of a terrain sample as it crosses the ABI. */
	struct LogiFfiTerrainSample
	{
		double HeightCm;
		double NormalX;
		double NormalY;
		double NormalZ;
		int32 bResident; // 0/1
	};

	/** Deterministic surface sample at tile-local (LocalX, LocalY) for a resident tile. */
	LogiFfiTerrainSample LogiRust_SampleSurface(uint64 TileId, double LocalX, double LocalY);

	/** 1 if the tile's source data is resident, else 0. */
	int32 LogiRust_IsTileResident(uint64 TileId);

	/** Ask the kernel to make a tile resident. The streaming decision is the kernel's. */
	void LogiRust_RequestTile(uint64 TileId);
}
