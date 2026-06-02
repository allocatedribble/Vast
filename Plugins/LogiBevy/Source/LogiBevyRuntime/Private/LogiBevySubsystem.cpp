// Copyright LogiUE. All Rights Reserved.

#include "LogiBevySubsystem.h"

#if LOGI_BEVY_HAVE_RUST
#include "logi_bevy_ffi.h"
#endif

void ULogiBevySubsystem::Initialize(FSubsystemCollectionBase& Collection)
{
	Super::Initialize(Collection);

#if LOGI_BEVY_HAVE_RUST
	// Seed 'LOGI' for a deterministic start. Spawn a few demo agents so the
	// snapshot pipeline has something to surface.
	RuntimeHandle = static_cast<void*>(logi_runtime_create(0x4C4F4749ull));
	if (RuntimeHandle != nullptr)
	{
		for (int32 Index = 0; Index < 4; ++Index)
		{
			LogiAgentSpawnDesc Desc{};
			Desc.agent_id = static_cast<uint64>(Index + 1);
			Desc.route_id = 1;
			Desc.tile_id = 1;
			Desc.position = Vec3{ static_cast<float>(Index) * 200.0f, 0.0f, 0.0f };
			Desc.payload_kg = 10.0;
			logi_runtime_spawn_agent(static_cast<LogiRuntimeHandle>(RuntimeHandle), Desc);
		}
		AdvanceAndExtract(1);
		UE_LOG(LogLogiBevy, Log, TEXT("Bevy runtime created; %d demo agents in snapshot."), AgentInstances.Num());
	}
	else
	{
		UE_LOG(LogLogiBevy, Error, TEXT("logi_runtime_create returned null."));
	}
#endif
}

void ULogiBevySubsystem::Deinitialize()
{
#if LOGI_BEVY_HAVE_RUST
	if (RuntimeHandle != nullptr)
	{
		logi_runtime_destroy(static_cast<LogiRuntimeHandle>(RuntimeHandle));
		RuntimeHandle = nullptr;
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
	// Real game worlds only (incl. PIE) — not editor preview/inactive worlds.
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
	if (RuntimeHandle == nullptr)
	{
		return;
	}

	Accumulator += DeltaTime;
	int32 Steps = 0;
	// Fixed-step catch-up, clamped so a hitch can't spiral.
	while (Accumulator >= FixedStepSeconds && Steps < 8)
	{
		Accumulator -= FixedStepSeconds;
		++Steps;
	}
	if (Steps > 0)
	{
		AdvanceAndExtract(Steps);
	}
#endif
}

void ULogiBevySubsystem::AdvanceAndExtract(int32 Steps)
{
#if LOGI_BEVY_HAVE_RUST
	if (RuntimeHandle == nullptr)
	{
		return;
	}

	logi_runtime_tick(static_cast<LogiRuntimeHandle>(RuntimeHandle), static_cast<uint32>(Steps));

	LogiInstanceView View{};
	if (logi_runtime_extract(static_cast<LogiRuntimeHandle>(RuntimeHandle), &View) == LogiStatus::Ok)
	{
		AgentInstances.Reset(static_cast<int32>(View.len));
		for (uintptr_t Index = 0; Index < View.len; ++Index)
		{
			const AgentInstance& Src = View.ptr[Index];
			FLogiAgentInstance& Dst = AgentInstances.AddDefaulted_GetRef();
			Dst.Entity = static_cast<int64>(Src.entity);
			Dst.Location = FVector(Src.x, Src.y, Src.z);
			Dst.AgentId = static_cast<int64>(Src.agent_id);
			Dst.SimLOD = Src.sim_lod;
		}
	}
#else
	(void)Steps;
#endif
}

int64 ULogiBevySubsystem::GetSimTick() const
{
#if LOGI_BEVY_HAVE_RUST
	if (RuntimeHandle != nullptr)
	{
		return static_cast<int64>(logi_runtime_tick_count(static_cast<LogiRuntimeHandle>(RuntimeHandle)));
	}
#endif
	return 0;
}

TStatId ULogiBevySubsystem::GetStatId() const
{
	RETURN_QUICK_DECLARE_CYCLE_STAT(ULogiBevySubsystem, STATGROUP_Tickables);
}
