// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Subsystems/WorldSubsystem.h"
#include "Templates/PimplPtr.h"
#include "LogiTerrainSubsystem.generated.h"

class FLogiTerrainWorld;

/**
 * Actorless terrain renderer (ADR-0007 / Milestone D, debug lane). Owns a Rust
 * terrain kernel (`LogiTerrainWorld`), feeds it the player camera each frame, steps
 * it, and draws the resident tile bounds (debug lane) -- NO `AActor` per tile, no
 * terrain truth in Unreal. The production Nanite lane (E) replaces the debug draw
 * with real mesh resources built from the same frame packets + zero-copy buffer
 * views. Runs in game/PIE worlds; an editor-camera variant comes later.
 */
UCLASS()
class LOGIBEVYRUNTIME_API ULogiTerrainSubsystem : public UTickableWorldSubsystem
{
	GENERATED_BODY()

public:
	virtual void Initialize(FSubsystemCollectionBase& Collection) override;
	virtual void Deinitialize() override;
	virtual bool ShouldCreateSubsystem(UObject* Outer) const override;

	// FTickableGameObject
	virtual void Tick(float DeltaTime) override;
	virtual TStatId GetStatId() const override;

private:
	void EnsureRenderTarget();

	TPimplPtr<FLogiTerrainWorld> TerrainWorld;
	TWeakObjectPtr<AActor> HostActor;
	TWeakObjectPtr<class UProceduralMeshComponent> TerrainMesh;
	TMap<uint64 /*patch handle*/, int32 /*mesh section*/> TileSection;
};
