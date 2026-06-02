// Copyright LogiUE. All Rights Reserved.

using UnrealBuildTool;

// LogiCore: foundational, dependency-light types shared across the project.
// Owns deterministic IDs, the fixed simulation clock, unit types, shared data
// structs, hashing utilities, logging categories, console variables, and the
// cross-boundary service interfaces (e.g. terrain).
//
// HARD RULE (see AGENTS.md / ADR-0002): LogiCore takes NO Mass dependency and
// NO terrain-Rust-FFI dependency. The Mass<->Terrain decoupling is expressed
// here as a pure interface (ILogiTerrainService) that neither side's concrete
// module needs to include.
public class LogiCore : ModuleRules
{
	public LogiCore(ReadOnlyTargetRules Target) : base(Target)
	{
		PCHUsage = PCHUsageMode.UseExplicitOrSharedPCHs;

		PublicDependencyModuleNames.AddRange(new string[]
		{
			"Core",
			"CoreUObject",
			"Engine",
			"DeveloperSettings",
		});
	}
}
