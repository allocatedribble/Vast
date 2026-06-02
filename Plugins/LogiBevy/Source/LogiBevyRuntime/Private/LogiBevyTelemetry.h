// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"

/**
 * Lightweight bridge telemetry: instanced + pooled-actor counts and the sim tick
 * (and, later, FFI snapshot timings). The budgeted line item from
 * Docs/Performance/Budgets.md. Plain C++ helper.
 */
class FLogiBevyTelemetry
{
public:
	void RecordFrame(int32 InstanceCount, int32 ActorCount, int64 SimTick);

private:
	int64 FramesRecorded = 0;
};
