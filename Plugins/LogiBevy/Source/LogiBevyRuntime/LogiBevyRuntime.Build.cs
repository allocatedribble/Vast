// Copyright LogiUE. All Rights Reserved.

using UnrealBuildTool;
using System;
using System.IO;
using System.Diagnostics;

// The C++ side of the Bevy bridge (ADR-0005). This module is a SHELL: it owns the
// opaque Rust runtime handle, pulls per-frame snapshots, and drives representation.
// It never owns gameplay truth.
//
// Build responsibility: build OR locate the Rust `logi_ffi` artifact, link its
// import lib, delay-load the DLL, and stage the DLL next to the module binary.
public class LogiBevyRuntime : ModuleRules
{
	public LogiBevyRuntime(ReadOnlyTargetRules Target) : base(Target)
	{
		PCHUsage = PCHUsageMode.UseExplicitOrSharedPCHs;

		PublicDependencyModuleNames.AddRange(new string[]
		{
			"Core",
			"CoreUObject",
			"Engine",
			"Projects",                // IPluginManager (locating the staged DLL at runtime)
			"DeveloperSettings",       // ULogiBevyDeveloperSettings (Project Settings)
			"ProceduralMeshComponent", // actorless terrain debug-lane mesh (ADR-0007 / D)
		});

		if (Target.bBuildEditor)
		{
			// Editor-only: feed the level-viewport camera so terrain streams in-editor (D).
			PrivateDependencyModuleNames.Add("UnrealEd");
		}

		// Plugins/LogiBevy/Source/LogiBevyRuntime -> .../Plugins/LogiBevy -> <ProjectRoot>
		string PluginDir = Path.GetFullPath(Path.Combine(ModuleDirectory, "..", ".."));
		string ProjectRoot = Path.GetFullPath(Path.Combine(PluginDir, "..", ".."));
		string RustDir = Path.Combine(ProjectRoot, "Rust");
		string FfiInclude = Path.Combine(RustDir, "crates", "logi_ffi", "include");
		string RustOutDir = Path.Combine(RustDir, "target", "release");

		PublicIncludePaths.Add(FfiInclude);

		if (Target.Platform == UnrealTargetPlatform.Win64)
		{
			// Editor builds get the editor_inspector feature (logi_editor_* exports);
			// Game/Shipping stay clean (zero editor cost, ADR-0006).
			bool bWithEditorInspector = Target.Type == TargetType.Editor;
			TryBuildCargo(RustDir, "logi_ffi", bWithEditorInspector ? " --features editor_inspector" : "");
			TryBuildCargo(RustDir, "logi_terrain_ffi", "");

			string ImportLib = Path.Combine(RustOutDir, "logi_ffi.dll.lib");
			string Dll = Path.Combine(RustOutDir, "logi_ffi.dll");

			if (!File.Exists(ImportLib) || !File.Exists(Dll))
			{
				throw new BuildException(
					"LogiBevy: Rust artifact not found in {0}. Build it with `cargo build --release -p logi_ffi` from /Rust.",
					RustOutDir);
			}

			// Link the import lib; delay-load the DLL so it resolves at first use
			// (the module's StartupModule loads it explicitly from the staged path).
			PublicAdditionalLibraries.Add(ImportLib);
			PublicDelayLoadDLLs.Add("logi_ffi.dll");
			RuntimeDependencies.Add("$(BinaryOutputDir)/logi_ffi.dll", Dll);

			PublicDefinitions.Add("LOGI_BEVY_HAVE_RUST=1");
			PublicDefinitions.Add("LOGI_BEVY_HAVE_EDITOR_INSPECTOR=" + (bWithEditorInspector ? "1" : "0"));

			// Terrain kernel DLL (ADR-0007): a second cdylib, logi_terrain.dll.
			PublicIncludePaths.Add(Path.Combine(RustDir, "crates", "logi_terrain_ffi", "include"));
			string TerrainLib = Path.Combine(RustOutDir, "logi_terrain.dll.lib");
			string TerrainDll = Path.Combine(RustOutDir, "logi_terrain.dll");
			if (File.Exists(TerrainLib) && File.Exists(TerrainDll))
			{
				PublicAdditionalLibraries.Add(TerrainLib);
				PublicDelayLoadDLLs.Add("logi_terrain.dll");
				RuntimeDependencies.Add("$(BinaryOutputDir)/logi_terrain.dll", TerrainDll);
				PublicDefinitions.Add("LOGI_BEVY_HAVE_TERRAIN=1");
			}
			else
			{
				PublicDefinitions.Add("LOGI_BEVY_HAVE_TERRAIN=0");
			}
		}
		else
		{
			// Other platforms: the bridge compiles to no-ops until the Rust target is added.
			PublicDefinitions.Add("LOGI_BEVY_HAVE_RUST=0");
			PublicDefinitions.Add("LOGI_BEVY_HAVE_EDITOR_INSPECTOR=0");
			PublicDefinitions.Add("LOGI_BEVY_HAVE_TERRAIN=0");
		}
	}

	// Best-effort: keep the linked artifact fresh. If cargo is unavailable we fall
	// back to a prebuilt artifact (validated by the File.Exists checks above).
	private void TryBuildCargo(string RustDir, string Package, string ExtraArgs)
	{
		try
		{
			var Psi = new ProcessStartInfo
			{
				FileName = "cargo",
				Arguments = "build --release -p " + Package + ExtraArgs,
				WorkingDirectory = RustDir,
				UseShellExecute = false,
				CreateNoWindow = true,
				RedirectStandardOutput = true,
				RedirectStandardError = true,
			};

			using (var Proc = Process.Start(Psi))
			{
				if (Proc == null)
				{
					Console.WriteLine("LogiBevy: could not start cargo; relying on prebuilt Rust artifact.");
					return;
				}
				// Async reads avoid a pipe-buffer deadlock on large cargo output.
				var OutTask = Proc.StandardOutput.ReadToEndAsync();
				var ErrTask = Proc.StandardError.ReadToEndAsync();
				Proc.WaitForExit();
				if (Proc.ExitCode != 0)
				{
					throw new BuildException("LogiBevy: cargo build for " + Package + " failed:\n" + ErrTask.Result);
				}
			}
		}
		catch (System.ComponentModel.Win32Exception)
		{
			// cargo not on PATH during this build; rely on the prebuilt artifact.
			Console.WriteLine("LogiBevy: cargo not found on PATH; relying on prebuilt Rust artifact.");
		}
	}
}
