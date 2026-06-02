// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyInstancedRenderer.h"
#include "LogiBevyTypes.h"

void FLogiBevyInstancedRenderer::SyncInstances(const TArray<FLogiAgentInstance>& Instances)
{
	// TODO: drive a UInstancedStaticMeshComponent / HISM transform buffer from the
	// snapshot. For now we only record how many instances the snapshot carried.
	InstanceCount = Instances.Num();
}
