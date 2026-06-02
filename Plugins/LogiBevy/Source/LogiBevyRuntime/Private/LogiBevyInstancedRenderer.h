// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"

struct FLogiAgentInstance;

/**
 * Drives instanced-static-mesh (ISM/HISM) rendering of the agent population from
 * the per-frame snapshot. The bulk "faked where you don't look" representation.
 * Plain C++ helper — owns no gameplay state.
 */
class FLogiBevyInstancedRenderer
{
public:
	/** Push agent transforms into instanced rendering. (Stub: records the count.) */
	void SyncInstances(const TArray<FLogiAgentInstance>& Instances);

	int32 LastInstanceCount() const { return InstanceCount; }

private:
	int32 InstanceCount = 0;
};
