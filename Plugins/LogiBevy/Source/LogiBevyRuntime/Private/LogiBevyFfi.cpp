// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyTypes.h"
#include "Modules/ModuleManager.h"
#include "Interfaces/IPluginManager.h"
#include "HAL/PlatformProcess.h"
#include "Misc/Paths.h"

#if LOGI_BEVY_HAVE_RUST
#include "logi_bevy_ffi.h"
#endif

#if LOGI_BEVY_HAVE_GEO
#include "logi_geo_ffi.h" // logi_geo_abi_version() + LOGI_GEO_ABI_VERSION (ABI drift check)
#endif

DEFINE_LOG_CATEGORY(LogLogiBevy);

#if LOGI_BEVY_HAVE_GEO
namespace
{
	// Runtime gate for the terrain DLL. The terrain exports are delay-loaded, so freeing the
	// DLL handle on an ABI mismatch does NOT stop a later call: the loader re-resolves and
	// re-loads the (mismatched) DLL on first use, then crashes. We flip this false on mismatch
	// and have the terrain subsystem consult it before EVER calling into the terrain DLL.
	// Defaults true so a clean load needs no opt-in.
	bool GLogiTerrainAbiOk = true;
}

// Defined here (the module owns the terrain-DLL load + ABI check). The globe + baked-terrain subsystems
// forward-declare this locally and consult it before any terrain-DLL call (the exports are delay-loaded).
bool LogiBevy_IsTerrainAbiOk()
{
	return GLogiTerrainAbiOk;
}
#endif // LOGI_BEVY_HAVE_GEO

// BC7 encoder (ispc_texcomp.dll) load gate — UNGATED so the tile-texture decode path links in every config.
// Set true in StartupModule iff the DLL loaded explicitly; the worker-thread tile decode consults it to SKIP
// BC7 (→ the tile shows a placeholder, NOT a crash) if the delay-load of ispc_texcomp.dll ever fails. (The
// explicit load itself is Win64 + GEO gated below.)
namespace
{
	bool GLogiIspcOk = false;
}
bool LogiBevy_IsIspcTexCompOk()
{
	return GLogiIspcOk;
}

/**
 * LogiBevyRuntime module. Owns the lifetime of the Rust `logi_ffi` DLL handle:
 * the DLL is staged next to this module's binary by RuntimeDependencies, and we
 * load it explicitly so the delay-loaded imports resolve.
 */
class FLogiBevyRuntimeModule : public IModuleInterface
{
public:
	virtual void StartupModule() override
	{
#if PLATFORM_WINDOWS && LOGI_BEVY_HAVE_RUST
		FString DllPath = TEXT("logi_ffi.dll");
		if (TSharedPtr<IPlugin> Plugin = IPluginManager::Get().FindPlugin(TEXT("LogiBevy")))
		{
			const FString Staged = FPaths::Combine(Plugin->GetBaseDir(), TEXT("Binaries"), TEXT("Win64"), TEXT("logi_ffi.dll"));
			if (FPaths::FileExists(Staged))
			{
				DllPath = Staged;
			}
		}

		DllHandle = FPlatformProcess::GetDllHandle(*DllPath);
		UE_CLOG(DllHandle == nullptr, LogLogiBevy, Error,
			TEXT("Failed to load logi_ffi.dll (looked for '%s'). The Bevy sim will be unavailable."), *DllPath);
		UE_CLOG(DllHandle != nullptr, LogLogiBevy, Log, TEXT("Loaded logi_ffi.dll: '%s'."), *DllPath);

		if (DllHandle != nullptr)
		{
			// Fail loud on a stale DLL (ABI drift) rather than crashing in a later call.
			const uint32 LibVersion = logi_abi_version();
			UE_CLOG(LibVersion != LOGI_ABI_VERSION, LogLogiBevy, Error,
				TEXT("logi_ffi ABI MISMATCH: header=%u, loaded DLL=%u. Rebuild the plugin (editor closed) to stage the matching DLL."),
				static_cast<uint32>(LOGI_ABI_VERSION), LibVersion);
			UE_CLOG(LibVersion == LOGI_ABI_VERSION, LogLogiBevy, Log, TEXT("logi_ffi ABI version %u OK."), LibVersion);
		}
#endif // PLATFORM_WINDOWS && LOGI_BEVY_HAVE_RUST

#if PLATFORM_WINDOWS && LOGI_BEVY_HAVE_GEO
		// Sibling of the logi_ffi load (not nested under HAVE_RUST) so the terrain feature
		// keys on LOGI_BEVY_HAVE_GEO alone — matching the Geo terrain/globe/building gating.
		{
			// Terrain kernel DLL (ADR-0007), loaded the same way as logi_ffi.dll.
			FString TerrainPath = TEXT("logi_geo.dll");
			if (TSharedPtr<IPlugin> Plugin = IPluginManager::Get().FindPlugin(TEXT("LogiBevy")))
			{
				const FString Staged = FPaths::Combine(Plugin->GetBaseDir(), TEXT("Binaries"), TEXT("Win64"), TEXT("logi_geo.dll"));
				if (FPaths::FileExists(Staged))
				{
					TerrainPath = Staged;
				}
			}
			TerrainDllHandle = FPlatformProcess::GetDllHandle(*TerrainPath);
			UE_CLOG(TerrainDllHandle == nullptr, LogLogiBevy, Warning, TEXT("Failed to load logi_geo.dll ('%s'); terrain unavailable."), *TerrainPath);
			UE_CLOG(TerrainDllHandle != nullptr, LogLogiBevy, Log, TEXT("Loaded logi_geo.dll."));

			if (TerrainDllHandle != nullptr)
			{
				// Fail loud on a stale terrain DLL (ABI drift) rather than crashing in a later call.
				const uint32 TerrainVersion = logi_geo_abi_version();
				if (TerrainVersion != LOGI_GEO_ABI_VERSION)
				{
					UE_LOG(LogLogiBevy, Error,
						TEXT("logi_geo ABI MISMATCH: header=%u, loaded DLL=%u. Rebuild the plugin (editor closed) to stage the matching DLL. Terrain unavailable."),
						static_cast<uint32>(LOGI_GEO_ABI_VERSION), TerrainVersion);
					// Treat terrain as unavailable. Freeing the handle is NOT enough: the terrain exports are
					// delay-loaded, so the first terrain call would re-resolve + re-load the mismatched DLL and
					// crash. Latch a runtime flag the subsystem checks before any terrain call, then drop the handle.
					GLogiTerrainAbiOk = false;
					FPlatformProcess::FreeDllHandle(TerrainDllHandle);
					TerrainDllHandle = nullptr;
				}
				else
				{
					UE_LOG(LogLogiBevy, Log, TEXT("logi_geo ABI version %u OK."), TerrainVersion);
				}
			}
		}

		// Intel ISPC texture compressor DLL (BC7 encode of the P3DT atlases). It is delay-loaded (Build.cs
		// PublicDelayLoadDLLs), and the FIRST CompressBlocksBC7 call runs on a worker thread whose delay-load
		// helper can't locate the engine ThirdParty DLL by name — that crashed with 0xC06D007E (MOD_NOT_FOUND).
		// Load it explicitly here (game thread, full engine path) so it is resident before any tile decode; the
		// later worker-thread delay-load then resolves the already-loaded module by name. GLogiIspcOk gates the
		// BC7 path off (→ placeholder) if this ever fails, so a missing DLL degrades instead of crashing.
		{
			const FString IspcPath = FPaths::Combine(FPaths::EngineDir(),
				TEXT("Binaries"), TEXT("ThirdParty"), TEXT("Intel"), TEXT("ISPCTexComp"), TEXT("Win64-Release"), TEXT("ispc_texcomp.dll"));
			IspcDllHandle = FPlatformProcess::GetDllHandle(*IspcPath);
			GLogiIspcOk = (IspcDllHandle != nullptr);
			UE_CLOG(IspcDllHandle == nullptr, LogLogiBevy, Warning,
				TEXT("Failed to load ispc_texcomp.dll ('%s'); BC7 tile-texture compression disabled (tiles show a placeholder, no crash)."), *IspcPath);
			UE_CLOG(IspcDllHandle != nullptr, LogLogiBevy, Log, TEXT("Loaded ispc_texcomp.dll (BC7 encoder)."));
		}
#endif // PLATFORM_WINDOWS && LOGI_BEVY_HAVE_GEO
	}

	virtual void ShutdownModule() override
	{
		if (DllHandle != nullptr)
		{
			FPlatformProcess::FreeDllHandle(DllHandle);
			DllHandle = nullptr;
		}
		if (TerrainDllHandle != nullptr)
		{
			FPlatformProcess::FreeDllHandle(TerrainDllHandle);
			TerrainDllHandle = nullptr;
		}
		if (IspcDllHandle != nullptr)
		{
			FPlatformProcess::FreeDllHandle(IspcDllHandle);
			IspcDllHandle = nullptr;
		}
	}

private:
	void* DllHandle = nullptr;
	void* TerrainDllHandle = nullptr;
	void* IspcDllHandle = nullptr;
};

IMPLEMENT_MODULE(FLogiBevyRuntimeModule, LogiBevyRuntime);
