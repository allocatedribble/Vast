// Copyright LogiUE. All Rights Reserved.

using UnrealBuildTool;

// LogiBevyEditor: authoring / import / editor-only tooling for the Bevy bridge
// (spawn-recipe authoring, asset-handle import, debug visualizers). Editor-type
// module — never loaded in packaged builds.
public class LogiBevyEditor : ModuleRules
{
	public LogiBevyEditor(ReadOnlyTargetRules Target) : base(Target)
	{
		PCHUsage = PCHUsageMode.UseExplicitOrSharedPCHs;

		PublicDependencyModuleNames.AddRange(new string[]
		{
			"Core",
			"CoreUObject",
			"Engine",
		});

		PrivateDependencyModuleNames.AddRange(new string[]
		{
			"UnrealEd",
			"Slate",
			"SlateCore",
			"ToolMenus",
			"EditorSubsystem",        // UEditorSubsystem (Bevy Entity Outliner)
			"WorkspaceMenuStructure", // Window-menu group for the nomad tab
			"PropertyEditor",         // SDetailsView for the selection proxy
			"InputCore",
			"Projects",
			"LogiBevyRuntime",
		});
	}
}
