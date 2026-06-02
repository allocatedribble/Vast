// Copyright LogiUE. All Rights Reserved.

#include "LogiTerrainSubsystem.h"
#include "LogiTerrainWorld.h"
#include "LogiBevyTypes.h"
#include "Engine/World.h"
#include "GameFramework/Actor.h"
#include "GameFramework/PlayerController.h"
#include "ProceduralMeshComponent.h"

#if LOGI_BEVY_HAVE_TERRAIN
#include "logi_terrain_ffi.h"
#endif

namespace
{
	// The terrain kernel works in metres; Unreal in centimetres.
	constexpr double LogiTerrainToUnreal = 100.0;

#if LOGI_BEVY_HAVE_TERRAIN
	// Build one procedural-mesh section from a tile's interleaved 32-byte vertices
	// (pos f3, normal f3, uv f2) + u32 indices. Terrain is Y-up + metres; Unreal is
	// Z-up + cm, so positions/normals swap Y<->Z and the winding is reversed.
	void BuildSection(UProceduralMeshComponent* Mesh, int32 Section, const LogiTerrainPatchDraw& P,
		const LogiBufferView& VView, const LogiBufferView& IView, const LogiDVec3& Origin)
	{
		const uint32 VCount = P.vertex_count;
		const uint32 Stride = VView.element_stride;
		const double X0 = Origin.x + static_cast<double>(P.local_position.x);
		const double Y0 = Origin.y + static_cast<double>(P.local_position.y);
		const double Z0 = Origin.z + static_cast<double>(P.local_position.z);

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

		const uint32 ICount = P.index_count;
		const uint32* IData = reinterpret_cast<const uint32*>(IView.data);
		TArray<int32> Triangles;
		Triangles.Reserve(ICount);
		for (uint32 t = 0; t + 3 <= ICount; t += 3)
		{
			Triangles.Add(static_cast<int32>(IData[t]));
			Triangles.Add(static_cast<int32>(IData[t + 2]));
			Triangles.Add(static_cast<int32>(IData[t + 1]));
		}

		Mesh->CreateMeshSection_LinearColor(Section, Positions, Triangles, Normals, UVs,
			TArray<FLinearColor>(), TArray<FProcMeshTangent>(), /*bCreateCollision=*/false);
	}
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
	if (AActor* Host = HostActor.Get())
	{
		Host->Destroy();
	}
	HostActor.Reset();
	TerrainMesh.Reset();
	TileSection.Reset();
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
		return World->IsGameWorld();
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
	UProceduralMeshComponent* Mesh = NewObject<UProceduralMeshComponent>(Host, TEXT("LogiTerrainMesh"));
	Host->SetRootComponent(Mesh);
	Mesh->SetMobility(EComponentMobility::Movable);
	Mesh->RegisterComponent();
	HostActor = Host;
	TerrainMesh = Mesh;
}

void ULogiTerrainSubsystem::Tick(float DeltaTime)
{
#if LOGI_BEVY_HAVE_TERRAIN
	UWorld* World = GetWorld();
	if (World == nullptr || !TerrainWorld.IsValid() || !TerrainWorld->IsValid())
	{
		return;
	}
	EnsureRenderTarget();
	UProceduralMeshComponent* Mesh = TerrainMesh.Get();
	if (Mesh == nullptr)
	{
		return;
	}

	// Feed the player camera (Unreal cm + Z-up -> terrain metres + Y-up).
	LogiCameraState Cam{};
	Cam.forward = LogiVec3{ 1.0f, 0.0f, 0.0f };
	Cam.up = LogiVec3{ 0.0f, 1.0f, 0.0f };
	Cam.fov_y_radians = 1.05f;
	Cam.viewport_width = 1920.0f;
	Cam.viewport_height = 1080.0f;
	Cam.sse_target_pixels = 8.0f;
	if (APlayerController* PC = World->GetFirstPlayerController())
	{
		FVector Loc;
		FRotator Rot;
		PC->GetPlayerViewPoint(Loc, Rot);
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

	LogiFramePacket Packet{};
	if (TerrainWorld->GetFramePacket(Frame, Packet))
	{
		TSet<uint64> Resident;
		Resident.Reserve(Packet.terrain_patch_count);
		for (uint32 i = 0; i < Packet.terrain_patch_count; ++i)
		{
			const LogiTerrainPatchDraw& P = Packet.terrain_patches[i];
			const uint64 Key = P.patch_handle;
			Resident.Add(Key);

			if (const int32* Existing = TileSection.Find(Key))
			{
				Mesh->SetMeshSectionVisible(*Existing, true); // decoded mesh is stable; just reveal
				continue;
			}

			LogiBufferView VView{};
			LogiBufferView IView{};
			if (TerrainWorld->AcquireBufferView(P.vertex_buffer, VView)
				&& TerrainWorld->AcquireBufferView(P.index_buffer, IView)
				&& VView.data != nullptr && IView.data != nullptr && P.vertex_count > 0)
			{
				const int32 Section = TileSection.Num();
				BuildSection(Mesh, Section, P, VView, IView, Packet.local_frame.origin);
				TileSection.Add(Key, Section);
			}
		}

		// Hide sections whose tile is not resident this frame (kept for reuse).
		for (const TPair<uint64, int32>& Pair : TileSection)
		{
			if (!Resident.Contains(Pair.Key))
			{
				Mesh->SetMeshSectionVisible(Pair.Value, false);
			}
		}
	}
	TerrainWorld->ReleaseFrame(Frame);
#else
	(void)DeltaTime;
#endif
}
