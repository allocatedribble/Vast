// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "MassEntityTypes.h"
#include "LogiSimTags.generated.h"

// Zero-size archetype discriminators. They carry no data; they partition
// entities so Processors can select them cheaply via EntityQueries.

/** Entity participates in the authoritative fixed-step simulation. */
USTRUCT()
struct LOGIMASS_API FLogiSimulatedTag : public FMassTag
{
	GENERATED_BODY()
};

/** Entity currently won a high-significance budget slot (ADR-0003). */
USTRUCT()
struct LOGIMASS_API FLogiHighSignificanceTag : public FMassTag
{
	GENERATED_BODY()
};
