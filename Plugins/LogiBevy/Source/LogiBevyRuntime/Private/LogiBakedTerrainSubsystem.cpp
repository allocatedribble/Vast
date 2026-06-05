// Copyright LogiUE. All Rights Reserved.

#include "LogiBakedTerrainSubsystem.h"
#include "LogiBevyTypes.h"
#include "LogiBevyDeveloperSettings.h"
#include "Engine/World.h"
#include "GameFramework/Actor.h"
#include "ProceduralMeshComponent.h"
#include "Materials/MaterialInterface.h"
#include "Materials/MaterialInstanceDynamic.h"
#include "Misc/Paths.h"

#if LOGI_BEVY_HAVE_TERRAIN
#include "logi_terrain_ffi.h"
// Defined in LogiBevyFfi.cpp: false once a terrain-DLL ABI mismatch is detected at load. The
// logi_baked_* exports live in the same delay-loaded logi_terrain.dll, so consult this first.
bool LogiBevy_IsTerrainAbiOk();
#endif

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
		// Game + PIE, plus the main level-editor world (NOT preview/thumbnail worlds).
		return World->IsGameWorld() || World->WorldType == EWorldType::Editor;
	}
#endif
	return false;
}

TStatId ULogiBakedTerrainSubsystem::GetStatId() const
{
	RETURN_QUICK_DECLARE_CYCLE_STAT(ULogiBakedTerrainSubsystem, STATGROUP_Tickables);
}

void ULogiBakedTerrainSubsystem::Tick(float /*DeltaTime*/)
{
	if (bLoaded)
	{
		return;
	}
	bLoaded = true; // one-shot: HE-0 serves the whole AOI statically (camera streaming is a later HE)
	LoadAndBuild();
}

void ULogiBakedTerrainSubsystem::LoadAndBuild()
{
#if LOGI_BEVY_HAVE_TERRAIN
	if (!LogiBevy_IsTerrainAbiOk())
	{
		UE_LOG(LogLogiBevy, Warning, TEXT("Baked terrain: terrain DLL ABI not OK; skipping load."));
		return;
	}
	UWorld* World = GetWorld();
	if (World == nullptr)
	{
		return;
	}

	const ULogiBevyDeveloperSettings* S = GetDefault<ULogiBevyDeveloperSettings>();
	// Relative store dirs resolve under <Project>/Saved (the default logi_bake output location).
	const FString StoreDir = FPaths::IsRelative(S->BakedTileStoreDir)
		? FPaths::Combine(FPaths::ProjectSavedDir(), S->BakedTileStoreDir)
		: S->BakedTileStoreDir;

	// Meshing toggle → the Rust create-info (uniform grid, or adaptive TIN at the configured error).
	LogiBakedCreateInfo Info{};
	Info.meshing_mode = (S->BakedTerrainMeshing == EBakedTerrainMeshing::TIN) ? 1u : 0u;
	Info.tin_max_error_m = FMath::Max(0.0f, S->BakedTerrainTinErrorM);

	const FTCHARToUTF8 StoreUtf8(*StoreDir);
	LogiBakedWorld* Baked = logi_baked_world_create(StoreUtf8.Get(), &Info);
	if (Baked == nullptr)
	{
		UE_LOG(LogLogiBevy, Error,
			TEXT("logi_baked_world_create failed for '%s' (bake it: python -m logi_bake bake --out <dir>)."),
			*StoreDir);
		return;
	}

	LogiBakedFrame Frame{};
	if (logi_baked_world_get_frame(Baked, &Frame) != LOGI_TERRAIN_OK || Frame.tiles == nullptr)
	{
		UE_LOG(LogLogiBevy, Error, TEXT("logi_baked_world_get_frame failed."));
		logi_baked_world_destroy(Baked);
		return;
	}

	FActorSpawnParameters SpawnParams;
	SpawnParams.ObjectFlags |= RF_Transient;
	AActor* Host = World->SpawnActor<AActor>(AActor::StaticClass(), FTransform::Identity, SpawnParams);
	if (Host == nullptr)
	{
		logi_baked_world_destroy(Baked);
		return;
	}
#if WITH_EDITOR
	Host->SetActorLabel(TEXT("LogiBakedTerrain"));
#endif
	Host->Tags.Add(TEXT("LogiBakedTerrain"));

	UProceduralMeshComponent* Mesh = NewObject<UProceduralMeshComponent>(Host, TEXT("LogiBakedTerrainMesh"));
	Host->SetRootComponent(Mesh);
	Mesh->SetMobility(EComponentMobility::Movable);
	Mesh->RegisterComponent();

	// Terrain material. When the Nanite-displacement toggle is on and a displacement material is set, use a
	// MID of it with its "DisplacementScale" parameter applied (first-person micro-detail); otherwise the
	// default terrain material (proc-mesh default if that's unauthored too -- untextured but visible). The
	// MID is rooted by the proc-mesh component's material slots (GC-reachable via the host actor).
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
		if (Mat == nullptr)
		{
			UE_LOG(LogLogiBevy, Warning,
				TEXT("Baked terrain: displacement material set but failed to load/instantiate; using default."));
		}
	}
	if (Mat == nullptr)
	{
		Mat = LoadObject<UMaterialInterface>(
			nullptr, TEXT("/Game/Terrain/M_LogiBakedTerrain.M_LogiBakedTerrain"), nullptr, LOAD_NoWarn | LOAD_Quiet);
	}

	int32 BuiltTris = 0;
	for (uint32 i = 0; i < Frame.tile_count; ++i)
	{
		const LogiBakedTile& T = Frame.tiles[i];
		const uint32 Stride = T.vertex_stride;
		if (Stride < 32 || T.vertices == nullptr || T.indices == nullptr)
		{
			continue;
		}
		const uint32 VCount = FMath::Min<uint32>(T.vertex_count, static_cast<uint32>(T.vertices_len / Stride));

		TArray<FVector> Positions;
		TArray<FVector> Normals;
		TArray<FVector2D> UVs;
		Positions.Reserve(VCount);
		Normals.Reserve(VCount);
		UVs.Reserve(VCount);
		for (uint32 v = 0; v < VCount; ++v)
		{
			const float* F = reinterpret_cast<const float*>(T.vertices + static_cast<uint64>(v) * Stride);
			Positions.Add(FVector(F[0], F[1], F[2])); // already cm, Z-up LH
			Normals.Add(FVector(F[3], F[4], F[5]));
			UVs.Add(FVector2D(F[6], F[7]));
		}

		const uint32* IData = reinterpret_cast<const uint32*>(T.indices);
		TArray<int32> Triangles;
		Triangles.Reserve(T.index_count);
		for (uint32 t = 0; t + 3 <= T.index_count; t += 3)
		{
			const uint32 I0 = IData[t];
			const uint32 I1 = IData[t + 1];
			const uint32 I2 = IData[t + 2];
			if (I0 >= VCount || I1 >= VCount || I2 >= VCount)
			{
				continue;
			}
			Triangles.Add(static_cast<int32>(I0));
			Triangles.Add(static_cast<int32>(I1));
			Triangles.Add(static_cast<int32>(I2));
		}

		const int32 Section = static_cast<int32>(i);
		Mesh->CreateMeshSection_LinearColor(Section, Positions, Triangles, Normals, UVs,
			TArray<FLinearColor>(), TArray<FProcMeshTangent>(), /*bCreateCollision=*/false);
		if (Mat != nullptr)
		{
			Mesh->SetMaterial(Section, Mat);
		}
		BuiltTris += Triangles.Num() / 3;
	}

	UE_LOG(LogLogiBevy, Log,
		TEXT("Baked terrain: %u tiles, %d tris from '%s' (origin ECEF %.0f, %.0f, %.0f)."),
		Frame.tile_count, BuiltTris, *StoreDir,
		Frame.origin_ecef[0], Frame.origin_ecef[1], Frame.origin_ecef[2]);

	HostActor = Host;
	// Geometry is copied into UE buffers; release the Rust-owned world (frame pointers dangle after this).
	logi_baked_world_destroy(Baked);
#endif // LOGI_BEVY_HAVE_TERRAIN
}

void ULogiBakedTerrainSubsystem::Deinitialize()
{
	if (AActor* Host = HostActor.Get())
	{
		Host->Destroy();
	}
	HostActor.Reset();
	Super::Deinitialize();
}
