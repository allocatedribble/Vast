// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"

/**
 * Lightweight bridge telemetry: agent counts, sim tick, and (later) FFI snapshot
 * timings — the budgeted line item from Docs/Performance/Budgets.md. Plain C++ helper.
 */
class FLogiBevyTelemetry
{
public:
	void RecordFrame(int32 AgentCount, int64 SimTick);

private:
	int64 FramesRecorded = 0;
};
