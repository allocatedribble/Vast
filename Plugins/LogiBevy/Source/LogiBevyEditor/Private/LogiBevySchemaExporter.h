// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"

class ULogiBevyScenarioAsset;

/**
 * Serializes editor-authored scenarios/recipes into a Rust-readable schema (JSON)
 * at cook/save time (§9). Runtime spawn commands reference recipe IDs, not
 * Blueprint classes; the Rust side (serde) reads this schema.
 */
class FLogiBevySchemaExporter
{
public:
	/** Write `Scenario` as JSON to `OutPath`. Returns true on success. */
	static bool ExportScenario(const ULogiBevyScenarioAsset* Scenario, const FString& OutPath);
};
