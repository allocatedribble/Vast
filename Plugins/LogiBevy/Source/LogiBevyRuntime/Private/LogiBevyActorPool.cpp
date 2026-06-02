// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyActorPool.h"

void FLogiActorPool::ApplyActorUpdate(const LogiActorUpdate& Update)
{
	const uint64 Entity = Update.entity.index;
	FAssignment& Assignment = EntityToActor.FindOrAdd(Entity);
	if (Assignment.ActorSlot == INDEX_NONE)
	{
		Assignment.ActorSlot = Acquire();
		// TODO: spawn/acquire a pooled AActor (class from Update.actor_class),
		// assign its skeletal mesh (Update.skeletal_mesh) + anim BP (Update.anim_blueprint).
	}
	Assignment.Significance = Update.significance;

	// TODO: set the pooled actor's transform from Update.transform_3x4.
}

void FLogiActorPool::ReleaseEntity(uint64 EntityIndex)
{
	FAssignment Removed;
	if (EntityToActor.RemoveAndCopyValue(EntityIndex, Removed))
	{
		if (Removed.ActorSlot != INDEX_NONE)
		{
			Release(Removed.ActorSlot);
			// TODO: return the pooled actor to its tier pool (forced release).
		}
	}
}

void FLogiActorPool::Reset()
{
	EntityToActor.Reset();
	FreeActorSlots.Reset();
	HighWater = 0;
}
