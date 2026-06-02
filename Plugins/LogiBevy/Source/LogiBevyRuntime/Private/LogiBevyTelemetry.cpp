// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyTelemetry.h"
#include "LogiBevyTypes.h"

void FLogiBevyTelemetry::RecordFrame(int32 InstanceCount, int32 ActorCount, int64 SimTick)
{
	++FramesRecorded;
	// Throttled heartbeat so the log doesn't flood every frame.
	if ((FramesRecorded % 300) == 1)
	{
		UE_LOG(LogLogiBevy, Verbose, TEXT("Bevy bridge: %d ISM instances, %d actors, sim tick %lld."),
			InstanceCount, ActorCount, SimTick);
	}
}
