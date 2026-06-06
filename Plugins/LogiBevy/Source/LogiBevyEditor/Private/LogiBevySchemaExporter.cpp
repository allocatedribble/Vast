// Copyright LogiUE. All Rights Reserved.

#include "LogiBevySchemaExporter.h"
#include "LogiBevyScenarioAsset.h"
#include "LogiBevyEditor.h"
#include "Misc/FileHelper.h"

bool FLogiBevySchemaExporter::ExportScenario(const ULogiBevyScenarioAsset* Scenario, const FString& OutPath)
{
	if (Scenario == nullptr)
	{
		return false;
	}

	TArray<FString> SpawnLines;
	SpawnLines.Reserve(Scenario->InitialSpawns.Num());
	for (const FLogiBevyInitialSpawn& Spawn : Scenario->InitialSpawns)
	{
		SpawnLines.Add(FString::Printf(
			TEXT("    { \"recipe_id\": %lld, \"position\": [%f, %f, %f], \"count\": %d }"),
			Spawn.RecipeId, Spawn.Position.X, Spawn.Position.Y, Spawn.Position.Z, Spawn.Count));
	}

	// NOTE (audit M4/M29): this is a WRITE-ONLY authoring export — no Rust deserializer reads it yet. The
	// keys below are aligned to the `LogiInitDesc` `#[repr(C)]` field names (deterministic_seed, fixed_step_hz,
	// high_res_actor_budget, low_res_actor_budget, instanced_mesh_budget) so a future serde loader binds
	// without silent drift. (`terrain_world_config` is scenario authoring data with no LogiInitDesc field; it
	// is resolved by the terrain kernel.) When a Rust loader lands, make the Rust types the single source of
	// truth (serialize FROM them) rather than maintaining these key strings by hand.
	const FString Json = FString::Printf(
		TEXT("{\n")
		TEXT("  \"deterministic_seed\": %lld,\n")
		TEXT("  \"fixed_step_hz\": %d,\n")
		TEXT("  \"terrain_world_config\": %lld,\n")
		TEXT("  \"high_res_actor_budget\": %d,\n")
		TEXT("  \"low_res_actor_budget\": %d,\n")
		TEXT("  \"instanced_mesh_budget\": %d,\n")
		TEXT("  \"initial_spawns\": [\n%s\n  ]\n")
		TEXT("}\n"),
		Scenario->Seed, Scenario->FixedStepHz, Scenario->TerrainWorldConfig,
		Scenario->HighResActorBudget, Scenario->LowResActorBudget, Scenario->InstancedMeshBudget,
		*FString::Join(SpawnLines, TEXT(",\n")));

	if (FFileHelper::SaveStringToFile(Json, *OutPath))
	{
		UE_LOG(LogLogiBevyEditor, Log, TEXT("Exported scenario schema -> %s"), *OutPath);
		return true;
	}

	UE_LOG(LogLogiBevyEditor, Error, TEXT("Failed to write scenario schema -> %s"), *OutPath);
	return false;
}
