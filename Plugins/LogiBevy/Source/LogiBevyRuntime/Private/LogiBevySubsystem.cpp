// Copyright LogiUE. All Rights Reserved.

#include "LogiBevySubsystem.h"
#include "Engine/World.h"

#if LOGI_BEVY_HAVE_RUST
#include "logi_bevy_ffi.h"
#endif

void ULogiBevySubsystem::Initialize(FSubsystemCollectionBase& Collection)
{
	Super::Initialize(Collection);

#if LOGI_BEVY_HAVE_RUST
	LogiInitDesc Desc{};
	Desc.abi_version = LOGI_ABI_VERSION;
	Desc.worker_thread_count = 0;
	Desc.fixed_step_hz = 30;
	Desc.deterministic_seed = 0x4C4F4749ull; // 'LOGI'
	Desc.flags = 0;

	LogiError Err{};
	WorldHandle = static_cast<void*>(logi_world_create(&Desc, &Err));
	if (WorldHandle == nullptr)
	{
		UE_LOG(LogLogiBevy, Error, TEXT("logi_world_create failed (code %u): %s"), Err.code, UTF8_TO_TCHAR(Err.message));
		return;
	}

	// Seed a few demo agents through the batched spawn-command path.
	LogiSpawnCommand Commands[4];
	FMemory::Memzero(Commands, sizeof(Commands));
	for (int32 Index = 0; Index < 4; ++Index)
	{
		Commands[Index].kind = 1;
		Commands[Index].position[0] = static_cast<double>(Index) * 200.0;
		Commands[Index].route_id = 1;
		Commands[Index].payload_kg = 10.0;
	}

	LogiError SpawnErr{};
	const uint32 Applied = logi_submit_spawn_commands(static_cast<LogiWorld*>(WorldHandle), Commands, 4, &SpawnErr);
	UE_LOG(LogLogiBevy, Log, TEXT("Bevy world created; %u demo agents submitted."), Applied);
#endif
}

void ULogiBevySubsystem::Deinitialize()
{
#if LOGI_BEVY_HAVE_RUST
	if (WorldHandle != nullptr)
	{
		logi_world_destroy(static_cast<LogiWorld*>(WorldHandle));
		WorldHandle = nullptr;
	}
#endif
	AgentInstances.Reset();
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

void ULogiBevySubsystem::Tick(float DeltaTime)
{
	Super::Tick(DeltaTime);

#if LOGI_BEVY_HAVE_RUST
	if (WorldHandle == nullptr)
	{
		return;
	}

	// Hand the frame delta to Rust; the fixed-step accumulator + clamp live there.
	LogiFrameInput Input{};
	Input.unreal_time_seconds = GetWorld() ? GetWorld()->GetTimeSeconds() : 0.0;
	Input.frame_delta_seconds = DeltaTime;
	Input.max_fixed_steps = 8;
	Input.camera_count = 0;
	Input.cameras = nullptr;
	Input.player_intent_count = 0;
	Input.player_intents = nullptr;

	LogiError TickErr{};
	const LogiTickResult Result = logi_world_tick(static_cast<LogiWorld*>(WorldHandle), &Input, &TickErr);
	if (Result.status != LOGI_OK)
	{
		UE_LOG(LogLogiBevy, Warning, TEXT("logi_world_tick status %u: %s"), Result.status, UTF8_TO_TCHAR(TickErr.message));
		return;
	}
	LastSimTick = static_cast<int64>(Result.sim_tick);

	// Pull the latest batched snapshot (one call, not per-entity).
	LogiRenderSnapshot Snapshot{};
	LogiError SnapErr{};
	if (logi_world_get_render_snapshot(static_cast<LogiWorld*>(WorldHandle), 0 /*latest*/, &Snapshot, &SnapErr) == LOGI_OK)
	{
		AgentInstances.Reset(static_cast<int32>(Snapshot.agent_count));
		for (uint32 Index = 0; Index < Snapshot.agent_count; ++Index)
		{
			const LogiAgentInstance& Src = Snapshot.agents[Index];
			FLogiAgentInstance& Dst = AgentInstances.AddDefaulted_GetRef();
			Dst.EntityIndex = static_cast<int64>(Src.id.index);
			Dst.EntityGeneration = static_cast<int32>(Src.id.generation);
			Dst.EntityKind = static_cast<int32>(Src.id.kind);
			Dst.Location = FVector(Src.x, Src.y, Src.z);
			Dst.SimLOD = Src.sim_lod;
		}
	}
#endif
}

TStatId ULogiBevySubsystem::GetStatId() const
{
	RETURN_QUICK_DECLARE_CYCLE_STAT(ULogiBevySubsystem, STATGROUP_Tickables);
}
