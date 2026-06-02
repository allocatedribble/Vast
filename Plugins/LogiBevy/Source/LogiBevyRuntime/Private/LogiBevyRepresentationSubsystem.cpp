// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyRepresentationSubsystem.h"
#include "LogiBevySubsystem.h"
#include "LogiBevyInstancedRenderer.h"
#include "LogiBevyActorPool.h"
#include "LogiBevyTelemetry.h"
#include "Engine/World.h"

ULogiBevyRepresentationSubsystem::ULogiBevyRepresentationSubsystem() = default;
ULogiBevyRepresentationSubsystem::~ULogiBevyRepresentationSubsystem() = default;

void ULogiBevyRepresentationSubsystem::Initialize(FSubsystemCollectionBase& Collection)
{
	Super::Initialize(Collection);
	InstancedRenderer = MakeUnique<FLogiBevyInstancedRenderer>();
	ActorPool = MakeUnique<FLogiBevyActorPool>();
	Telemetry = MakeUnique<FLogiBevyTelemetry>();
}

void ULogiBevyRepresentationSubsystem::Deinitialize()
{
	InstancedRenderer.Reset();
	ActorPool.Reset();
	Telemetry.Reset();
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

void ULogiBevyRepresentationSubsystem::Tick(float DeltaTime)
{
	Super::Tick(DeltaTime);

	const UWorld* World = GetWorld();
	if (World == nullptr)
	{
		return;
	}

	const ULogiBevySubsystem* Sim = World->GetSubsystem<ULogiBevySubsystem>();
	if (Sim == nullptr || !Sim->IsRuntimeValid())
	{
		return;
	}

	const TArray<FLogiAgentInstance>& Instances = Sim->GetAgentInstances();
	if (InstancedRenderer.IsValid())
	{
		InstancedRenderer->SyncInstances(Instances);
	}
	if (Telemetry.IsValid())
	{
		Telemetry->RecordFrame(Instances.Num(), Sim->GetSimTick());
	}
}

TStatId ULogiBevyRepresentationSubsystem::GetStatId() const
{
	RETURN_QUICK_DECLARE_CYCLE_STAT(ULogiBevyRepresentationSubsystem, STATGROUP_Tickables);
}
