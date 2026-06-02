// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"

class ILogiTerrainService;

/**
 * Minimal service locator that lets consumer modules (e.g. LogiMass) reach
 * cross-boundary services (e.g. terrain) through interfaces in LogiCore, without
 * a compile-time dependency on the providing module (e.g. LogiTerrain).
 *
 * The provider registers its implementation at startup; consumers resolve the
 * interface. This is the seam that keeps Mass and the Rust FFI separated.
 */
class LOGICORE_API FLogiServiceRegistry
{
public:
	/** Provider (LogiTerrain) registers its implementation here. */
	static void RegisterTerrainService(ILogiTerrainService* InService);

	/** Provider clears its registration on shutdown (only if it is the current one). */
	static void UnregisterTerrainService(ILogiTerrainService* InService);

	/** Consumers (LogiMass, ...) resolve the terrain interface. May be null before registration. */
	static ILogiTerrainService* GetTerrainService();

private:
	static ILogiTerrainService* TerrainService;
};
