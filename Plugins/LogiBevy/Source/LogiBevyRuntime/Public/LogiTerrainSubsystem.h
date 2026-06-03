// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Subsystems/WorldSubsystem.h"
#include "Templates/PimplPtr.h"
#include "LogiTerrainSubsystem.generated.h"

class FLogiTerrainWorld;

/** One resident tile row for the editor Logi Terrain panel (read-only mirror). */
struct FLogiTerrainTileRow
{
	uint64 Key = 0;
	uint32 Lod = 0;
	FVector Center = FVector::ZeroVector;
	float Radius = 0.0f;
};

/** Terrain-kernel telemetry snapshot for the editor panel. */
struct FLogiTerrainStats
{
	uint64 FrameId = 0;
	int32 ResidentTiles = 0;
	int32 InFlight = 0;
	int64 BytesResident = 0;
	int64 PeakBytesResident = 0;
	int64 Evictions = 0;
	int64 Fallbacks = 0;
	int64 RequestsCompleted = 0;
};

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

#if WITH_EDITOR
	/** Latest telemetry + resident tiles for the editor Logi Terrain panel (ADR-0006). */
	void GetEditorTerrainState(FLogiTerrainStats& OutStats, TArray<FLogiTerrainTileRow>& OutTiles) const;
#endif

private:
	struct FTileEntry
	{
		int32 Section = INDEX_NONE;
		uint64 LastSeen = 0;
	};

	void EnsureRenderTarget();
	void EvictIfOverBudget();

	TPimplPtr<FLogiTerrainWorld> TerrainWorld;
	TWeakObjectPtr<AActor> HostActor;
	TWeakObjectPtr<class UProceduralMeshComponent> TerrainMesh;
	TMap<uint64 /*patch handle*/, FTileEntry> TileSection;
	TArray<int32> FreeSections; // cleared section indices available for reuse
	int32 NextSection = 0;
	uint64 FrameCounter = 0;
#if WITH_EDITOR
	FLogiTerrainStats EditorStats;
	TArray<FLogiTerrainTileRow> EditorTiles;
#endif
};
