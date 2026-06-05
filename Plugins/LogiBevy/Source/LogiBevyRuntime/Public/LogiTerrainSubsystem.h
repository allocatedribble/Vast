// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "Subsystems/WorldSubsystem.h"
#include "Templates/PimplPtr.h"
#include "UObject/StrongObjectPtr.h" // Nanite re-entry cache keeps evicted meshes GC-strong
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
 * Actorless terrain renderer (ADR-0007 / Milestone D). Owns a Rust terrain kernel
 * (`LogiTerrainWorld`), feeds it the player camera each frame, steps it, and builds
 * real `UProceduralMeshComponent` sections from the kernel's zero-copy vertex/index
 * buffer views (one section per resident tile) -- NO `AActor` per tile, no terrain
 * truth in Unreal. The production Nanite lane (E) will replace the proc-mesh sections
 * with native Nanite mesh resources built from the same frame packets. Runs in
 * game/PIE worlds AND the editor level-viewport (the latter throttled to ~30 Hz).
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
	/** Stream in the editor level-viewport too (not just PIE). Tick() throttles the editor world to ~30 Hz. */
	virtual bool IsTickableInEditor() const override { return true; }

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

#if LOGI_TERRAIN_NANITE_LANE
	/** One resident tile in the Nanite lane: a pooled UStaticMeshComponent holding a transient
	 *  Nanite UStaticMesh built from the tile's geometry. */
	struct FNaniteTile
	{
		TWeakObjectPtr<class UStaticMeshComponent> Comp;
		uint64 LastSeen = 0;
	};
	/** Pull a pooled (or new) UStaticMeshComponent under the host actor for a Nanite tile. */
	class UStaticMeshComponent* AcquireStaticMeshComponent();
	/** Release a Nanite tile's component back to the pool (drops its mesh, hides it). */
	void ReleaseNaniteTile(const FNaniteTile& Tile);
	/** Bounded-LRU safety cap for the Nanite lane (parity with proc-mesh EvictIfOverBudget):
	 *  when TileNanite exceeds MaxNaniteTiles, release the oldest (never a current-frame tile). */
	void EvictNaniteIfOverBudget();
	/** Tear down every Nanite-lane tile (lane switch / shutdown -- NOT rebase; tiles survive it now). */
	void ClearNaniteLane();

	/** One recently-EVICTED Nanite tile, retained for cheap re-entry: keeps the already-encoded
	 *  UStaticMesh alive (TStrongObjectPtr, GC-strong, no UPROPERTY needed) so re-visiting the area
	 *  skips the expensive async Nanite re-encode. Bounded by NaniteReentryCacheSize. The meshes are
	 *  origin-INDEPENDENT (tile-local geometry + component transform), so the cache survives an origin
	 *  rebase -- only a lane switch / shutdown clears it. */
	struct FLogiNaniteCachedTile
	{
		TStrongObjectPtr<class UStaticMesh> Mesh;
		uint64 CachedFrame = 0; // for LRU trim
	};
	/** Move a departed tile's encoded mesh into the re-entry cache (then pool its component). */
	void CacheNaniteTile(uint64 Key, const FNaniteTile& Tile);
	/** Bound the re-entry cache to NaniteReentryCacheSize (LRU: drop the oldest). */
	void TrimNaniteCache();
	/** Drop the whole re-entry cache (lane switch / shutdown). */
	void ClearNaniteCache();
#endif

	TPimplPtr<FLogiTerrainWorld> TerrainWorld;
	TWeakObjectPtr<AActor> HostActor;
	TWeakObjectPtr<class UProceduralMeshComponent> TerrainMesh;
	TMap<uint64 /*patch handle*/, FTileEntry> TileSection;
	TArray<int32> FreeSections; // cleared section indices available for reuse
	int32 NextSection = 0;
	uint64 FrameCounter = 0;
	float EditorTickAccum = 0.0f; // editor-world tick throttle (don't step the streaming kernel every editor frame)
	// Reused per-frame scratch for the resident-set diff (both render lanes; only one runs per frame): the
	// keys resident THIS frame + the keys that departed. Members so the per-frame diff reuses backing storage.
	TSet<uint64> ResidentScratch;
	TArray<uint64> DepartedScratch;
#if LOGI_TERRAIN_NANITE_LANE
	TMap<uint64 /*patch handle*/, FNaniteTile> TileNanite; // resident Nanite-lane tiles
	TArray<TWeakObjectPtr<class UStaticMeshComponent>> FreeStaticComps; // recyclable pooled components
	bool bNaniteLaneActive = false; // which lane is currently live (tracks the setting for switch-over)
	TMap<uint64 /*patch handle*/, FLogiNaniteCachedTile> NaniteMeshCache; // evicted meshes kept for cheap re-entry
#endif
#if WITH_EDITOR
	FLogiTerrainStats EditorStats;
	TArray<FLogiTerrainTileRow> EditorTiles;
#endif
};
