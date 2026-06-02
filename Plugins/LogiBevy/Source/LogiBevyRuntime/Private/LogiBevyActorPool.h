// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "logi_bevy_ffi.h"

/**
 * Pools high/low-res representation actors for the significant few (§8 / ADR-0003).
 * Maintains an entity->actor-slot assignment with a free-list; releases on
 * explicit despawn or cross-representation move. Actors are shells -- no gameplay
 * writes back to Rust. (AActor spawn/return + asset resolution are TODOs.)
 */
class FLogiActorPool
{
public:
	void ApplyActorUpdate(const LogiActorUpdate& Update);
	void ReleaseEntity(uint64 EntityIndex);
	void Reset();

	int32 AssignedCount() const { return EntityToActor.Num(); }

private:
	struct FAssignment
	{
		int32 ActorSlot = INDEX_NONE;
		float Significance = 0.0f;
	};

	TMap<uint64 /*entity index*/, FAssignment> EntityToActor;
	TArray<int32> FreeActorSlots;
	int32 HighWater = 0;

	int32 Acquire() { return FreeActorSlots.Num() > 0 ? FreeActorSlots.Pop() : HighWater++; }
	void Release(int32 Slot) { FreeActorSlots.Add(Slot); }
};
