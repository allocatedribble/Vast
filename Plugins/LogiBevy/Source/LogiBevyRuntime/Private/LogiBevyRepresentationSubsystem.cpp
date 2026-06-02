// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyRepresentationSubsystem.h"
#include "LogiBevySubsystem.h"
#include "LogiBevyInstancedRenderer.h"
#include "LogiBevyActorPool.h"
#include "LogiBevyTelemetry.h"
#include "Engine/World.h"
#include "Engine/Level.h"

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

ULogiBevyRepresentationSubsystem::ULogiBevyRepresentationSubsystem() = default;
ULogiBevyRepresentationSubsystem::~ULogiBevyRepresentationSubsystem() = default;

void ULogiBevyRepresentationSubsystem::Initialize(FSubsystemCollectionBase& Collection)
{
	Super::Initialize(Collection);
	InstancedRenderer = MakeUnique<FLogiBevyInstancedRenderer>();
	ActorPool = MakeUnique<FLogiBevyActorPool>();
	Telemetry = MakeUnique<FLogiBevyTelemetry>();
}

void ULogiBevyRepresentationSubsystem::OnWorldBeginPlay(UWorld& InWorld)
{
	Super::OnWorldBeginPlay(InWorld);

	if (InWorld.PersistentLevel == nullptr)
	{
		return;
	}

	// Register the representation tick in TG_PostPhysics.
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
}

void ULogiBevyRepresentationSubsystem::Deinitialize()
{
	if (RepTickFunction.IsTickFunctionRegistered())
	{
		RepTickFunction.UnRegisterTickFunction();
	}
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

void ULogiBevyRepresentationSubsystem::TickRepresentation(float /*DeltaTime*/)
{
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
