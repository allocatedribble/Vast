// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyTelemetry.h"
#include "LogiBevyTypes.h"

void FLogiBevyTelemetry::RecordFrame(int32 TrackedInstancedEntities, int32 ActorCount, int64 SimTick)
{
	++FramesRecorded;
	// Throttled heartbeat so the log doesn't flood every frame.
	if ((FramesRecorded % 300) == 1)
	{
		// "tracked instanced entities", not rendered instances: hidden/recycled ISM slots are not subtracted
		// from the entity->slot table this counts (audit C++ L7).
		UE_LOG(LogLogiBevy, Verbose, TEXT("Bevy bridge: %d instanced entities, %d actors, sim tick %lld."),
			TrackedInstancedEntities, ActorCount, SimTick);
	}
}
