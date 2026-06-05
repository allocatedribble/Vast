// Copyright LogiUE. All Rights Reserved.

#include "LogiBuildingSubsystem.h"
#include "LogiBevyTypes.h"
#include "LogiBevyDeveloperSettings.h"
#include "Engine/World.h"
#include "GameFramework/Actor.h"
#include "ProceduralMeshComponent.h"
#include "Materials/MaterialInterface.h"
#include "Misc/Paths.h"

#if LOGI_BEVY_HAVE_TERRAIN
#include "logi_terrain_ffi.h"
// Defined in LogiBevyFfi.cpp: false once a terrain-DLL ABI mismatch is detected at load. The
// logi_building_* exports live in the same delay-loaded logi_terrain.dll, so consult this first.
bool LogiBevy_IsTerrainAbiOk();
#endif

bool ULogiBuildingSubsystem::ShouldCreateSubsystem(UObject* Outer) const
{
	if (!Super::ShouldCreateSubsystem(Outer))
	{
		return false;
	}
#if LOGI_BEVY_HAVE_TERRAIN
	if (const UWorld* World = Cast<UWorld>(Outer))
	{
		if (!GetDefault<ULogiBevyDeveloperSettings>()->bShowAgiBuilding)
		{
			return false;
		}
		// Game + PIE, plus the main level-editor world (NOT preview/thumbnail worlds).
		return World->IsGameWorld() || World->WorldType == EWorldType::Editor;
	}
#endif
	return false;
}

TStatId ULogiBuildingSubsystem::GetStatId() const
{
	RETURN_QUICK_DECLARE_CYCLE_STAT(ULogiBuildingSubsystem, STATGROUP_Tickables);
}

void ULogiBuildingSubsystem::Tick(float /*DeltaTime*/)
{
	if (bLoaded)
	{
		return;
	}
	bLoaded = true; // one-shot: load the building on the first tick (world is ready to spawn actors)
	LoadAndBuild();
}

void ULogiBuildingSubsystem::Deinitialize()
{
	if (AActor* Host = HostActor.Get())
	{
		Host->Destroy();
	}
	HostActor.Reset();
	Super::Deinitialize();
}

void ULogiBuildingSubsystem::LoadAndBuild()
{
#if LOGI_BEVY_HAVE_TERRAIN
	if (!LogiBevy_IsTerrainAbiOk())
	{
		UE_LOG(LogLogiBevy, Warning, TEXT("AGI building: terrain DLL ABI not OK; skipping load."));
		return;
	}
	UWorld* World = GetWorld();
	if (World == nullptr)
	{
		return;
	}

	const FString Base = FPaths::Combine(FPaths::ProjectContentDir(), TEXT("AGI_HQ"));
	const FString DaePath = FPaths::Combine(Base, TEXT("AGI_HQ.dae"));
	const FString JpgPath = FPaths::Combine(Base, TEXT("AGI_HQ_TextureAtlas0.jpg"));

	// The FFI takes UTF-8 C strings; keep the converters alive across the call.
	const FTCHARToUTF8 DaeUtf8(*DaePath);
	const FTCHARToUTF8 JpgUtf8(*JpgPath);

	// doc.kml placement of the AGI HQ model (lon, lat, alt, heading).
	LogiPlacement Placement{ -75.5967013459, 40.0386977494, 50.0, 90.0 };

	LogiBuilding* Building = logi_building_load(DaeUtf8.Get(), JpgUtf8.Get(), &Placement);
	if (Building == nullptr)
	{
		UE_LOG(LogLogiBevy, Error, TEXT("logi_building_load failed for '%s' (is the file present?)."), *DaePath);
		return;
	}

	LogiBuildingData Data{};
	if (logi_building_get_data(Building, &Data) != LOGI_TERRAIN_OK || Data.vertices == nullptr)
	{
		UE_LOG(LogLogiBevy, Error, TEXT("logi_building_get_data failed."));
		logi_building_destroy(Building);
		return;
	}

	// Spawn a transient host actor + proc-mesh at the origin (M1 renders at local origin; the ECEF
	// origin is carried for the globe lane). The vertices are already in Unreal space (cm, Z-up LH) and
	// the winding is already fixed by the Rust loader -- copy them straight in.
	FActorSpawnParameters SpawnParams;
	SpawnParams.ObjectFlags |= RF_Transient;
	AActor* Host = World->SpawnActor<AActor>(AActor::StaticClass(), FTransform::Identity, SpawnParams);
	if (Host == nullptr)
	{
		logi_building_destroy(Building);
		return;
	}
#if WITH_EDITOR
	Host->SetActorLabel(TEXT("AGI_HQ_Building"));
#endif
	Host->Tags.Add(TEXT("LogiBuilding"));

	UProceduralMeshComponent* Mesh = NewObject<UProceduralMeshComponent>(Host, TEXT("AGI_HQ_Mesh"));
	Host->SetRootComponent(Mesh);
	Mesh->SetMobility(EComponentMobility::Movable);
	Mesh->RegisterComponent();

	const uint32 VCount = Data.vertex_count;
	const uint8* VData = Data.vertices;
	TArray<FVector> Positions;
	TArray<FVector> Normals;
	TArray<FVector2D> UVs;
	Positions.Reserve(VCount);
	Normals.Reserve(VCount);
	UVs.Reserve(VCount);
	for (uint32 v = 0; v < VCount; ++v)
	{
		float F[8]; FMemory::Memcpy(F, VData + static_cast<uint64>(v) * 32, sizeof(F)); // aligned copy: Rust vertex buffer is byte-aligned (`*const u8`); a direct `(const float*)` deref is UB on strict-alignment targets (audit)
		Positions.Add(FVector(F[0], F[1], F[2])); // already cm, Z-up
		Normals.Add(FVector(F[3], F[4], F[5]));
		UVs.Add(FVector2D(F[6], F[7]));
	}

	const uint8* IBytes = Data.indices; // byte ptr; each index read via aligned Memcpy below (audit)
	TArray<int32> Triangles;
	Triangles.Reserve(Data.index_count);
	for (uint32 t = 0; t + 3 <= Data.index_count; t += 3)
	{
		uint32 Tri[3];
		FMemory::Memcpy(Tri, IBytes + static_cast<uint64>(t) * 4, sizeof(Tri));
		const uint32 I0 = Tri[0];
		const uint32 I1 = Tri[1];
		const uint32 I2 = Tri[2];
		if (I0 >= VCount || I1 >= VCount || I2 >= VCount)
		{
			continue;
		}
		Triangles.Add(static_cast<int32>(I0));
		Triangles.Add(static_cast<int32>(I1));
		Triangles.Add(static_cast<int32>(I2));
	}

	Mesh->CreateMeshSection_LinearColor(0, Positions, Triangles, Normals, UVs,
		TArray<FLinearColor>(), TArray<FProcMeshTangent>(), /*bCreateCollision=*/false);

	// Phase 2 (texture): apply the two-sided material whose BaseColor samples the imported atlas
	// (/Game/AGI_HQ/M_LogiBuilding, built via the asset pipeline; references AGI_HQ_TextureAtlas0). The
	// section's UVs come straight from the COLLADA TEXCOORD_0, so the atlas maps directly.
	if (UMaterialInterface* Mat = LoadObject<UMaterialInterface>(nullptr, TEXT("/Game/AGI_HQ/M_LogiBuilding.M_LogiBuilding")))
	{
		Mesh->SetMaterial(0, Mat);
	}
	else
	{
		UE_LOG(LogLogiBevy, Warning, TEXT("AGI_HQ building: material /Game/AGI_HQ/M_LogiBuilding not found; using default."));
	}

	UE_LOG(LogLogiBevy, Log,
		TEXT("AGI_HQ building: %u verts, %d tris, bounds r=%.0f cm, ECEF (%.0f, %.0f, %.0f)."),
		VCount, Triangles.Num() / 3, Data.bounds_radius,
		Data.ecef_origin[0], Data.ecef_origin[1], Data.ecef_origin[2]);

	HostActor = Host;
	// Geometry is now copied into UE buffers; release the Rust-owned building.
	logi_building_destroy(Building);
#endif // LOGI_BEVY_HAVE_TERRAIN
}
