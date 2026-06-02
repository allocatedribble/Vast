// Copyright LogiUE. All Rights Reserved.

#include "Services/LogiServiceRegistry.h"
#include "Services/ILogiTerrainService.h"
#include "LogiCore.h"

ILogiTerrainService* FLogiServiceRegistry::TerrainService = nullptr;

void FLogiServiceRegistry::RegisterTerrainService(ILogiTerrainService* InService)
{
	if (TerrainService != nullptr && TerrainService != InService)
	{
		UE_LOG(LogLogiCore, Warning, TEXT("Terrain service replaced while one was already registered."));
	}
	TerrainService = InService;
	UE_LOG(LogLogiCore, Log, TEXT("Terrain service %s."), InService ? TEXT("registered") : TEXT("cleared"));
}

void FLogiServiceRegistry::UnregisterTerrainService(ILogiTerrainService* InService)
{
	if (TerrainService == InService)
	{
		TerrainService = nullptr;
		UE_LOG(LogLogiCore, Log, TEXT("Terrain service unregistered."));
	}
}

ILogiTerrainService* FLogiServiceRegistry::GetTerrainService()
{
	return TerrainService;
}
