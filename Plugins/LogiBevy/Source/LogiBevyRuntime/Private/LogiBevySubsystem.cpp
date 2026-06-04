// Copyright LogiUE. All Rights Reserved.

#include "LogiBevySubsystem.h"
#include "LogiBevyDeveloperSettings.h"
#include "LogiBevyWorld.h"
#include "Engine/World.h"
#include "Engine/Level.h"
#include "Engine/GameViewportClient.h"
#include "GameFramework/PlayerController.h"
#include "Camera/PlayerCameraManager.h"

#if LOGI_BEVY_HAVE_RUST
#include "logi_bevy_ffi.h"
#endif
#if WITH_EDITOR && LOGI_BEVY_HAVE_EDITOR_INSPECTOR
#include "logi_bevy_editor_ffi.h"
#endif

void FLogiBevySimTickFunction::ExecuteTick(float DeltaTime, ELevelTick TickType, ENamedThreads::Type /*CurrentThread*/, const FGraphEventRef& /*MyCompletionGraphEvent*/)
{
	if (Target != nullptr && TickType != LEVELTICK_ViewportsOnly)
	{
		Target->TickSimulation(DeltaTime);
	}
}

FString FLogiBevySimTickFunction::DiagnosticMessage()
{
	return TEXT("FLogiBevySimTickFunction (LogiBevy PrePhysics sim)");
}

void ULogiBevySubsystem::Initialize(FSubsystemCollectionBase& Collection)
{
	Super::Initialize(Collection);

#if LOGI_BEVY_HAVE_RUST
	const ULogiBevyDeveloperSettings& Settings = *GetDefault<ULogiBevyDeveloperSettings>();

	LogiInitDesc Desc{};
	Desc.abi_version = LOGI_ABI_VERSION;
	Desc.worker_thread_count = static_cast<uint32>(FMath::Max(0, Settings.WorkerThreadCount));
	Desc.fixed_step_hz = static_cast<uint32>(FMath::Clamp(Settings.FixedStepHz, 1, 240));
	Desc.deterministic_seed = static_cast<uint64>(Settings.DeterministicSeed);
	Desc.flags = Settings.bForceSingleThreaded ? LOGI_FLAG_FORCE_SINGLE_THREADED : 0u;
	Desc.high_res_actor_budget = static_cast<uint32>(FMath::Max(0, Settings.HighResActorBudget));
	Desc.low_res_actor_budget = static_cast<uint32>(FMath::Max(0, Settings.LowResActorBudget));
	Desc.instanced_mesh_budget = static_cast<uint32>(FMath::Max(0, Settings.InstancedMeshBudget));

	BevyWorld = MakePimpl<FLogiBevyWorld>();
	LogiError Err{};
	if (!BevyWorld->Create(Desc, Err))
	{
		UE_LOG(LogLogiBevy, Error, TEXT("logi_world_create failed (code %u): %s"), Err.code, UTF8_TO_TCHAR(Err.message));
		return;
	}

	const int32 NumDemo = FMath::Max(0, Settings.DemoAgentCount);
	if (NumDemo > 0)
	{
		TArray<LogiSpawnCommand> Commands;
		Commands.SetNumZeroed(NumDemo);
		for (int32 Index = 0; Index < NumDemo; ++Index)
		{
			Commands[Index].kind = 1;
			Commands[Index].position[0] = static_cast<double>(Index) * 200.0;
			Commands[Index].route_id = 1;
			Commands[Index].payload_kg = 10.0;
		}
		LogiError SpawnErr{};
		const uint32 Applied = BevyWorld->SubmitSpawns(Commands.GetData(), static_cast<uint32>(NumDemo), SpawnErr);
		UE_LOG(LogLogiBevy, Log, TEXT("Bevy world created; %u demo agents submitted (budgets %u/%u/%u)."),
			Applied, Desc.high_res_actor_budget, Desc.low_res_actor_budget, Desc.instanced_mesh_budget);
	}
	else
	{
		UE_LOG(LogLogiBevy, Log, TEXT("Bevy world created (no demo agents)."));
	}
#endif
}

void ULogiBevySubsystem::OnWorldBeginPlay(UWorld& InWorld)
{
	Super::OnWorldBeginPlay(InWorld);

	if (!IsRuntimeValid() || InWorld.PersistentLevel == nullptr)
	{
		return;
	}

	SimTickFunction.bCanEverTick = true;
	SimTickFunction.bStartWithTickEnabled = true;
	SimTickFunction.bTickEvenWhenPaused = false;
	SimTickFunction.TickGroup = TG_PrePhysics;
	SimTickFunction.Target = this;
	SimTickFunction.RegisterTickFunction(InWorld.PersistentLevel);
}

void ULogiBevySubsystem::Deinitialize()
{
	if (SimTickFunction.IsTickFunctionRegistered())
	{
		SimTickFunction.UnRegisterTickFunction();
	}

#if LOGI_BEVY_HAVE_RUST
	if (BevyWorld.IsValid())
	{
		BevyWorld->Destroy();
	}
#endif
	Super::Deinitialize();
}

bool ULogiBevySubsystem::ShouldCreateSubsystem(UObject* Outer) const
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

bool ULogiBevySubsystem::IsRuntimeValid() const
{
	return BevyWorld.IsValid() && BevyWorld->IsValid();
}

void ULogiBevySubsystem::TickSimulation(float DeltaTime)
{
#if LOGI_BEVY_HAVE_RUST
	if (!BevyWorld.IsValid() || !BevyWorld->IsValid())
	{
		return;
	}

	TRACE_CPUPROFILER_EVENT_SCOPE(LogiBevy_SimTick);

	LogiFrameInput Input{};
	Input.unreal_time_seconds = GetWorld() ? GetWorld()->GetTimeSeconds() : 0.0;
	Input.frame_delta_seconds = DeltaTime;
	Input.max_fixed_steps = 8;

	// Feed the player camera so significance (and thus actor promotion) follows the view.
	LogiCameraView Camera{};
	uint32 CameraCount = 0;
	if (UWorld* W = GetWorld())
	{
		if (APlayerController* PC = W->GetFirstPlayerController())
		{
			FVector ViewLoc;
			FRotator ViewRot;
			PC->GetPlayerViewPoint(ViewLoc, ViewRot);
			const FVector Forward = ViewRot.Vector();
			Camera.position[0] = ViewLoc.X;
			Camera.position[1] = ViewLoc.Y;
			Camera.position[2] = ViewLoc.Z;
			Camera.forward[0] = Forward.X;
			Camera.forward[1] = Forward.Y;
			Camera.forward[2] = Forward.Z;

			// FOV from the live camera manager (falls back to 90 if unavailable, e.g.
			// before the view target is set). GetFOVAngle() is horizontal FOV in degrees.
			float FovDegrees = 90.0f;
			if (const APlayerCameraManager* CamMgr = PC->PlayerCameraManager)
			{
				const float ActiveFov = CamMgr->GetFOVAngle();
				if (ActiveFov > 0.0f)
				{
					FovDegrees = ActiveFov;
				}
			}
			Camera.fov_degrees = FovDegrees;

			// Screen height from the game viewport (drives screen-space significance);
			// fall back to 1080 when there is no viewport (dedicated server / headless).
			float ScreenHeightPx = 1080.0f;
			if (UGameViewportClient* Viewport = W->GetGameViewport())
			{
				FVector2D ViewportSize = FVector2D::ZeroVector;
				Viewport->GetViewportSize(ViewportSize);
				if (ViewportSize.Y > 0.0)
				{
					ScreenHeightPx = static_cast<float>(ViewportSize.Y);
				}
			}
			Camera.screen_height_px = ScreenHeightPx;

			CameraCount = 1;
		}
	}
	Input.camera_count = CameraCount;
	Input.cameras = (CameraCount > 0) ? &Camera : nullptr;
	Input.player_intent_count = 0;
	Input.player_intents = nullptr;

	LogiError TickErr{};
	const LogiTickResult Result = BevyWorld->Tick(Input, TickErr);
	if (Result.status != LOGI_OK)
	{
		UE_LOG(LogLogiBevy, Warning, TEXT("logi_world_tick status %u: %s"), Result.status, UTF8_TO_TCHAR(TickErr.message));
		return;
	}
	LastSimTick = static_cast<int64>(Result.sim_tick);
#else
	(void)DeltaTime;
#endif
}

#if WITH_EDITOR
void ULogiBevySubsystem::EditorQueryEntities(int32 Offset, int32 Limit, TArray<FLogiBevyEntityRow>& OutRows, int32& OutTotal) const
{
	OutRows.Reset();
	OutTotal = 0;
#if LOGI_BEVY_HAVE_RUST && LOGI_BEVY_HAVE_EDITOR_INSPECTOR
	if (!BevyWorld.IsValid() || !BevyWorld->IsValid())
	{
		return;
	}
	LogiEntityQueryDesc Query{};
	Query.offset = static_cast<uint32>(FMath::Max(0, Offset));
	Query.limit = static_cast<uint32>(FMath::Max(0, Limit));
	LogiEntityInspectorPacket Packet{};
	LogiError Err{};
	if (BevyWorld->EditorQuery(Query, Packet, Err) != LOGI_OK)
	{
		return;
	}
	OutTotal = static_cast<int32>(Packet.total_count);
	OutRows.Reserve(static_cast<int32>(Packet.entity_count));
	for (uint32 Index = 0; Index < Packet.entity_count; ++Index)
	{
		const LogiEntitySummary& S = Packet.entities[Index];
		FLogiBevyEntityRow Row;
		Row.Index = S.id.index;
		Row.Generation = S.id.generation;
		Row.Shard = S.id.shard;
		Row.Kind = S.id.kind;
		Row.Position = FVector(S.position_cm[0], S.position_cm[1], S.position_cm[2]);
		Row.Significance = S.significance;
		Row.Tier = S.representation_tier;
		Row.bPinned = S.pinned != 0;
		OutRows.Add(Row);
	}
#else
	(void)Offset;
	(void)Limit;
#endif
}

void ULogiBevySubsystem::EditorSetPin(const FLogiBevyEntityRow& Row, bool bPinned)
{
#if LOGI_BEVY_HAVE_RUST && LOGI_BEVY_HAVE_EDITOR_INSPECTOR
	if (!BevyWorld.IsValid() || !BevyWorld->IsValid())
	{
		return;
	}
	LogiEntityId Id{};
	Id.index = Row.Index;
	Id.generation = Row.Generation;
	Id.shard = Row.Shard;
	Id.kind = Row.Kind;
	LogiError Err{};
	BevyWorld->EditorSetPin(Id, bPinned ? 1 : 0, Err);
#else
	(void)Row;
	(void)bPinned;
#endif
}
#endif // WITH_EDITOR
