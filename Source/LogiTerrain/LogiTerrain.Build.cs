// Copyright LogiUE. All Rights Reserved.

using UnrealBuildTool;

// LogiTerrain: the runtime side of the terrain system. Owns the Rust FFI, the
// native terrain subsystem, request queues, the tile cache, collision-payload
// handoff, and render-host integration interfaces.
//
// It implements ILogiTerrainService (declared in LogiCore) so consumers never
// see the FFI. NO editor-only baking code lives here (that is LogiTerrainEditor).
public class LogiTerrain : ModuleRules
{
	public LogiTerrain(ReadOnlyTargetRules Target) : base(Target)
	{
		PCHUsage = PCHUsageMode.UseExplicitOrSharedPCHs;

		PublicDependencyModuleNames.AddRange(new string[]
		{
			"Core",
			"CoreUObject",
			"Engine",
			"LogiCore",
		});

		PrivateDependencyModuleNames.AddRange(new string[]
		{
			// Render-host integration + collision payload handoff live behind
			// these; the FFI itself is plain C and needs no UE module.
			"RenderCore",
			"RHI",
		});

		// Flip to 1 (and link the kernel via PublicAdditionalLibraries) once the
		// Rust terrain kernel is integrated. Until then the FFI compiles to stubs.
		PublicDefinitions.Add("LOGI_WITH_RUST_KERNEL=0");
	}
}
