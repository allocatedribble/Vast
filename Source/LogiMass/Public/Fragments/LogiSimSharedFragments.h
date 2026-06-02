// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "MassEntityTypes.h"
#include "Time/LogiSimClock.h"
#include "LogiSimSharedFragments.generated.h"

/** Per-chunk simulation-LOD bookkeeping (cheap scratch shared by a chunk of entities). */
USTRUCT()
struct LOGIMASS_API FLogiSimLODChunkFragment : public FMassChunkFragment
{
	GENERATED_BODY()

	/** 0 = High, 1 = Medium, 2 = Low, 3 = Off. Drives update cadence/model fidelity. */
	UPROPERTY()
	uint8 SimLOD = 0;
};

/** Immutable, shared config for the deterministic sim step (one instance, many entities). */
USTRUCT()
struct LOGIMASS_API FLogiSimConfigSharedFragment : public FMassSharedFragment
{
	GENERATED_BODY()

	UPROPERTY()
	FLogiFixedStep FixedStep;
};
