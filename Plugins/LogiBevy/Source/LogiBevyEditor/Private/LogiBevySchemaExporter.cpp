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

	// NOTE (audit): this is a WRITE-ONLY authoring export — no Rust deserializer reads it yet, and these
	// JSON keys (`high_res_actors`, `terrain_world_config`, …) do NOT match the Rust `WorldConfig` /
	// `LogiInitDesc` field names (`high_res_actor_budget`, …). When a Rust scenario loader is added, make it
	// the single source of truth (serialize FROM the Rust types) and keep these keys in sync — otherwise
	// this silently drifts from whatever finally consumes it.
	const FString Json = FString::Printf(
		TEXT("{\n")
		TEXT("  \"seed\": %lld,\n")
		TEXT("  \"fixed_step_hz\": %d,\n")
		TEXT("  \"terrain_world_config\": %lld,\n")
		TEXT("  \"budget\": { \"high_res_actors\": %d, \"low_res_actors\": %d, \"instanced_meshes\": %d },\n")
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
