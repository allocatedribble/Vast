// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyRepresentationSubsystem.h"
#include "LogiBevySubsystem.h"
#include "LogiBevyInstancedRenderer.h"
#include "LogiBevyActorPool.h"
#include "LogiBevyAnimationBridge.h"
#include "LogiBevyTelemetry.h"
#include "LogiBevyTypes.h"
#include "LogiBevyDeveloperSettings.h"
#include "LogiBevyAssetRegistry.h"
#include "LogiBevyWorld.h"
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

	// Pure-presentation guard (audit L7): if the sim runtime is invalid (the DLL or
	// the Rust world failed to come up) there is nothing to mirror. Skip renderer
	// init, asset preload, and tick registration entirely so we never spawn render
	// shells (host actor / ISM / pooled actors) that would never be driven. The sim
	// subsystem itself already early-returns in that case, so its tick function is
	// not registered and the prerequisite below would be a no-op anyway.
	ULogiBevySubsystem* Sim = InWorld.GetSubsystem<ULogiBevySubsystem>();
	if (Sim == nullptr || !Sim->IsRuntimeValid())
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
	RepTickFunction.AddPrerequisite(Sim, Sim->GetSimTickFunction());

	// The renderers create world objects: the instanced renderer's host actor + ISM
	// components for the many, and the actor pool's promoted actors for the few.
	if (InstancedRenderer.IsValid())
	{
		InstancedRenderer->Initialize(&InWorld);
	}
	if (ActorPool.IsValid())
	{
		ActorPool->Initialize(&InWorld);
	}

	// Resolve + async-preload the asset registry from project settings (one sync
	// load of the small registry data asset; its referenced meshes load async).
	// Hold the loaded registry in a GC-rooted UPROPERTY (audit H4) so it -- and the
	// FStreamableHandle preload it owns -- survive GC for the subsystem's lifetime;
	// the renderer/pool only borrow a raw pointer to it (they are non-UObject pimpls
	// that cannot root it). Without this root the registry is collected and every
	// bucket silently and permanently falls back to the engine cube.
	const ULogiBevyDeveloperSettings& Settings = *GetDefault<ULogiBevyDeveloperSettings>();
	AssetRegistry = Settings.AssetRegistry.LoadSynchronous();
	if (AssetRegistry != nullptr)
	{
		AssetRegistry->PreloadAll();
		if (InstancedRenderer.IsValid())
		{
			InstancedRenderer->SetAssetRegistry(AssetRegistry);
		}
		if (ActorPool.IsValid())
		{
			ActorPool->SetAssetRegistry(AssetRegistry);
		}
	}
}

void ULogiBevyRepresentationSubsystem::Deinitialize()
{
	if (RepTickFunction.IsTickFunctionRegistered())
	{
		RepTickFunction.UnRegisterTickFunction();
	}
	// Safe whether or not OnWorldBeginPlay ran: the pimpls are created in Initialize
	// and Shutdown() is a no-op on never-initialized (default-constructed) state.
	if (InstancedRenderer.IsValid())
	{
		InstancedRenderer->Shutdown();
	}
	if (ActorPool.IsValid())
	{
		ActorPool->Shutdown();
	}
	// Drop the GC root (audit H4) so the registry + its preload handle can be
	// collected when the world tears down. Null if we early-returned on L7.
	AssetRegistry = nullptr;
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
	TRACE_CPUPROFILER_EVENT_SCOPE(LogiBevy_RepresentationApply);

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
	FLogiBevyWorld* BevyWorld = Sim->GetBevyWorld();
	if (BevyWorld == nullptr)
	{
		return;
	}

	LogiRenderSnapshot Snapshot{};
	LogiError Err{};
	if (BevyWorld->GetSnapshot(Snapshot, Err) != LOGI_OK)
	{
		return;
	}

	// Local-frame origin (P0 #6): both renderers place content at origin + rebased local.
	const FVector FrameOrigin(Snapshot.world_origin_cm[0], Snapshot.world_origin_cm[1], Snapshot.world_origin_cm[2]);
	InstancedRenderer->SetFrameOrigin(FrameOrigin);
	ActorPool->SetFrameOrigin(FrameOrigin);

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

	// Flush all deferred ISM render-state dirties once per touched component (audit H4): despawns and
	// bucket switches hid their instances WITHOUT a per-instance MarkRenderStateDirty.
	InstancedRenderer->FlushDirty();

	Telemetry->RecordFrame(InstancedRenderer->LiveInstanceCount(), ActorPool->AssignedCount(), Sim->GetSimTick());

	// §11 thread-safety policy: representation only READS the published snapshot
	// and then RETIRES it (triple buffer). This runs on the game thread in
	// TG_PostPhysics -- the same (owner) thread as the sim's TG_PrePhysics tick,
	// so the owner-thread guard passes. No render/animation worker thread ever
	// calls into LogiWorld; all mutation goes through ULogiBevySubsystem.
	BevyWorld->ReleaseSnapshot(Snapshot.snapshot_id);
#else
	(void)DeltaTime;
#endif
}
