// Copyright LogiUE. All Rights Reserved.

#include "LogiTerrainSubsystem.h"
#include "LogiTerrainWorld.h"
#include "LogiBevyTypes.h"
#include "LogiBevyDeveloperSettings.h"
#include "Engine/World.h"
#include "Engine/GameViewportClient.h" // UGameViewportClient::GetViewportSize (real viewport size)
#include "GameFramework/Actor.h"
#include "GameFramework/PlayerController.h"
#include "Camera/PlayerCameraManager.h" // APlayerCameraManager::GetFOVAngle (real camera FOV)
#include "ProceduralMeshComponent.h"

#if WITH_EDITOR
#include "LevelEditorViewport.h" // GCurrentLevelEditingViewportClient
#endif

#if LOGI_BEVY_HAVE_TERRAIN
#include "logi_terrain_ffi.h"
#endif

#if LOGI_TERRAIN_NANITE_LANE
// Editor-only Nanite lane (Milestone E): build a transient Nanite UStaticMesh per tile.
#include "Components/StaticMeshComponent.h"
#include "Engine/StaticMesh.h"
#include "StaticMeshAttributes.h"
#include "MeshDescription.h"
#include "Materials/Material.h"
#include "MaterialDomain.h" // UMaterial::GetDefaultMaterial(MD_Surface)
#endif

namespace
{
	// The terrain kernel works in metres; Unreal in centimetres.
	constexpr double LogiTerrainToUnreal = 100.0;

	// A tile whose bounding radius (metres) exceeds this — the coarse quadtree roots — is skipped by
	// both render lanes: as one giant proc-mesh section it bloats the component bounds to ~world size
	// and breaks UE frustum culling (dropping the real fine terrain); the Nanite lane skips it for
	// parity. Radius-based (not a fixed level) so it adapts to any world size.
	constexpr float GiantTileRadiusMeters = 2500.0f;

	// View point for streaming: the PIE/game player camera, else (editor, no PIE)
	// the active level-viewport camera so terrain streams in the editor viewport.
	bool GetTerrainViewPoint(UWorld* World, FVector& OutLoc, FRotator& OutRot)
	{
		if (APlayerController* PC = World->GetFirstPlayerController())
		{
			PC->GetPlayerViewPoint(OutLoc, OutRot);
			return true;
		}
#if WITH_EDITOR
		if (GCurrentLevelEditingViewportClient != nullptr)
		{
			OutLoc = GCurrentLevelEditingViewportClient->GetViewLocation();
			OutRot = GCurrentLevelEditingViewportClient->GetViewRotation();
			return true;
		}
#endif
		return false;
	}

#if LOGI_BEVY_HAVE_TERRAIN
	// Build one procedural-mesh section from a tile's interleaved 32-byte vertices
	// (pos f3, normal f3, uv f2) + u32 indices. Terrain is Y-up + metres; Unreal is
	// Z-up + cm, so positions/normals swap Y<->Z. That Y<->Z swap is a reflection
	// (negative-determinant basis change), so it flips handedness: the kernel winds
	// its triangles +Y-up, and the swap reflects them into UE's clockwise-from-front
	// convention. Emit the kernel index order UNCHANGED — do NOT reverse winding, or
	// up-faces become back-facing and get culled when viewed from above.
	void BuildSection(UProceduralMeshComponent* Mesh, int32 Section, const LogiTerrainPatchDraw& P,
		const LogiBufferView& VView, const LogiBufferView& IView)
	{
		// Floating-origin: vertices are RELATIVE to the kernel render origin. The host actor is parked at
		// Origin*100 (see the rebase in Tick), so the proc-mesh draws near its host with small float coords.
		// local_position is already world-minus-origin, so use it directly — re-adding the origin would put
		// us back at planetary world coords that UE won't render (ADR-0007).
		const double X0 = static_cast<double>(P.local_position.x);
		const double Y0 = static_cast<double>(P.local_position.y);
		const double Z0 = static_cast<double>(P.local_position.z);

		// Buffer-bounds safety: never trust the patch counts past the actual view lengths.
		const uint32 Stride = VView.element_stride;
		if (Stride < 32) { return; } // 32-byte interleaved vertex required; malformed view -> build nothing
		const uint32 VCount = FMath::Min<uint32>(P.vertex_count, static_cast<uint32>(VView.byte_length / Stride));

		TArray<FVector> Positions;
		TArray<FVector> Normals;
		TArray<FVector2D> UVs;
		Positions.Reserve(VCount);
		Normals.Reserve(VCount);
		UVs.Reserve(VCount);
		for (uint32 v = 0; v < VCount; ++v)
		{
			const float* F = reinterpret_cast<const float*>(VView.data + static_cast<uint64>(v) * Stride);
			Positions.Add(FVector((X0 + F[0]) * LogiTerrainToUnreal, (Z0 + F[2]) * LogiTerrainToUnreal, (Y0 + F[1]) * LogiTerrainToUnreal));
			Normals.Add(FVector(F[3], F[5], F[4]));
			UVs.Add(FVector2D(F[6], F[7]));
		}

		// Clamp index count to the view length (u32 each) and drop any triangle that references
		// a vertex past the clamped vertex count -- guards against a truncated/garbage index view.
		const uint32 ICount = FMath::Min<uint32>(P.index_count, static_cast<uint32>(IView.byte_length / 4));
		const uint32* IData = reinterpret_cast<const uint32*>(IView.data);
		TArray<int32> Triangles;
		Triangles.Reserve(ICount);
		for (uint32 t = 0; t + 3 <= ICount; t += 3)
		{
			const uint32 I0 = IData[t];
			const uint32 I1 = IData[t + 1];
			const uint32 I2 = IData[t + 2];
			if (I0 >= VCount || I1 >= VCount || I2 >= VCount) { continue; }
			// Kernel winding is already correct for UE after the Y<->Z reflection -- emit as-is (see above).
			Triangles.Add(static_cast<int32>(I0));
			Triangles.Add(static_cast<int32>(I1));
			Triangles.Add(static_cast<int32>(I2));
		}

		Mesh->CreateMeshSection_LinearColor(Section, Positions, Triangles, Normals, UVs,
			TArray<FLinearColor>(), TArray<FProcMeshTangent>(), /*bCreateCollision=*/false);
	}

#if LOGI_TERRAIN_NANITE_LANE
	// World-relative offset (under the host actor) for a Nanite tile's component. The kernel's
	// local_position is origin-relative metres in kernel space (Y up); map it to Unreal cm with the
	// same Y<->Z swap the vertices use. Because the tile MESH is built tile-local (origin-free, see
	// BuildTileNaniteMesh), applying local_position HERE as the component transform -- instead of
	// baking it into the vertices -- makes the encoded mesh origin-INDEPENDENT: a floating-origin
	// rebase only repositions components (cheap) instead of re-encoding every tile, and the re-entry
	// cache survives the rebase. host(at origin) + component(local_position) == tile_center, exactly.
	FVector NaniteTileRelativeLocation(const LogiTerrainPatchDraw& P)
	{
		return FVector(
			static_cast<double>(P.local_position.x) * LogiTerrainToUnreal,
			static_cast<double>(P.local_position.z) * LogiTerrainToUnreal,
			static_cast<double>(P.local_position.y) * LogiTerrainToUnreal);
	}

	// Build a transient, **Nanite-enabled** UStaticMesh from a tile's interleaved 32-byte verts
	// (pos f3, normal f3, uv f2) + u32 indices. Vertices are TILE-LOCAL (kernel-space, tile-centered,
	// Y<->Z swap, m->cm x100, kernel winding unchanged) -- the origin/local_position is NOT baked in;
	// the caller positions the component via NaniteTileRelativeLocation so the encoded mesh is
	// origin-independent and reusable across rebases. Editor-only: BuildFromMeshDescriptions with
	// bFastBuild=false reaches UStaticMesh::Build(true), whose Nanite encode lives in the editor-only
	// NaniteBuilder module.
	//
	// ASYNC, not synchronous: Build(true) -> BatchBuild queues an FStaticMeshAsyncBuildTask on the
	// static-mesh thread pool and returns immediately with IsCompiling()==true (it does NOT block
	// the game thread on the encode). The freshly-built tile therefore pops in a FEW FRAMES LATE,
	// self-healing once the compile finishes (the manager calls PostStaticMeshCompilation on the
	// component; we intentionally do NOT FinishCompilation, which would force a game-thread stall).
	// So the per-step build budget is NOT a game-thread-cost throttle -- it bounds how many builds
	// are IN FLIGHT on the worker pool (and thus the transient-mesh / encode memory high-water mark).
	UStaticMesh* BuildTileNaniteMesh(UObject* Outer, const LogiTerrainPatchDraw& P,
		const LogiBufferView& VView, const LogiBufferView& IView, UMaterialInterface* Material)
	{
		const uint32 Stride = VView.element_stride;
		if (Stride < 32) { return nullptr; }
		const uint32 VCount = FMath::Min<uint32>(P.vertex_count, static_cast<uint32>(VView.byte_length / Stride));
		const uint32 ICount = FMath::Min<uint32>(P.index_count, static_cast<uint32>(IView.byte_length / 4));
		if (VCount < 3 || ICount < 3) { return nullptr; }

		const uint32* IData = reinterpret_cast<const uint32*>(IView.data);

		FMeshDescription MeshDesc;
		FStaticMeshAttributes Attrs(MeshDesc);
		Attrs.Register();
		TVertexAttributesRef<FVector3f> Positions = Attrs.GetVertexPositions();
		TVertexInstanceAttributesRef<FVector3f> Normals = Attrs.GetVertexInstanceNormals();
		TVertexInstanceAttributesRef<FVector2f> UVs = Attrs.GetVertexInstanceUVs();
		UVs.SetNumChannels(1);

		MeshDesc.ReserveNewVertices(VCount);
		MeshDesc.ReserveNewVertexInstances(ICount);
		MeshDesc.ReserveNewPolygons(ICount / 3);
		MeshDesc.ReserveNewEdges(ICount);

		const FPolygonGroupID Group = MeshDesc.CreatePolygonGroup();
		Attrs.GetPolygonGroupMaterialSlotNames()[Group] = TEXT("LogiTerrain");

		TArray<FVertexID> VIDs;
		VIDs.SetNumUninitialized(VCount);
		for (uint32 v = 0; v < VCount; ++v)
		{
			const float* F = reinterpret_cast<const float*>(VView.data + static_cast<uint64>(v) * Stride);
			const FVertexID Vid = MeshDesc.CreateVertex();
			// Tile-local (no origin/local_position term): the component transform places the tile.
			Positions[Vid] = FVector3f(
				static_cast<float>(F[0] * LogiTerrainToUnreal),
				static_cast<float>(F[2] * LogiTerrainToUnreal),
				static_cast<float>(F[1] * LogiTerrainToUnreal));
			VIDs[v] = Vid;
		}

		TArray<FVertexInstanceID> Corners;
		Corners.SetNum(3);
		for (uint32 t = 0; t + 3 <= ICount; t += 3)
		{
			const uint32 Tri[3] = { IData[t], IData[t + 1], IData[t + 2] };
			if (Tri[0] >= VCount || Tri[1] >= VCount || Tri[2] >= VCount) { continue; }
			for (int32 c = 0; c < 3; ++c)
			{
				const uint32 Vi = Tri[c];
				const float* F = reinterpret_cast<const float*>(VView.data + static_cast<uint64>(Vi) * Stride);
				const FVertexInstanceID Iid = MeshDesc.CreateVertexInstance(VIDs[Vi]);
				Normals[Iid] = FVector3f(F[3], F[5], F[4]);
				UVs.Set(Iid, 0, FVector2f(F[6], F[7]));
				Corners[c] = Iid;
			}
			MeshDesc.CreatePolygon(Group, Corners);
		}
		if (MeshDesc.Triangles().Num() == 0) { return nullptr; }

		UStaticMesh* Mesh = NewObject<UStaticMesh>(Outer, NAME_None, RF_Transient);
		Mesh->bSupportRayTracing = 0; // skip per-tile BLAS (project has ray tracing on; terrain doesn't need it)
		Mesh->GetStaticMaterials().Add(FStaticMaterial(
			Material ? Material : UMaterial::GetDefaultMaterial(MD_Surface),
			TEXT("LogiTerrain"), TEXT("LogiTerrain")));

		FMeshNaniteSettings Nanite = Mesh->GetNaniteSettings();
		Nanite.bEnabled = true;
		Mesh->SetNaniteSettings(Nanite);

		// Keep the kernel-supplied normals (M6): FMeshBuildSettings defaults bRecomputeNormals=true,
		// which DISCARDS our imported per-vertex normals and recomputes them per-tile -> the lighting
		// normal differs across a shared tile edge => visible seams. BuildFromMeshDescriptions creates
		// its source models internally via SetNumSourceModels, but that only DEFAULT-CONSTRUCTS *newly
		// added* models -- it does NOT reset BuildSettings on a model that already exists (verified vs
		// UE 5.8 UStaticMesh::SetNumSourceModels / BuildFromMeshDescriptions). So we pre-create source
		// model 0 here with normal-recompute disabled; the build reuses it (OldNum==Num) and honours
		// the flag. (The flags are part of the DDC key, so bUseHashAsGuid stays correct.)
		// We deliberately LEAVE bRecomputeTangents=true: the kernel vertex carries no tangent, and a
		// tangent basis recomputed from the *preserved* normals is fine (tangents only affect normal-map
		// sampling, not the cross-tile lighting normal that caused the seam).
		Mesh->SetNumSourceModels(1);
		Mesh->GetSourceModel(0).BuildSettings.bRecomputeNormals = false;

		UStaticMesh::FBuildMeshDescriptionsParams Params;
		Params.bMarkPackageDirty = false;     // transient: never dirties a package (callable off the dirtied-state path)
		Params.bUseHashAsGuid = true;         // identical tile content reuses the cached build
		Params.bFastBuild = false;            // full build -> reaches the (editor-only) Nanite encode
		Params.bCommitMeshDescription = true; // required when not fast-building
		Params.bBuildSimpleCollision = false;
		Mesh->BuildFromMeshDescriptions({ &MeshDesc }, Params);
		return Mesh;
	}
#endif // LOGI_TERRAIN_NANITE_LANE
#endif
}

void ULogiTerrainSubsystem::Initialize(FSubsystemCollectionBase& Collection)
{
	Super::Initialize(Collection);
	TerrainWorld = MakePimpl<FLogiTerrainWorld>();
#if LOGI_BEVY_HAVE_TERRAIN
	if (TerrainWorld->Create())
	{
		UE_LOG(LogLogiBevy, Log, TEXT("Terrain kernel created (actorless, ADR-0007)."));
	}
	else
	{
		UE_LOG(LogLogiBevy, Error, TEXT("logi_terrain_world_create failed."));
	}
#endif
}

void ULogiTerrainSubsystem::Deinitialize()
{
#if LOGI_TERRAIN_NANITE_LANE
	ClearNaniteLane();
	FreeStaticComps.Reset();
	bNaniteLaneActive = false;
#endif
	if (AActor* Host = HostActor.Get())
	{
		Host->Destroy();
	}
	HostActor.Reset();
	TerrainMesh.Reset();
	TileSection.Reset();
	FreeSections.Reset();
	NextSection = 0;
	FrameCounter = 0;
	if (TerrainWorld.IsValid())
	{
		TerrainWorld->Destroy();
	}
	Super::Deinitialize();
}

bool ULogiTerrainSubsystem::ShouldCreateSubsystem(UObject* Outer) const
{
	if (!Super::ShouldCreateSubsystem(Outer))
	{
		return false;
	}
	if (const UWorld* World = Cast<UWorld>(Outer))
	{
		// Opt-in, and now game/PIE **and** the editor level-viewport. The earlier
		// editor-streaming revert was caused by LOD over-refinement that flooded the
		// kernel with finest-level tiles every editor frame; that root cause is fixed
		// (base geometric error, winding, hidden-section bounds, per-tile section budget),
		// so editor streaming is safe again -- and Tick() throttles the editor world to
		// ~30 Hz so a streaming kernel never runs at the editor's full frame rate.
		if (!GetDefault<ULogiBevyDeveloperSettings>()->bEnableTerrain)
		{
			return false;
		}
		// Game + PIE, plus the main level-editor world (NOT preview/thumbnail worlds).
		return World->IsGameWorld() || World->WorldType == EWorldType::Editor;
	}
	return false;
}

TStatId ULogiTerrainSubsystem::GetStatId() const
{
	RETURN_QUICK_DECLARE_CYCLE_STAT(ULogiTerrainSubsystem, STATGROUP_Tickables);
}

void ULogiTerrainSubsystem::EnsureRenderTarget()
{
	if (HostActor.IsValid() && TerrainMesh.IsValid())
	{
		return;
	}
	UWorld* World = GetWorld();
	if (World == nullptr)
	{
		return;
	}
	FActorSpawnParameters Params;
	Params.ObjectFlags |= RF_Transient;
	AActor* Host = World->SpawnActor<AActor>(AActor::StaticClass(), FTransform::Identity, Params);
	if (Host == nullptr)
	{
		return;
	}
#if WITH_EDITOR
	Host->SetActorLabel(TEXT("LogiTerrain"));
#endif
	Host->Tags.Add(TEXT("LogiTerrain"));
	UProceduralMeshComponent* Mesh = NewObject<UProceduralMeshComponent>(Host, TEXT("LogiTerrainMesh"));
	Host->SetRootComponent(Mesh);
	Mesh->SetMobility(EComponentMobility::Movable);
	// Debug terrain is non-Nanite: casting into Virtual Shadow Maps floods the VSM non-Nanite marking
	// job queue (it overflows and primitives get dropped from the pass). The debug lane doesn't need
	// shadows; the production Nanite lane (Milestone E) feeds VSM natively. See ADR-0007.
	Mesh->SetCastShadow(false);
	Mesh->RegisterComponent();
	HostActor = Host;
	TerrainMesh = Mesh;
}

void ULogiTerrainSubsystem::EvictIfOverBudget()
{
	// Resident proc-mesh section cap. Kept >= the kernel's resident budget (logi_terrain_scene
	// MESH_CACHE_CAP = 4096) so the renderer holds every tile the kernel keeps -> this LRU is a pure
	// safety backstop that should not fire in normal operation. Keep in sync with the kernel cap.
	constexpr int32 MaxSections = 4096;
	UProceduralMeshComponent* Mesh = TerrainMesh.Get();
	if (Mesh == nullptr)
	{
		return;
	}
	if (TileSection.Num() <= MaxSections)
	{
		return;
	}
	// Single LRU pass: snapshot every (LastSeen, Key, Section), sort oldest-first, and evict
	// exactly the overflow in one go. Avoids the old O(n^2) "rescan the whole map per victim".
	// NEVER evict a tile that is resident THIS frame (LastSeen == FrameCounter): dropping a
	// live-frame tile would immediately rebuild it next frame (thrash) -- matches the Rust mesh
	// cache's "never evict the live frame" rule. We can over-shoot MaxSections for one frame if a
	// single frame's working set genuinely exceeds the cap; it drains as those tiles age out.
	struct FEvictCand { uint64 LastSeen; uint64 Key; int32 Section; };
	TArray<FEvictCand> Cands;
	Cands.Reserve(TileSection.Num());
	for (const TPair<uint64, FTileEntry>& Pair : TileSection)
	{
		if (Pair.Value.LastSeen == FrameCounter) { continue; } // resident this frame -> not an eviction candidate
		Cands.Add(FEvictCand{ Pair.Value.LastSeen, Pair.Key, Pair.Value.Section });
	}
	Cands.Sort([](const FEvictCand& A, const FEvictCand& B) { return A.LastSeen < B.LastSeen; });
	// Clamp to the eligible (non-live-frame) candidate count so we never touch this frame's tiles.
	const int32 NumToEvict = FMath::Min(TileSection.Num() - MaxSections, Cands.Num());
	for (int32 i = 0; i < NumToEvict; ++i)
	{
		Mesh->ClearMeshSection(Cands[i].Section);
		FreeSections.Add(Cands[i].Section);
		TileSection.Remove(Cands[i].Key);
	}
}

#if LOGI_TERRAIN_NANITE_LANE
UStaticMeshComponent* ULogiTerrainSubsystem::AcquireStaticMeshComponent()
{
	AActor* Host = HostActor.Get();
	if (Host == nullptr)
	{
		return nullptr;
	}
	// Reuse a pooled component if one is free (pooled comps stay attached + registered, so they
	// are GC-safe and cheap to recycle -- only the per-tile UStaticMesh is rebuilt).
	while (FreeStaticComps.Num() > 0)
	{
		TWeakObjectPtr<UStaticMeshComponent> Weak = FreeStaticComps.Pop(EAllowShrinking::No);
		if (UStaticMeshComponent* Pooled = Weak.Get())
		{
			Pooled->SetVisibility(true);
			return Pooled;
		}
	}
	UStaticMeshComponent* Comp = NewObject<UStaticMeshComponent>(Host, NAME_None, RF_Transient);
	Comp->SetupAttachment(Host->GetRootComponent());
	Comp->SetMobility(EComponentMobility::Movable);
	Comp->SetCollisionEnabled(ECollisionEnabled::NoCollision);
	Comp->SetCastShadow(true); // Nanite feeds Virtual Shadow Maps natively -- no VSM overflow, so shadows are fine
	Comp->RegisterComponent();
	return Comp;
}

void ULogiTerrainSubsystem::ReleaseNaniteTile(const FNaniteTile& Tile)
{
	if (UStaticMeshComponent* Comp = Tile.Comp.Get())
	{
		Comp->SetStaticMesh(nullptr); // drop the transient Nanite mesh -> eligible for GC
		Comp->SetVisibility(false);
		FreeStaticComps.Add(Comp);    // keep attached/registered -> reusable next tile
	}
}

void ULogiTerrainSubsystem::EvictNaniteIfOverBudget()
{
	// Hard safety cap mirroring the proc-mesh MaxSections: the per-frame resident-set release in Tick
	// is the primary bound, but a runaway working set (or a bug) must never grow TileNanite without
	// limit. Release the oldest tiles by LastSeen, but NEVER one resident this frame (see L9 in
	// EvictIfOverBudget) -- dropping a live tile would just rebuild it next frame.
	// Resident Nanite-tile cap — the same budget as the proc-mesh MaxSections and the kernel's
	// MESH_CACHE_CAP (4096); a safety backstop, since the kernel already bounds the resident set.
	constexpr int32 MaxNaniteTiles = 4096;
	if (TileNanite.Num() <= MaxNaniteTiles)
	{
		return;
	}
	struct FEvictCand { uint64 LastSeen; uint64 Key; };
	TArray<FEvictCand> Cands;
	Cands.Reserve(TileNanite.Num());
	for (const TPair<uint64, FNaniteTile>& Pair : TileNanite)
	{
		if (Pair.Value.LastSeen == FrameCounter) { continue; } // resident this frame -> not a candidate
		Cands.Add(FEvictCand{ Pair.Value.LastSeen, Pair.Key });
	}
	Cands.Sort([](const FEvictCand& A, const FEvictCand& B) { return A.LastSeen < B.LastSeen; });
	const int32 NumToEvict = FMath::Min(TileNanite.Num() - MaxNaniteTiles, Cands.Num());
	for (int32 i = 0; i < NumToEvict; ++i)
	{
		// Cache (don't drop) the evicted mesh so a re-entry skips the async re-encode, same as the
		// per-frame resident-set release. This backstop shouldn't fire under the synchronized 4096
		// caps, but if it ever does keep it re-entry-cheap (TrimNaniteCache bounds the cache next step).
		CacheNaniteTile(Cands[i].Key, TileNanite.FindAndRemoveChecked(Cands[i].Key));
	}
}

void ULogiTerrainSubsystem::ClearNaniteLane()
{
	for (const TPair<uint64, FNaniteTile>& Pair : TileNanite)
	{
		ReleaseNaniteTile(Pair.Value);
	}
	TileNanite.Reset();
	// ClearNaniteLane runs only on lane switch / shutdown now (NOT rebase -- tiles are origin-independent
	// since the tile-local-mesh change), so dropping the whole re-entry cache here is correct.
	ClearNaniteCache();
}

void ULogiTerrainSubsystem::CacheNaniteTile(uint64 Key, const FNaniteTile& Tile)
{
	UStaticMeshComponent* Comp = Tile.Comp.Get();
	if (Comp == nullptr)
	{
		return;
	}
	// Retain the encoded mesh for cheap re-entry (skips the async re-encode), then pool the component.
	// The TStrongObjectPtr becomes the only ref keeping the mesh alive while it sits out of view.
	if (UStaticMesh* Mesh = Comp->GetStaticMesh())
	{
		// Invariant: a resident tile is never also in the re-entry cache (cache<->resident are mutually
		// exclusive), so this key can't already be present and FindOrAdd won't orphan a prior encode.
		checkSlow(!NaniteMeshCache.Contains(Key));
		FLogiNaniteCachedTile& Entry = NaniteMeshCache.FindOrAdd(Key);
		Entry.Mesh.Reset(Mesh);
		Entry.CachedFrame = FrameCounter;
	}
	Comp->SetStaticMesh(nullptr);
	Comp->SetVisibility(false);
	FreeStaticComps.Add(Comp);
}

void ULogiTerrainSubsystem::TrimNaniteCache()
{
	const int32 MaxCache = FMath::Max(0, GetDefault<ULogiBevyDeveloperSettings>()->NaniteReentryCacheSize);
	if (NaniteMeshCache.Num() <= MaxCache)
	{
		return;
	}
	// LRU: drop the oldest cached entries (their TStrongObjectPtr releases -> mesh eligible for GC).
	struct FCacheAge { uint64 CachedFrame; uint64 Key; };
	TArray<FCacheAge> ByAge;
	ByAge.Reserve(NaniteMeshCache.Num());
	for (const TPair<uint64, FLogiNaniteCachedTile>& Pair : NaniteMeshCache)
	{
		ByAge.Add(FCacheAge{ Pair.Value.CachedFrame, Pair.Key });
	}
	ByAge.Sort([](const FCacheAge& A, const FCacheAge& B) { return A.CachedFrame < B.CachedFrame; });
	const int32 NumToDrop = NaniteMeshCache.Num() - MaxCache;
	for (int32 i = 0; i < NumToDrop; ++i)
	{
		NaniteMeshCache.Remove(ByAge[i].Key);
	}
}

void ULogiTerrainSubsystem::ClearNaniteCache()
{
	NaniteMeshCache.Empty(); // every TStrongObjectPtr releases -> cached meshes eligible for GC
}
#endif // LOGI_TERRAIN_NANITE_LANE

void ULogiTerrainSubsystem::Tick(float DeltaTime)
{
#if LOGI_BEVY_HAVE_TERRAIN
	UWorld* World = GetWorld();
	if (World == nullptr || !TerrainWorld.IsValid() || !TerrainWorld->IsValid())
	{
		return;
	}
	// Editor-world streaming throttle: the editor renders at its full (often >100 fps) rate,
	// and stepping a streaming kernel that fast over-works it -- that is what stalled the
	// viewport in the first editor-streaming attempt. Game/PIE steps every frame; the editor
	// world steps at ~30 Hz, handing the kernel the real accumulated elapsed time.
	if (World->WorldType == EWorldType::Editor)
	{
		constexpr float EditorStepSeconds = 1.0f / 30.0f;
		EditorTickAccum += DeltaTime;
		if (EditorTickAccum < EditorStepSeconds)
		{
			return;
		}
		DeltaTime = EditorTickAccum;
		EditorTickAccum = 0.0f;
	}
	EnsureRenderTarget();
	UProceduralMeshComponent* Mesh = TerrainMesh.Get();
	if (Mesh == nullptr)
	{
		return;
	}

#if LOGI_TERRAIN_NANITE_LANE
	// Pick the active render lane from settings; on a switch, tear down the lane we are leaving so
	// the proc-mesh debug lane and the Nanite production lane never double-draw the same terrain.
	{
		const bool bWantNanite = GetDefault<ULogiBevyDeveloperSettings>()->bUseNaniteTerrainLane;
		if (bWantNanite != bNaniteLaneActive)
		{
			if (bWantNanite)
			{
				Mesh->ClearAllMeshSections();
				TileSection.Reset();
				FreeSections.Reset();
				NextSection = 0;
			}
			else
			{
				ClearNaniteLane();
			}
			bNaniteLaneActive = bWantNanite;
		}
	}
#endif

	// Feed the player camera (Unreal cm + Z-up -> terrain metres + Y-up).
	LogiCameraState Cam{};
	Cam.forward = LogiVec3{ 1.0f, 0.0f, 0.0f };
	Cam.up = LogiVec3{ 0.0f, 1.0f, 0.0f };
	Cam.fov_y_radians = 1.05f;     // fallback if no viewport/camera manager is available
	Cam.viewport_width = 1920.0f;  // fallback
	Cam.viewport_height = 1080.0f; // fallback
	Cam.sse_target_pixels = 8.0f;
	// Prefer the real game viewport size when present (keeps SSE-driven LOD honest).
	if (UGameViewportClient* VPC = World->GetGameViewport())
	{
		FVector2D Sz;
		VPC->GetViewportSize(Sz);
		if (Sz.X > 0 && Sz.Y > 0)
		{
			Cam.viewport_width = static_cast<float>(Sz.X);
			Cam.viewport_height = static_cast<float>(Sz.Y);
		}
	}
	// Prefer the real player-camera vertical FOV. GetFOVAngle() is the *horizontal* FOV in degrees,
	// so convert to vertical using the (possibly just-updated) viewport aspect ratio.
	if (APlayerController* PC = World->GetFirstPlayerController())
	{
		if (PC->PlayerCameraManager)
		{
			const float HFovDeg = PC->PlayerCameraManager->GetFOVAngle();
			const float Aspect = Cam.viewport_width / FMath::Max(Cam.viewport_height, 1.0f);
			Cam.fov_y_radians = 2.0f * FMath::Atan(FMath::Tan(FMath::DegreesToRadians(HFovDeg) * 0.5f) / FMath::Max(Aspect, 0.001f));
		}
	}
#if WITH_EDITOR
	// Editor level-viewport streaming (no PIE player / game viewport): the game-viewport size and
	// player-camera FOV above never fired, so SSE LOD would run on the 1920/1080/1.05 fallbacks.
	// Pull the editing viewport's real size + FOV instead (matches GetTerrainViewPoint's editor path).
	else if (GCurrentLevelEditingViewportClient != nullptr)
	{
		if (FViewport* EditorViewport = GCurrentLevelEditingViewportClient->Viewport)
		{
			const FIntPoint Sz = EditorViewport->GetSizeXY();
			if (Sz.X > 0 && Sz.Y > 0)
			{
				Cam.viewport_width = static_cast<float>(Sz.X);
				Cam.viewport_height = static_cast<float>(Sz.Y);
			}
		}
		// ViewFOV is the viewport's current *horizontal* FOV in degrees (camera locks can modify it);
		// convert to vertical with the just-updated aspect, same as the player-camera branch.
		const float HFovDeg = GCurrentLevelEditingViewportClient->ViewFOV;
		if (HFovDeg > 1.0f)
		{
			const float Aspect = Cam.viewport_width / FMath::Max(Cam.viewport_height, 1.0f);
			Cam.fov_y_radians = 2.0f * FMath::Atan(FMath::Tan(FMath::DegreesToRadians(HFovDeg) * 0.5f) / FMath::Max(Aspect, 0.001f));
		}
	}
#endif
	FVector Loc;
	FRotator Rot;
	if (GetTerrainViewPoint(World, Loc, Rot))
	{
		Cam.position = LogiDVec3{ Loc.X / LogiTerrainToUnreal, Loc.Z / LogiTerrainToUnreal, Loc.Y / LogiTerrainToUnreal };
		const FVector F = Rot.Vector();
		Cam.forward = LogiVec3{ static_cast<float>(F.X), static_cast<float>(F.Z), static_cast<float>(F.Y) };
		const FVector U = FRotationMatrix(Rot).GetUnitAxis(EAxis::Z);
		Cam.up = LogiVec3{ static_cast<float>(U.X), static_cast<float>(U.Z), static_cast<float>(U.Y) };
	}
	TerrainWorld->SetCamera(Cam);

	const LogiFrameId Frame = TerrainWorld->Step(DeltaTime);
	if (Frame == 0)
	{
		return;
	}

	++FrameCounter;
	LogiFramePacket Packet{};
	if (TerrainWorld->GetFramePacket(Frame, Packet))
	{
		// Floating-origin rebase: park the host actor at the kernel's render origin so the proc-mesh draws
		// near it (small coords). When the origin snaps to a new grid cell the existing sections are stale
		// (built relative to the old origin) -- drop them and let them rebuild this frame.
		const FVector HostTarget(
			Packet.local_frame.origin.x * LogiTerrainToUnreal,
			Packet.local_frame.origin.z * LogiTerrainToUnreal,
			Packet.local_frame.origin.y * LogiTerrainToUnreal);
		if (AActor* H = HostActor.Get())
		{
			if (!H->GetActorLocation().Equals(HostTarget, 1.0))
			{
				H->SetActorLocation(HostTarget);
				Mesh->ClearAllMeshSections();
				TileSection.Reset();
				FreeSections.Reset();
				NextSection = 0;
#if LOGI_TERRAIN_NANITE_LANE
				// Nanite tiles are origin-INDEPENDENT (tile-local geometry; the origin lives in each
				// component's relative transform, not the vertices) -- they survive the rebase. Don't clear
				// them: the resident loop below slides each component to the new origin via SetRelativeLocation,
				// and the re-entry cache stays valid (keys are origin-free tile ids). This is what stops the
				// whole view re-encoding every time the camera crosses a REBASE_SNAP grid line.
#endif
			}
		}

#if LOGI_TERRAIN_NANITE_LANE
		if (bNaniteLaneActive)
		{
			// === Nanite production lane (Milestone E, editor/PIE only) ===
			// Build a real Nanite UStaticMesh per resident tile and draw it via a pooled
			// UStaticMeshComponent, so terrain gets native Nanite LOD / GPU-driven culling / VSM
			// (no SetCastShadow(false) hack). The build is ASYNC (encode runs on the static-mesh
			// worker pool; BuildFromMeshDescriptions returns with IsCompiling()==true and the tile
			// pops in a few frames later, self-healing via PostStaticMeshCompilation). We throttle
			// SUBMISSIONS, not game-thread cost: at most NaniteTileBuildsPerStep builds are submitted
			// per step, bounding in-flight worker builds + encode memory; the rest submit on later steps.
			int32 BuildBudget = FMath::Max(1, GetDefault<ULogiBevyDeveloperSettings>()->NaniteTileBuildsPerStep);
			TSet<uint64>& Resident = ResidentScratch; // reused member; Empty reuses backing (no per-frame free)
			Resident.Empty(Packet.terrain_patch_count);
			for (uint32 i = 0; i < Packet.terrain_patch_count; ++i)
			{
				const LogiTerrainPatchDraw& P = Packet.terrain_patches[i];
				const uint64 Key = P.patch_handle;
				if (P.bounds.radius > GiantTileRadiusMeters) // same giant-root skip as the proc-mesh lane
				{
					if (FNaniteTile* Giant = TileNanite.Find(Key))
					{
						ReleaseNaniteTile(*Giant);
						TileNanite.Remove(Key);
					}
					continue;
				}
				Resident.Add(Key);
				if (FNaniteTile* Existing = TileNanite.Find(Key))
				{
					Existing->LastSeen = FrameCounter; // already built -> just refresh its LRU stamp
					// Tile-local mesh: keep the component on the tile by (re)setting its origin-relative
					// offset. Constant between rebases (SetRelativeLocation early-outs when unchanged); on a
					// rebase this is what slides the resident tile to the new origin -- no re-encode.
					if (UStaticMeshComponent* C = Existing->Comp.Get())
					{
						C->SetRelativeLocation(NaniteTileRelativeLocation(P));
					}
					continue;
				}
				// Re-entry cache: a recently-evicted tile re-entered the view -> reuse its already-encoded
				// Nanite mesh instead of re-running the (expensive, async) encode. Cheap -> NOT budgeted.
				if (FLogiNaniteCachedTile* Cached = NaniteMeshCache.Find(Key))
				{
					if (UStaticMesh* CachedMesh = Cached->Mesh.Get())
					{
						if (UStaticMeshComponent* Comp = AcquireStaticMeshComponent())
						{
							Comp->SetStaticMesh(CachedMesh); // the component now roots the mesh
							Comp->SetRelativeLocation(NaniteTileRelativeLocation(P)); // tile-local mesh -> place via transform
							TileNanite.Add(Key, FNaniteTile{ Comp, FrameCounter });
							NaniteMeshCache.Remove(Key); // moved cache -> resident
						}
						continue; // reused (or host gone -> left cached, retry next step); no rebuild
					}
					NaniteMeshCache.Remove(Key); // stale entry -> fall through to a fresh build
				}
				if (BuildBudget <= 0)
				{
					continue; // over the per-step build budget -> this tile builds on a later step
				}
				LogiBufferView VView{};
				LogiBufferView IView{};
				if (TerrainWorld->AcquireBufferView(P.vertex_buffer, VView)
					&& TerrainWorld->AcquireBufferView(P.index_buffer, IView)
					&& VView.data != nullptr && IView.data != nullptr && P.vertex_count > 0)
				{
					// This is a confirmed build attempt: spend the budget regardless of the outcome
					// below (L11). A degenerate-rejected or host-torn-down attempt that did NOT spend
					// would otherwise be retried every frame at full budget -> a hot rebuild loop.
					--BuildBudget;
					if (UStaticMesh* TileMesh = BuildTileNaniteMesh(this, P, VView, IView, nullptr))
					{
						if (UStaticMeshComponent* Comp = AcquireStaticMeshComponent())
						{
							Comp->SetStaticMesh(TileMesh);
							Comp->SetRelativeLocation(NaniteTileRelativeLocation(P)); // tile-local mesh -> place via transform
							TileNanite.Add(Key, FNaniteTile{ Comp, FrameCounter });
						}
						else
						{
							// Host actor is gone (torn down mid-tick): the freshly built mesh has no home.
							// Leave it unreferenced (RF_Transient + no owner ref) so GC reclaims it once its
							// in-flight async build releases the task's reference -- do NOT MarkAsGarbage
							// here, which could race that build task. Stop: every further acquire this step
							// also fails, and re-attempting would never converge while the host is gone.
							(void)TileMesh;
							break;
						}
					}
					// else: BuildTileNaniteMesh rejected the tile as degenerate (no triangles / bad
					// view). Budget already spent above, so we don't hammer it again this step.
				}
			}
			// Return tiles that left the resident set to the component pool.
			DepartedScratch.Reset(); // reused member (keeps capacity, no per-frame alloc)
			for (const TPair<uint64, FNaniteTile>& Pair : TileNanite)
			{
				if (!Resident.Contains(Pair.Key)) { DepartedScratch.Add(Pair.Key); }
			}
			for (uint64 K : DepartedScratch)
			{
				// Cache the departed tile's encoded mesh (don't drop it) so a quick re-visit skips the rebuild.
				CacheNaniteTile(K, TileNanite.FindAndRemoveChecked(K));
			}
			TrimNaniteCache();         // bound the re-entry cache (NaniteReentryCacheSize, LRU)
			EvictNaniteIfOverBudget(); // bounded-LRU safety cap (parity with proc-mesh EvictIfOverBudget)
		}
		else
#endif
		{
		TSet<uint64>& Resident = ResidentScratch; // reused member; Empty reuses backing (no per-frame free)
		Resident.Empty(Packet.terrain_patch_count);
		for (uint32 i = 0; i < Packet.terrain_patch_count; ++i)
		{
			const LogiTerrainPatchDraw& P = Packet.terrain_patches[i];
			const uint64 Key = P.patch_handle;
			// Skip world-spanning coarse/root fallbacks (lod 0-2, radius > 2.5 km): drawn as one giant
			// proc-mesh section they bloat the component bounds to ~world size and break UE's culling,
			// dropping the real fine terrain. Drop any existing giant section so it stops polluting bounds.
			if (P.bounds.radius > GiantTileRadiusMeters)
			{
				if (FTileEntry* Giant = TileSection.Find(Key))
				{
					Mesh->ClearMeshSection(Giant->Section);
					FreeSections.Add(Giant->Section);
					TileSection.Remove(Key);
				}
				continue;
			}
			Resident.Add(Key);

			if (FTileEntry* Existing = TileSection.Find(Key))
			{
				// Sections are never hidden now (non-resident tiles are cleared below), so the decoded
				// mesh just stays live -- only refresh its LRU timestamp.
				Existing->LastSeen = FrameCounter;
				continue;
			}

			LogiBufferView VView{};
			LogiBufferView IView{};
			if (TerrainWorld->AcquireBufferView(P.vertex_buffer, VView)
				&& TerrainWorld->AcquireBufferView(P.index_buffer, IView)
				&& VView.data != nullptr && IView.data != nullptr && P.vertex_count > 0)
			{
				const int32 Section = (FreeSections.Num() > 0) ? FreeSections.Pop(EAllowShrinking::No) : NextSection++;
				BuildSection(Mesh, Section, P, VView, IView);
				TileSection.Add(Key, FTileEntry{ Section, FrameCounter });
			}
		}

		// Clear (do NOT hide) tiles that left the resident set. UProceduralMeshComponent::UpdateLocalBounds()
		// unions SectionLocalBox over ALL sections ignoring visibility, and SetMeshSectionVisible never
		// updates bounds -- so merely hiding a tile leaves it inflating the component bounds and wrecking
		// frustum culling. Collect keys first (can't mutate TileSection while iterating it), then clear.
		DepartedScratch.Reset(); // reused member (keeps capacity, no per-frame alloc)
		for (const TPair<uint64, FTileEntry>& Pair : TileSection)
		{
			if (!Resident.Contains(Pair.Key)) { DepartedScratch.Add(Pair.Key); }
		}
		for (uint64 K : DepartedScratch)
		{
			const FTileEntry E = TileSection.FindAndRemoveChecked(K);
			Mesh->ClearMeshSection(E.Section);
			FreeSections.Add(E.Section);
		}

		EvictIfOverBudget();
		} // end proc-mesh lane

#if WITH_EDITOR
		// Mirror telemetry + resident tiles for the editor Logi Terrain panel.
		EditorStats.FrameId = Packet.frame_id;
		EditorStats.ResidentTiles = static_cast<int32>(Packet.telemetry.resident_tiles);
		EditorStats.InFlight = static_cast<int32>(Packet.telemetry.in_flight);
		EditorStats.BytesResident = static_cast<int64>(Packet.telemetry.bytes_resident);
		EditorStats.PeakBytesResident = static_cast<int64>(Packet.telemetry.peak_bytes_resident);
		EditorStats.Evictions = static_cast<int64>(Packet.telemetry.evictions);
		EditorStats.Fallbacks = static_cast<int64>(Packet.telemetry.fallbacks_used);
		EditorStats.RequestsCompleted = static_cast<int64>(Packet.telemetry.requests_completed);
		EditorTiles.Reset(Packet.terrain_patch_count);
		const LogiDVec3 TO = Packet.local_frame.origin;
		for (uint32 t = 0; t < Packet.terrain_patch_count; ++t)
		{
			const LogiTerrainPatchDraw& TP = Packet.terrain_patches[t];
			FLogiTerrainTileRow Row;
			Row.Key = TP.patch_handle;
			Row.Lod = TP.lod;
			Row.Center = FVector(
				(TO.x + static_cast<double>(TP.bounds.center.x)) * LogiTerrainToUnreal,
				(TO.z + static_cast<double>(TP.bounds.center.z)) * LogiTerrainToUnreal,
				(TO.y + static_cast<double>(TP.bounds.center.y)) * LogiTerrainToUnreal);
			Row.Radius = TP.bounds.radius * static_cast<float>(LogiTerrainToUnreal);
			EditorTiles.Add(Row);
		}
#endif
	}
	TerrainWorld->ReleaseFrame(Frame);
#else
	(void)DeltaTime;
#endif
}

#if WITH_EDITOR
void ULogiTerrainSubsystem::GetEditorTerrainState(FLogiTerrainStats& OutStats, TArray<FLogiTerrainTileRow>& OutTiles) const
{
	OutStats = EditorStats;
	OutTiles = EditorTiles;
}
#endif
