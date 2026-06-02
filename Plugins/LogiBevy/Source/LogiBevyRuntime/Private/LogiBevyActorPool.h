// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "UObject/WeakObjectPtr.h"
#include "logi_bevy_ffi.h"

class UWorld;
class AStaticMeshActor;

/**
 * Spawns / recycles real Unreal actors for entities the budget promotes to actor
 * tiers (significant -> full actor; everything else stays Bevy-only as ISM/data;
 * ADR-0003). Actors are non-authoritative shells synced from the snapshot --
 * positioned from the entity transform, destroyed when the entity is demoted or
 * despawned. No gameplay writes back to Rust.
 */
class FLogiActorPool
{
public:
	void Initialize(UWorld* InWorld);
	void Shutdown();

	void ApplyActorUpdate(const LogiActorUpdate& Update);
	void ReleaseEntity(uint64 EntityIndex);

	int32 AssignedCount() const { return EntityToActor.Num(); }

private:
	TWeakObjectPtr<UWorld> World;
	TMap<uint64 /*entity index*/, TWeakObjectPtr<AStaticMeshActor>> EntityToActor;

	AStaticMeshActor* SpawnActor(const FVector& Location);
	static FVector TranslationOf(const LogiActorUpdate& Update);
};
