// Copyright LogiUE. All Rights Reserved.

using UnrealBuildTool;

// LogiMass: the authoritative logistics simulation. Owns Mass Fragments, Tags,
// Chunk/Shared fragments, Traits, Processors, entity-config helpers, and
// simulation-LOD promotion/demotion.
//
// It reaches terrain ONLY through ILogiTerrainService (resolved via LogiCore's
// FLogiServiceRegistry) and therefore takes NO dependency on LogiTerrain or the
// Rust FFI (acceptance criterion: Mass isolated from terrain FFI).
public class LogiMass : ModuleRules
{
	public LogiMass(ReadOnlyTargetRules Target) : base(Target)
	{
		PCHUsage = PCHUsageMode.UseExplicitOrSharedPCHs;

		PublicDependencyModuleNames.AddRange(new string[]
		{
			"Core",
			"CoreUObject",
			"Engine",
			"LogiCore",
			// Mass framework (data + simulation + LOD + representation hooks).
			"MassEntity",
			"MassCommon",
			"MassSpawner",
			"MassSimulation",
			"MassLOD",
			"MassRepresentation",
		});
	}
}
