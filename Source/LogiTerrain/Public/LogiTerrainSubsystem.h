// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Subsystems/WorldSubsystem.h"
#include "Services/ILogiTerrainService.h"
#include "Ids/LogiIds.h"
#include "LogiTerrainSubsystem.generated.h"

/**
 * Runtime terrain access point. Implements ILogiTerrainService (LogiCore) and
 * registers itself with FLogiServiceRegistry so consumers (LogiMass, ...) can
 * query terrain without ever seeing LogiTerrain or the Rust FFI.
 *
 * This is a presentation/integration shell — terrain TRUTH is the Rust kernel
 * (ADR-0002). The subsystem only caches and marshals.
 */
UCLASS()
class LOGITERRAIN_API ULogiTerrainSubsystem : public UWorldSubsystem, public ILogiTerrainService
{
	GENERATED_BODY()

public:
	// UWorldSubsystem
	virtual void Initialize(FSubsystemCollectionBase& Collection) override;
	virtual void Deinitialize() override;

	// ILogiTerrainService (explicit interface boundary)
	virtual bool IsTileResident(const FLogiTileId& Tile) const override;
	virtual FLogiTerrainSample SampleSurface(const FLogiTileId& Tile, double LocalX, double LocalY) const override;
	virtual void RequestTile(const FLogiTileId& Tile) override;

private:
	/** Tiles for which residency has been requested from the kernel (request queue stub). */
	TSet<uint64> RequestedTiles;
};
