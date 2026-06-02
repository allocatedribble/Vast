// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyAnimationBridge.h"

void FLogiAnimationBridge::ApplyActorUpdate(const LogiActorUpdate& Update)
{
	FTracked& Tracked = EntityParams.FindOrAdd(Update.entity.index);
	Tracked.Significance = Update.significance;
	Tracked.Anim = Update.animation;

	// TODO: push Tracked.Anim into the pooled AnimInstance (locomotion state, speed,
	// turn rate, aim), register the USkeletalMeshComponent with the Animation Budget
	// Allocator, and set its significance from Tracked.Significance so the allocator
	// prioritizes it under the fixed CPU budget.
}

void FLogiAnimationBridge::ReleaseEntity(uint64 EntityIndex)
{
	EntityParams.Remove(EntityIndex);
	// TODO: unregister the skeletal mesh from the Animation Budget Allocator.
}

void FLogiAnimationBridge::Reset()
{
	EntityParams.Reset();
}
