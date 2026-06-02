// Copyright LogiUE. All Rights Reserved.

#include "LogiTerrainSubsystem.h"
#include "LogiTerrainWorld.h"
#include "LogiBevyTypes.h"
#include "Engine/World.h"
#include "GameFramework/PlayerController.h"
#include "DrawDebugHelpers.h"

#if LOGI_BEVY_HAVE_TERRAIN
#include "logi_terrain_ffi.h"
#endif

namespace
{
	// The terrain kernel works in metres; Unreal in centimetres.
	constexpr double LogiTerrainToUnreal = 100.0;
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

void ULogiTerrainSubsystem::Tick(float DeltaTime)
{
#if LOGI_BEVY_HAVE_TERRAIN
	UWorld* World = GetWorld();
	if (World == nullptr || !TerrainWorld.IsValid() || !TerrainWorld->IsValid())
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
		const LogiDVec3 Origin = Packet.local_frame.origin;
		for (uint32 i = 0; i < Packet.terrain_patch_count; ++i)
		{
			const LogiTerrainPatchDraw& P = Packet.terrain_patches[i];
			// world (terrain) = origin + render-local centre; map Y-up -> Z-up; metres -> cm.
			const double WX = (Origin.x + static_cast<double>(P.bounds.center.x)) * LogiTerrainToUnreal;
			const double WY = (Origin.z + static_cast<double>(P.bounds.center.z)) * LogiTerrainToUnreal;
			const double WZ = (Origin.y + static_cast<double>(P.bounds.center.y)) * LogiTerrainToUnreal;
			const float Radius = P.bounds.radius * static_cast<float>(LogiTerrainToUnreal);
			// Colour by LOD: coarse = red ... fine = green (parent fallback shows as red blobs).
			const uint8 Green = static_cast<uint8>(FMath::Clamp(static_cast<int32>(P.lod) * 28, 0, 255));
			const FColor Colour(static_cast<uint8>(255 - Green), Green, 40);
			DrawDebugSphere(World, FVector(WX, WY, WZ), Radius, 8, Colour, /*bPersistent=*/false, -1.0f, 0, 1.0f);
		}
	}
	TerrainWorld->ReleaseFrame(Frame);
#else
	(void)DeltaTime;
#endif
}
