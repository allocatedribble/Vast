// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "MassEntityTypes.h"
#include "Ids/LogiIds.h"
#include "LogiAgentFragments.generated.h"

// Canonical runtime state for a logistics agent lives HERE, as Mass Fragments —
// never on an Actor, UObject, or Blueprint (AGENTS.md / ADR-0001).

/** Identity + assignment of a simulated agent. */
USTRUCT()
struct LOGIMASS_API FLogiAgentFragment : public FMassFragment
{
	GENERATED_BODY()

	UPROPERTY()
	FLogiAgentId AgentId;

	/** Route the agent is currently serving. */
	UPROPERTY()
	FLogiRouteId RouteId;

	/** Terrain tile the agent is currently over (used to query ILogiTerrainService). */
	UPROPERTY()
	FLogiTileId TileId;
};

/** The shipment an agent is carrying. */
USTRUCT()
struct LOGIMASS_API FLogiShipmentFragment : public FMassFragment
{
	GENERATED_BODY()

	UPROPERTY()
	FLogiShipmentId ShipmentId;

	/** Payload mass in kilograms. */
	UPROPERTY()
	double PayloadKg = 0.0;
};
