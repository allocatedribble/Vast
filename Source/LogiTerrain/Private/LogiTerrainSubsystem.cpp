// Copyright LogiUE. All Rights Reserved.

#include "LogiTerrainSubsystem.h"
#include "LogiTerrain.h"
#include "FFI/LogiRustFFI.h"
#include "Services/LogiServiceRegistry.h"

void ULogiTerrainSubsystem::Initialize(FSubsystemCollectionBase& Collection)
{
	Super::Initialize(Collection);

	// Publish ourselves as the terrain service for consumers to resolve.
	FLogiServiceRegistry::RegisterTerrainService(this);
	UE_LOG(LogLogiTerrain, Log, TEXT("LogiTerrainSubsystem initialized and registered as ILogiTerrainService."));
}

void ULogiTerrainSubsystem::Deinitialize()
{
	FLogiServiceRegistry::UnregisterTerrainService(this);
	RequestedTiles.Reset();

	Super::Deinitialize();
}

bool ULogiTerrainSubsystem::IsTileResident(const FLogiTileId& Tile) const
{
	return LogiRust_IsTileResident(Tile.Value) != 0;
}

FLogiTerrainSample ULogiTerrainSubsystem::SampleSurface(const FLogiTileId& Tile, double LocalX, double LocalY) const
{
	const LogiFfiTerrainSample Raw = LogiRust_SampleSurface(Tile.Value, LocalX, LocalY);

	FLogiTerrainSample Out;
	Out.HeightCm = Raw.HeightCm;
	Out.Normal = FVector(Raw.NormalX, Raw.NormalY, Raw.NormalZ);
	Out.bResident = Raw.bResident != 0;
	return Out;
}

void ULogiTerrainSubsystem::RequestTile(const FLogiTileId& Tile)
{
	bool bAlready = false;
	RequestedTiles.Add(Tile.Value, &bAlready);
	if (!bAlready)
	{
		LogiRust_RequestTile(Tile.Value);
	}
}
