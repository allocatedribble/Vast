// Copyright LogiUE. All Rights Reserved.

#include "LogiBakedTerrainSubsystem.h"
#include "LogiBevyTypes.h"
#include "LogiBevyDeveloperSettings.h"
#include "LogiTileRenderShared.h" // FLogiStreamView/LogiResolveStreamView/LogiBuildTile*(shared render core)
#include "Engine/World.h"
#include "GameFramework/Actor.h"
#include "GameFramework/PlayerController.h"
#include "Camera/PlayerCameraManager.h"
#include "Components/SceneComponent.h"
#include "Components/StaticMeshComponent.h"
#include "ProceduralMeshComponent.h"
#include "Materials/MaterialInterface.h"
#include "Materials/MaterialInstanceDynamic.h"
#include "Misc/Paths.h"

#if WITH_EDITOR
#include "LevelEditorViewport.h" // GCurrentLevelEditingViewportClient
#endif

#if LOGI_TERRAIN_NANITE_LANE
// Editor-only Nanite lane (ADR-0008): build a transient Nanite UStaticMesh per baked tile.
#include "Engine/StaticMesh.h"
#include "StaticMeshAttributes.h"
#include "MeshDescription.h"
#include "Materials/Material.h"
#endif

#if LOGI_BEVY_HAVE_TERRAIN
#include "logi_terrain_ffi.h"
// Defined in LogiBevyFfi.cpp: false once a terrain-DLL ABI mismatch is detected. The logi_baked_* exports
// live in the same delay-loaded logi_terrain.dll, so consult this first.
bool LogiBevy_IsTerrainAbiOk();
#endif

namespace
{
#if LOGI_BEVY_HAVE_TERRAIN
	// The streaming viewpoint (FLogiStreamView + LogiResolveStreamView), the proc-mesh section builder
	// (LogiBuildTileSection) and the Nanite mesh builder (LogiBuildTileNaniteMesh) now live in
	// LogiTileRenderShared — one render core shared with the globe lane instead of a per-subsystem copy.
#endif // LOGI_BEVY_HAVE_TERRAIN
}

bool ULogiBakedTerrainSubsystem::ShouldCreateSubsystem(UObject* Outer) const
{
	if (!Super::ShouldCreateSubsystem(Outer))
	{
		return false;
	}
#if LOGI_BEVY_HAVE_TERRAIN
	if (const UWorld* World = Cast<UWorld>(Outer))
	{
		if (!GetDefault<ULogiBevyDeveloperSettings>()->bShowBakedTerrain)
		{
			return false;
		}
		return World->IsGameWorld() || World->WorldType == EWorldType::Editor;
	}
#endif
	return false;
}

TStatId ULogiBakedTerrainSubsystem::GetStatId() const
{
	RETURN_QUICK_DECLARE_CYCLE_STAT(ULogiBakedTerrainSubsystem, STATGROUP_Tickables);
}

bool ULogiBakedTerrainSubsystem::EnsureWorld()
{
#if LOGI_BEVY_HAVE_TERRAIN
	if (BakedWorld != nullptr)
	{
		return true;
	}
	if (bCreateAttempted)
	{
		return false; // a prior create failed — don't retry every tick
	}
	bCreateAttempted = true;

	if (!LogiBevy_IsTerrainAbiOk())
	{
		UE_LOG(LogLogiBevy, Warning, TEXT("Baked terrain: terrain DLL ABI not OK; skipping."));
		return false;
	}

	const ULogiBevyDeveloperSettings* S = GetDefault<ULogiBevyDeveloperSettings>();
	// Meshing toggle + skirt are common to both lanes (store + procedural).
	const uint32 MeshingMode = (S->BakedTerrainMeshing == EBakedTerrainMeshing::TIN) ? 1u : 0u;
	const float TinErr = FMath::Max(0.0f, S->BakedTerrainTinErrorM);
	const float Skirt = FMath::Max(0.0f, S->BakedTerrainSkirtDepthM);

	if (S->bBakedTerrainProcedural)
	{
		// One-engine PROCEDURAL lane: generate the SSE-LOD pyramid from the canonical fractal field
		// (gameplay's surface) — no baked store on disk. Same render / select / build path as a store.
		LogiBakedProceduralInfo PInfo{};
		PInfo.seed = static_cast<uint64>(FMath::Max(0, S->BakedTerrainProceduralSeed));
		PInfo.origin_lon_deg = S->BakedTerrainProceduralOriginLongitude;
		PInfo.origin_lat_deg = S->BakedTerrainProceduralOriginLatitude;
		PInfo.origin_height_m = 0.0;
		PInfo.radius_m = FMath::Max(1.0, S->BakedTerrainProceduralRadiusM);
		PInfo.leaf_level = static_cast<uint32>(FMath::Clamp(S->BakedTerrainProceduralLeafLevel, 0, 10));
		PInfo.grid_n = static_cast<uint32>(FMath::Clamp(S->BakedTerrainProceduralGridN, 2, 513));
		PInfo.meshing_mode = MeshingMode;
		PInfo.tin_max_error_m = TinErr;
		PInfo.skirt_depth_m = Skirt;
		BakedWorld = logi_baked_world_create_procedural(&PInfo);
		if (BakedWorld == nullptr)
		{
			UE_LOG(LogLogiBevy, Error, TEXT("logi_baked_world_create_procedural failed."));
			return false;
		}
		UE_LOG(LogLogiBevy, Log,
			TEXT("Baked terrain world: PROCEDURAL (seed %d, origin %.5f,%.5f, radius %.0f m, leaf %d, grid %d, meshing %s)."),
			S->BakedTerrainProceduralSeed, S->BakedTerrainProceduralOriginLatitude, S->BakedTerrainProceduralOriginLongitude,
			PInfo.radius_m, S->BakedTerrainProceduralLeafLevel, S->BakedTerrainProceduralGridN,
			MeshingMode == 1 ? TEXT("TIN") : TEXT("Uniform"));
	}
	else
	{
		const FString StoreDir = FPaths::IsRelative(S->BakedTileStoreDir)
			? FPaths::Combine(FPaths::ProjectSavedDir(), S->BakedTileStoreDir)
			: S->BakedTileStoreDir;
		LogiBakedCreateInfo Info{};
		Info.meshing_mode = MeshingMode;
		Info.tin_max_error_m = TinErr;
		Info.skirt_depth_m = Skirt;
		const FTCHARToUTF8 StoreUtf8(*StoreDir);
		BakedWorld = logi_baked_world_create(StoreUtf8.Get(), &Info);
		if (BakedWorld == nullptr)
		{
			UE_LOG(LogLogiBevy, Error,
				TEXT("logi_baked_world_create failed for '%s' (bake it, or enable 'Baked Terrain: Procedural')."),
				*StoreDir);
			return false;
		}
		UE_LOG(LogLogiBevy, Log, TEXT("Baked terrain world: store '%s', meshing %s, skirt %.1f m."),
			*StoreDir, MeshingMode == 1 ? TEXT("TIN") : TEXT("Uniform"), Skirt);
	}

#if LOGI_TERRAIN_NANITE_LANE
	bUseNaniteLane = S->bBakedTerrainNaniteDisplacement; // Nanite tessellation is editor-only (ADR-0008)
#else
	bUseNaniteLane = false;
#endif
	return true;
#else
	return false;
#endif
}

void ULogiBakedTerrainSubsystem::EnsureHost()
{
	if (!HostActor.IsValid())
	{
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
		Host->SetActorLabel(TEXT("LogiBakedTerrain"));
#endif
		Host->Tags.Add(TEXT("LogiBakedTerrain"));
		USceneComponent* Root = NewObject<USceneComponent>(Host, TEXT("Root"));
		Host->SetRootComponent(Root);
		Root->RegisterComponent();
		HostActor = Host;
	}

	AActor* Host = HostActor.Get();
	if (Host == nullptr)
	{
		return;
	}

	// Proc-mesh lane: one shared component carrying a section per tile.
	if (!bUseNaniteLane && !ProcMesh.IsValid())
	{
		UProceduralMeshComponent* Mesh = NewObject<UProceduralMeshComponent>(Host, TEXT("LogiBakedTerrainMesh"));
		Mesh->SetupAttachment(Host->GetRootComponent());
		Mesh->SetMobility(EComponentMobility::Movable);
		Mesh->RegisterComponent();
		ProcMesh = Mesh;
	}

	// Resolve the terrain material once: the displacement MID when the toggle + material are set, else the
	// default terrain material (proc-mesh default if that's unauthored too — untextured but visible).
	if (!TerrainMaterial.IsValid())
	{
		const ULogiBevyDeveloperSettings* S = GetDefault<ULogiBevyDeveloperSettings>();
		UMaterialInterface* Mat = nullptr;
		if (S->bBakedTerrainNaniteDisplacement && !S->BakedTerrainDisplacementMaterial.IsNull())
		{
			if (UMaterialInterface* Disp = S->BakedTerrainDisplacementMaterial.LoadSynchronous())
			{
				if (UMaterialInstanceDynamic* MID = UMaterialInstanceDynamic::Create(Disp, this))
				{
					MID->SetScalarParameterValue(TEXT("DisplacementScale"), S->BakedTerrainDisplacementScale);
					Mat = MID;
				}
			}
		}
		if (Mat == nullptr)
		{
			Mat = LoadObject<UMaterialInterface>(
				nullptr, TEXT("/Game/Terrain/M_LogiBakedTerrain.M_LogiBakedTerrain"), nullptr, LOAD_NoWarn | LOAD_Quiet);
		}
		if (Mat != nullptr)
		{
			TerrainMaterial.Reset(Mat);
		}
	}
}

void ULogiBakedTerrainSubsystem::Tick(float DeltaTime)
{
#if LOGI_BEVY_HAVE_TERRAIN
	UWorld* World = GetWorld();
	if (World == nullptr)
	{
		return;
	}
	// ~30 Hz step throttle: the editor renders far faster than streaming needs; the meshes persist and
	// render every frame, only the step/build cadence is capped.
	StepAccum += DeltaTime;
	constexpr float StepSeconds = 1.0f / 30.0f;
	if (StepAccum < StepSeconds)
	{
		return;
	}
	StepAccum = 0.0f;

	if (!EnsureWorld())
	{
		return;
	}
	EnsureHost();
	AActor* Host = HostActor.Get();
	if (Host == nullptr)
	{
		return;
	}

	FLogiStreamView View;
	if (!LogiResolveStreamView(World, View))
	{
		return; // no viewport this tick
	}

	// SSE-LOD selection over the baked pyramid (coarse far, fine near). A single-level store renders all.
	const ULogiBevyDeveloperSettings* S = GetDefault<ULogiBevyDeveloperSettings>();
	const float Sse = FMath::Clamp(S->BakedTerrainSse, 1.0f, 256.0f);
	int32 BuildBudget = FMath::Max(1, S->BakedTerrainBuildsPerStep); // spread builds over steps (anti-spike)

	LogiBakedFrame Frame{};
	if (logi_baked_world_select(BakedWorld, static_cast<float>(View.Loc.X), static_cast<float>(View.Loc.Y),
			static_cast<float>(View.Loc.Z), View.FovY, View.ViewportHeight, Sse, &Frame) != LOGI_TERRAIN_OK
		|| Frame.tiles == nullptr)
	{
		return;
	}

	++FrameCounter;
	UProceduralMeshComponent* PMesh = ProcMesh.Get();
	ResidentScratch.Empty(Frame.tile_count);
	for (uint32 i = 0; i < Frame.tile_count; ++i)
	{
		const LogiBakedTile& T = Frame.tiles[i];
		const uint64 Key = T.tile_key;
		ResidentScratch.Add(Key);

		if (FBakedTileEntry* Existing = Tiles.Find(Key))
		{
			Existing->LastSeen = FrameCounter; // already built — refresh
			continue;
		}
		if (T.vertex_count == 0 || T.vertices == nullptr)
		{
			continue;
		}
		if (BuildBudget <= 0)
		{
			continue; // over the per-step build budget — builds on a later step (stays selected in Rust)
		}
		--BuildBudget;

		FBakedTileEntry Entry;
		Entry.LastSeen = FrameCounter;
#if LOGI_TERRAIN_NANITE_LANE
		if (bUseNaniteLane)
		{
			UStaticMesh* SM = LogiBuildTileNaniteMesh(this, T.vertices, T.vertex_stride, T.vertices_len,
				T.vertex_count, T.indices, T.index_count, TerrainMaterial.Get(), TEXT("LogiBakedTerrain"));
			if (SM == nullptr)
			{
				continue;
			}
			UStaticMeshComponent* Comp = NewObject<UStaticMeshComponent>(Host, NAME_None, RF_Transient);
			Comp->SetupAttachment(Host->GetRootComponent());
			Comp->SetMobility(EComponentMobility::Movable);
			Comp->RegisterComponent();
			Comp->SetRelativeLocation(FVector::ZeroVector); // verts are absolute UE cm (origin at world 0)
			Comp->SetStaticMesh(SM);
			if (UMaterialInterface* Mat = TerrainMaterial.Get())
			{
				Comp->SetMaterial(0, Mat);
			}
			Entry.NaniteComp.Reset(Comp);
		}
		else
#endif
		{
			if (PMesh == nullptr)
			{
				continue;
			}
			const int32 Section = (FreeSections.Num() > 0) ? FreeSections.Pop(EAllowShrinking::No) : NextSection++;
			LogiBuildTileSection(PMesh, Section, T.vertices, T.vertex_stride, T.vertices_len, T.vertex_count,
				T.indices, T.index_count);
			if (UMaterialInterface* Mat = TerrainMaterial.Get())
			{
				PMesh->SetMaterial(Section, Mat);
			}
			Entry.Section = Section;
		}
		Tiles.Add(Key, MoveTemp(Entry));
	}

	EvictDeparted(ResidentScratch);
#else
	(void)DeltaTime;
#endif
}

void ULogiBakedTerrainSubsystem::EvictDeparted(const TSet<uint64>& Resident)
{
	DepartedScratch.Reset();
	for (const TPair<uint64, FBakedTileEntry>& Pair : Tiles)
	{
		if (!Resident.Contains(Pair.Key))
		{
			DepartedScratch.Add(Pair.Key);
		}
	}
	UProceduralMeshComponent* PMesh = ProcMesh.Get();
	for (uint64 Key : DepartedScratch)
	{
		FBakedTileEntry Entry;
		if (!Tiles.RemoveAndCopyValue(Key, Entry))
		{
			continue;
		}
		if (Entry.Section != INDEX_NONE)
		{
			if (PMesh != nullptr)
			{
				PMesh->ClearMeshSection(Entry.Section);
			}
			FreeSections.Add(Entry.Section);
		}
		if (UStaticMeshComponent* Comp = Entry.NaniteComp.Get())
		{
			Comp->DestroyComponent();
		}
	}
}

void ULogiBakedTerrainSubsystem::ReleaseWorld()
{
#if LOGI_BEVY_HAVE_TERRAIN
	if (BakedWorld != nullptr)
	{
		logi_baked_world_destroy(BakedWorld);
		BakedWorld = nullptr;
	}
#endif
}

void ULogiBakedTerrainSubsystem::Deinitialize()
{
	if (AActor* Host = HostActor.Get())
	{
		Host->Destroy();
	}
	HostActor.Reset();
	ProcMesh.Reset();
	TerrainMaterial.Reset();
	Tiles.Reset(); // releases any Nanite-component strong refs
	FreeSections.Reset();
	NextSection = 0;
	FrameCounter = 0;
	ReleaseWorld();
	Super::Deinitialize();
}
