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
			"Projects",          // IPluginManager (locating the staged DLL at runtime)
			"DeveloperSettings", // ULogiBevyDeveloperSettings (Project Settings)
		});

		// Plugins/LogiBevy/Source/LogiBevyRuntime -> .../Plugins/LogiBevy -> <ProjectRoot>
		string PluginDir = Path.GetFullPath(Path.Combine(ModuleDirectory, "..", ".."));
		string ProjectRoot = Path.GetFullPath(Path.Combine(PluginDir, "..", ".."));
		string RustDir = Path.Combine(ProjectRoot, "Rust");
		string FfiInclude = Path.Combine(RustDir, "crates", "logi_ffi", "include");
		string RustOutDir = Path.Combine(RustDir, "target", "release");

		PublicIncludePaths.Add(FfiInclude);

		if (Target.Platform == UnrealTargetPlatform.Win64)
		{
			TryBuildRustArtifact(RustDir);

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
		}
		else
		{
			// Other platforms: the bridge compiles to no-ops until the Rust target is added.
			PublicDefinitions.Add("LOGI_BEVY_HAVE_RUST=0");
		}
	}

	// Best-effort: keep the linked artifact fresh. If cargo is unavailable we fall
	// back to a prebuilt artifact (validated by the File.Exists checks above).
	private void TryBuildRustArtifact(string RustDir)
	{
		try
		{
			var Psi = new ProcessStartInfo
			{
				FileName = "cargo",
				Arguments = "build --release -p logi_ffi",
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
					throw new BuildException("LogiBevy: `cargo build --release -p logi_ffi` failed:\n" + ErrTask.Result);
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
