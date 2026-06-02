// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyRepresentationSubsystem.h"
#include "LogiBevySubsystem.h"
#include "LogiBevyInstancedRenderer.h"
#include "LogiBevyActorPool.h"
#include "LogiBevyAnimationBridge.h"
#include "LogiBevyTelemetry.h"
#include "LogiBevyTypes.h"
#include "Engine/World.h"
#include "Engine/Level.h"

#if LOGI_BEVY_HAVE_RUST
#include "logi_bevy_ffi.h"
#endif

void FLogiBevyRepresentationTickFunction::ExecuteTick(float DeltaTime, ELevelTick TickType, ENamedThreads::Type /*CurrentThread*/, const FGraphEventRef& /*MyCompletionGraphEvent*/)
{
	if (Target != nullptr && TickType != LEVELTICK_ViewportsOnly)
	{
		Target->TickRepresentation(DeltaTime);
	}
}

FString FLogiBevyRepresentationTickFunction::DiagnosticMessage()
{
	return TEXT("FLogiBevyRepresentationTickFunction (LogiBevy PostPhysics representation)");
}

void ULogiBevyRepresentationSubsystem::Initialize(FSubsystemCollectionBase& Collection)
{
	Super::Initialize(Collection);
	InstancedRenderer = MakePimpl<FLogiInstancedRenderer>();
	ActorPool = MakePimpl<FLogiActorPool>();
	AnimationBridge = MakePimpl<FLogiAnimationBridge>();
	Telemetry = MakePimpl<FLogiBevyTelemetry>();
}

void ULogiBevyRepresentationSubsystem::OnWorldBeginPlay(UWorld& InWorld)
{
	Super::OnWorldBeginPlay(InWorld);

	if (InWorld.PersistentLevel == nullptr)
	{
		return;
	}

	RepTickFunction.bCanEverTick = true;
	RepTickFunction.bStartWithTickEnabled = true;
	RepTickFunction.bTickEvenWhenPaused = false;
	RepTickFunction.TickGroup = TG_PostPhysics;
	RepTickFunction.Target = this;
	RepTickFunction.RegisterTickFunction(InWorld.PersistentLevel);

	// Explicit prerequisite: always run after the sim's PrePhysics tick.
	if (ULogiBevySubsystem* Sim = InWorld.GetSubsystem<ULogiBevySubsystem>())
	{
		RepTickFunction.AddPrerequisite(Sim, Sim->GetSimTickFunction());
	}

	// The actor pool spawns real actors into this world for promoted entities.
	if (ActorPool.IsValid())
	{
		ActorPool->Initialize(&InWorld);
	}
}

void ULogiBevyRepresentationSubsystem::Deinitialize()
{
	if (RepTickFunction.IsTickFunctionRegistered())
	{
		RepTickFunction.UnRegisterTickFunction();
	}
	if (ActorPool.IsValid())
	{
		ActorPool->Shutdown();
	}
	Super::Deinitialize();
}

bool ULogiBevyRepresentationSubsystem::ShouldCreateSubsystem(UObject* Outer) const
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

void ULogiBevyRepresentationSubsystem::TickRepresentation(float DeltaTime)
{
	UWorld* World = GetWorld();
	if (World == nullptr)
	{
		return;
	}

	ULogiBevySubsystem* Sim = World->GetSubsystem<ULogiBevySubsystem>();
	if (Sim == nullptr || !Sim->IsRuntimeValid())
	{
		return;
	}

#if LOGI_BEVY_HAVE_RUST
	LogiRenderSnapshot Snapshot{};
	LogiError Err{};
	if (logi_world_get_render_snapshot(static_cast<LogiWorld*>(Sim->GetWorldHandle()), 0 /*latest*/, &Snapshot, &Err) != LOGI_OK)
	{
		return;
	}

	// Despawns: drop the entity from every representation.
	for (uint32 D = 0; D < Snapshot.despawn_count; ++D)
	{
		const uint64 Entity = Snapshot.despawns[D].index;
		InstancedRenderer->ApplyDespawn(Entity);
		ActorPool->ReleaseEntity(Entity);
		AnimationBridge->ReleaseEntity(Entity);
	}

	// ISM batches: move entities out of the actor representation, then apply.
	for (uint32 B = 0; B < Snapshot.static_batch_count; ++B)
	{
		const LogiStaticBatch& Batch = Snapshot.static_batches[B];
		for (uint32 I = 0; I < Batch.instance_count; ++I)
		{
			const uint64 Entity = Batch.instances[I].entity.index;
			ActorPool->ReleaseEntity(Entity);
			AnimationBridge->ReleaseEntity(Entity);
		}
		InstancedRenderer->ApplyBatch(Batch);
	}

	// Actor updates: move entities out of ISM, then apply actor + animation.
	for (uint32 A = 0; A < Snapshot.actor_update_count; ++A)
	{
		const LogiActorUpdate& Update = Snapshot.actor_updates[A];
		InstancedRenderer->ApplyDespawn(Update.entity.index);
		ActorPool->ApplyActorUpdate(Update);
		AnimationBridge->ApplyActorUpdate(Update);
	}

	Telemetry->RecordFrame(InstancedRenderer->LiveInstanceCount(), ActorPool->AssignedCount(), Sim->GetSimTick());

	// §11 thread-safety policy: representation only READS the published snapshot
	// and then RETIRES it (triple buffer). This runs on the game thread in
	// TG_PostPhysics -- the same (owner) thread as the sim's TG_PrePhysics tick,
	// so the owner-thread guard passes. No render/animation worker thread ever
	// calls into LogiWorld; all mutation goes through ULogiBevySubsystem.
	logi_buffer_release(static_cast<LogiWorld*>(Sim->GetWorldHandle()), Snapshot.snapshot_id);
#else
	(void)DeltaTime;
#endif
}
