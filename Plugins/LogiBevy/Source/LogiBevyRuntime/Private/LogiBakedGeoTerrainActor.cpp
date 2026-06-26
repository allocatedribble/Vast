// Copyright LogiUE. All Rights Reserved.

#include "LogiBakedGeoTerrainActor.h"
#include "LogiBevyTypes.h"
#include "LogiTileRenderShared.h"   // FLogiStreamView / LogiResolveStreamView / LogiBuildTileNaniteMesh (shared render core)
#include "LogiTileMeshComponent.h"  // ULogiTileMeshComponent — per-tile render-thread bulk upload (audit T1)
#include "Engine/World.h"
#include "Components/SceneComponent.h"
#include "Components/StaticMeshComponent.h"
#include "Materials/MaterialInterface.h"
#include "Materials/MaterialInstanceDynamic.h"
#include "Materials/Material.h"                          // UMaterial — runtime-built debug heatmap base (editor)
#include "Materials/MaterialExpressionVectorParameter.h" // the "DebugColor" parameter node
#include "MaterialDomain.h"                              // MD_Surface (+ blend/shading-model enums)
#include "Components/MeshComponent.h"                    // UMeshComponent::SetMaterial/GetMaterial (debug rebinds)
#include "Debug/DebugDrawService.h"                      // UDebugDrawService — viewport telemetry overlay
#include "Engine/Canvas.h"                               // UCanvas (overlay draw)
#include "CanvasItem.h"                                  // FCanvasTextItem / FCanvasTileItem (overlay draw)
#include "Engine/Texture2D.h"
#include "TextureResource.h"
#include "RenderingThread.h"                   // ENQUEUE_RENDER_COMMAND — defer the BC7 mip upload to the render thread (audit T4)
#include "RHICommandList.h"                    // FRHICommandListImmediate / LockTexture2D / UnlockTexture2D (render-thread mip upload)
#include "RenderUtils.h"                       // CopyTextureData2D + GPixelFormats/FPixelFormatInfo — block-aware (BC7) mip copy
#include "IImageWrapper.h"
#include "IImageWrapperModule.h"
#include "Modules/ModuleManager.h"
#include "Misc/Paths.h"
#include "Misc/FileHelper.h"
#include "ProfilingDebugging/CpuProfilerTrace.h" // TRACE_CPUPROFILER_EVENT_SCOPE (zero cost when not tracing)
#include "HAL/PlatformTime.h"                  // FPlatformTime — geo-step game-thread cost probe
#include "RHIStats.h"                          // GNumDrawCallsRHI / GNumPrimitivesDrawnRHI (bench metrics)
#include "DynamicRHI.h"                        // RHIGetGPUFrameCycles (bench metrics)
#include "RenderTimer.h"                       // GGameThreadTime / GRenderThreadTime (bench metrics, cycles)
#include "Async/Async.h"                       // Async(EAsyncExecution::ThreadPool, …) — off-thread atlas decode
#include "ispc_texcomp.h"                      // Intel ISPC CompressBlocksBC7 — runtime BC7 encode of the tile atlases

#if LOGI_GEO_NANITE_LANE
#include "Engine/StaticMesh.h"
#include "Components/SkyAtmosphereComponent.h" // ASkyAtmosphere / USkyAtmosphereComponent — planet-true sky anchoring
#include "Engine/DirectionalLight.h"           // ADirectionalLight — sun-at-camera-zenith (planet-true sun)
#include "Engine/ExponentialHeightFog.h"       // AExponentialHeightFog — flat-world fog slab, disabled with the anchor
#include "Components/ExponentialHeightFogComponent.h"
#include "EngineUtils.h"                       // TActorIterator (find the level's SkyAtmosphere / sun / fog)
#endif

#if WITH_EDITOR
#include "MaterialEditingLibrary.h"                     // UMaterialEditingLibrary — wire the FadeAlpha focus-pull
#include "Materials/Material.h"                          // UMaterial
#include "Materials/MaterialExpressionScalarParameter.h" // the FadeAlpha + ClipBoxCount parameter nodes
#include "Materials/MaterialExpressionVectorParameter.h" // the per-box clip OBB parameter nodes
#include "Materials/MaterialExpressionTextureSampleParameter2D.h" // find/author the BaseColorTex sampler
#include "Materials/MaterialExpressionLinearInterpolate.h"        // placeholder→atlas emissive develop
#include "Materials/MaterialExpressionConstant3Vector.h"          // the neutral placeholder colour
#include "Materials/MaterialExpressionCustom.h"          // the hole-clip OpacityMask HLSL node
#include "Materials/MaterialExpressionWorldPosition.h"   // absolute world position fed to the clip test
#include "SceneTypes.h"                                  // EMaterialProperty::MP_EmissiveColor (focus-pull target)
#endif

#if LOGI_BEVY_HAVE_GEO
#include "logi_geo_ffi.h"
// Defined in LogiBevyFfi.cpp: false once a Geo-DLL ABI mismatch is detected. The logi_baked_* / logi_globe_*
// exports live in the same delay-loaded logi_geo.dll, so consult this before any world call.
bool LogiBevy_IsTerrainAbiOk();
// Defined in LogiBevyFfi.cpp: true iff ispc_texcomp.dll (the BC7 encoder) loaded at module startup. The tile
// decode consults it to SKIP BC7 (→ a placeholder tile) instead of crashing if that delay-loaded DLL is absent.
bool LogiBevy_IsIspcTexCompOk();

// Hole-clip: max clip OBBs the masked clip material's unrolled per-pixel loop handles — MUST equal
// MAX_CLIP_BOXES_PER_TILE in the Rust selector (logi_3dtiles_select). A REPLACE stand-in has >=1 hole, so it
// clips <=3 of its 4 quadtree children in the common case; 4 covers that with margin.
// Must match the Rust selector's MAX_CLIP_BOXES_PER_TILE (8, raised from 4 after the PIE skip-level
// overflow artifact): per-pixel cost rides ONLY stand-in tiles (ClipBoxCount bounds the unrolled loop).
static constexpr uint32 LOGI_CLIP_MAX = 8;

namespace
{
	// --- Google-tiles (ion) lane helpers -------------------------------------------------------------

	// Cesium ion token, read transiently: CESIUM_ION_TOKEN env var first (headless/CI), else the gitignored
	// <Project>/Intermediate/CesiumIonToken.txt. Never committed config.
	FString ReadIonToken()
	{
		FString Token = FPlatformMisc::GetEnvironmentVariable(TEXT("CESIUM_ION_TOKEN"));
		Token.TrimStartAndEndInline();
		if (!Token.IsEmpty()) { return Token; }
		FString FromFile;
		const FString Path = FPaths::Combine(FPaths::ProjectIntermediateDir(), TEXT("CesiumIonToken.txt"));
		if (FFileHelper::LoadFileToString(FromFile, *Path)) { FromFile.TrimStartAndEndInline(); return FromFile; }
		return FString();
	}

	// Box-downsample a tightly packed RGBA8 image (SrcW x SrcH) into Dst (DstW x DstH = half each, min 1 px),
	// averaging each 2x2 source block with round-to-nearest. Channel-agnostic: it does a per-channel box average
	// over all 4 bytes, so it works equally for RGBA8 or BGRA8 input. Odd source dims clamp the trailing sample to
	// the last row/column so the edge texel isn't dropped (no out-of-bounds read at SrcW/SrcH == 1). Averaging is
	// in sRGB byte space (not gamma-decoded): the cheap, standard runtime path — it slightly darkens mips at
	// high-contrast edges versus a gamma-correct filter, acceptable for photoreal aerial atlases. A 256-entry
	// sRGB->linear / linear->sRGB LUT pair is the upgrade if darkening ever shows on a flamegraph-justified path.
	void DownsampleBox(const uint8* Src, int32 SrcW, int32 SrcH, uint8* Dst, int32 DstW, int32 DstH)
	{
		for (int32 y = 0; y < DstH; ++y)
		{
			const int32 sy0 = y * 2;
			const int32 sy1 = FMath::Min(sy0 + 1, SrcH - 1);
			const uint8* Row0 = Src + static_cast<int64>(sy0) * SrcW * 4;
			const uint8* Row1 = Src + static_cast<int64>(sy1) * SrcW * 4;
			uint8* DstRow = Dst + static_cast<int64>(y) * DstW * 4;
			for (int32 x = 0; x < DstW; ++x)
			{
				const int32 sx0 = (x * 2) * 4;                              // byte offset of source column 2x
				const int32 sx1 = FMath::Min(x * 2 + 1, SrcW - 1) * 4;      // ...and 2x+1, clamped at the edge
				uint8* D = DstRow + static_cast<int64>(x) * 4;
				for (int32 c = 0; c < 4; ++c) // per-channel box average (channel-agnostic; A is opaque-255 here, averages to 255)
				{
					D[c] = static_cast<uint8>(
						(static_cast<uint32>(Row0[sx0 + c]) + Row0[sx1 + c] + Row1[sx0 + c] + Row1[sx1 + c] + 2) >> 2);
				}
			}
		}
	}

	// --- Runtime BC7 texture compression ---------------------------------------------------------------
	// Google Photorealistic 3D Tiles ship JPEG atlases (no GPU-ready KTX2/Basis to request), so we decode to
	// RGBA8 then compress to BC7 here on the worker thread. BC7 (16 bytes / 4x4 block) is near-LOSSLESS for
	// photographic colour — unlike BC1 (8 bytes/block, only TWO endpoint colours per 4x4), which crushed the
	// grass/ground into visible blocky patches (the reported "pixelated, independent of camera"). BC7 is 2x
	// BC1's VRAM but still HALF of uncompressed PF_B8G8R8A8, and the GPU samples it natively — fits an 8 GB GPU
	// at streaming scale. ISPC's `CompressBlocksBC7` does the encode (Intel ISPC Texture Compressor).

	// Compress a tightly packed W*H RGBA8 image to BC7 (PF_BC7 layout). ISPC reads whole 4x4 blocks, so a sub-4
	// / non-multiple-of-4 mip is padded up to (ceil(W/4)*4 x ceil(H/4)*4) with edge replication first (no
	// out-of-bounds read); for the common 256x256 atlas there is neither pad nor copy. Input is RGBA (not BGRA):
	// ISPC treats byte 0 as red, and PF_BC7 is sampled as RGBA, so decoding straight to RGBA keeps colours correct.
	void CompressRgbaToBC7(const uint8* Rgba, int32 W, int32 H, TArray64<uint8>& Out)
	{
		const int32 BlocksX = FMath::Max(1, (W + 3) / 4);
		const int32 BlocksY = FMath::Max(1, (H + 3) / 4);
		const int32 PaddedW = BlocksX * 4;
		const int32 PaddedH = BlocksY * 4;
		Out.SetNumUninitialized(static_cast<int64>(BlocksX) * BlocksY * 16); // BC7: 16 bytes / 4x4 block

		const uint8* SrcPtr = Rgba;
		int32 SrcStride = W * 4;
		TArray64<uint8> Padded;
		if (PaddedW != W || PaddedH != H)
		{
			Padded.SetNumUninitialized(static_cast<int64>(PaddedW) * PaddedH * 4);
			for (int32 y = 0; y < PaddedH; ++y)
			{
				const int32 sy = FMath::Min(y, H - 1);
				for (int32 x = 0; x < PaddedW; ++x)
				{
					const int32 sx = FMath::Min(x, W - 1);
					FMemory::Memcpy(&Padded[(static_cast<int64>(y) * PaddedW + x) * 4],
						Rgba + (static_cast<int64>(sy) * W + sx) * 4, 4);
				}
			}
			SrcPtr = Padded.GetData();
			SrcStride = PaddedW * 4;
		}

		rgba_surface Surf;
		Surf.ptr = const_cast<uint8*>(SrcPtr);
		Surf.width = PaddedW;
		Surf.height = PaddedH;
		Surf.stride = SrcStride;
		bc7_enc_settings Settings;
		GetProfile_veryfast(&Settings); // runtime-fast yet far above BC1's 2-colour-per-block floor (opaque atlases)
		CompressBlocksBC7(&Surf, Out.GetData(), &Settings);
	}

	// Worker-side decode: a tile atlas (JPEG/PNG) -> BC7 + full box-filtered mip chain, as plain CPU byte
	// buffers (NO UObject, NO RHI). Runs OFF the game thread (audit #3); the game thread only turns the result
	// into a UTexture2D (AcquireTexture). Returns false on an unrecognised/invalid image. `Module` is resolved
	// on the game thread (module load isn't thread-safe) and lives for the session; CreateImageWrapper makes a
	// fresh per-call wrapper, so concurrent decodes share no state.
	bool DecodeJpegToBgraMips(IImageWrapperModule* Module, const uint8* Data, int64 Len,
		int32& OutW, int32& OutH, TArray<TArray64<uint8>>& OutMips)
	{
		if (Module == nullptr || Data == nullptr || Len < 12) { return false; }
		// BC7 needs the ISPC encoder DLL (loaded explicitly at module startup). If that load failed, skip the
		// decode (the tile shows its placeholder) rather than crashing on the delay-loaded CompressBlocksBC7.
		if (!LogiBevy_IsIspcTexCompOk()) { return false; }
		EImageFormat Fmt = EImageFormat::Invalid;
		if (Data[0] == 0xFF && Data[1] == 0xD8) { Fmt = EImageFormat::JPEG; }
		else if (Data[0] == 0x89 && Data[1] == 0x50 && Data[2] == 0x4E && Data[3] == 0x47) { Fmt = EImageFormat::PNG; }
		else { return false; }

		TSharedPtr<IImageWrapper> Wrapper = Module->CreateImageWrapper(Fmt);
		if (!Wrapper.IsValid() || !Wrapper->SetCompressed(Data, Len)) { return false; }

		TArray64<uint8> Raw;
		// Decode straight to RGBA (not BGRA): the BC7 encoder (ISPC) treats byte 0 as red and PF_BC7 samples as
		// RGBA, so this keeps colours correct end-to-end. The box-downsample below is channel-agnostic (per-channel
		// average), so the same code path produces the RGBA mip chain.
		if (!Wrapper->GetRaw(ERGBFormat::RGBA, 8, Raw)) { return false; }
		const int32 W = Wrapper->GetWidth();
		const int32 H = Wrapper->GetHeight();
		if (W <= 0 || H <= 0 || Raw.Num() < static_cast<int64>(W) * H * 4) { return false; }

		TRACE_CPUPROFILER_EVENT_SCOPE(LogiGeoTileDecodeMips);
		const int32 NumMips = static_cast<int32>(FMath::FloorLog2(static_cast<uint32>(FMath::Max(W, H)))) + 1;
		OutMips.Reset();
		OutMips.SetNum(NumMips);

		// mip 0: the tightly packed W*H*4 RGBA8 (decoded RGBA above — ISPC's BC7 reads byte 0 as red; audit
		// 2026-06-10 §4.8 — the "BGRA" here was stale wording from the BC1 era). Clamp to that size — a decoder
		// returning padded rows could make Raw.Num() exceed it, and an unclamped copy would overrun the destination.
		const int64 Mip0Bytes = static_cast<int64>(W) * H * 4;
		OutMips[0].SetNumUninitialized(Mip0Bytes);
		FMemory::Memcpy(OutMips[0].GetData(), Raw.GetData(), FMath::Min<int64>(Raw.Num(), Mip0Bytes));

		// mips 1..N-1: each is the previous level box-downsampled to half size (min 1 px), so distant terrain
		// trilinear-filters instead of aliasing. Source is always the previous OutMips entry (a distinct buffer),
		// so there is no in-place read/write aliasing.
		int32 SrcW = W, SrcH = H;
		for (int32 i = 1; i < NumMips; ++i)
		{
			const int32 DstW = FMath::Max(1, SrcW >> 1);
			const int32 DstH = FMath::Max(1, SrcH >> 1);
			OutMips[i].SetNumUninitialized(static_cast<int64>(DstW) * DstH * 4);
			DownsampleBox(OutMips[i - 1].GetData(), SrcW, SrcH, OutMips[i].GetData(), DstW, DstH);
			SrcW = DstW;
			SrcH = DstH;
		}

		// Compress every RGBA8 mip → BC7 in place. Still on the worker thread, so the game thread only uploads the
		// small BC7 blocks. After this, OutMips holds PF_BC7 data (AcquireTexture uploads it).
		for (int32 i = 0; i < NumMips; ++i)
		{
			const int32 MipW = FMath::Max(1, W >> i);
			const int32 MipH = FMath::Max(1, H >> i);
			TArray64<uint8> Bc7;
			CompressRgbaToBC7(OutMips[i].GetData(), MipW, MipH, Bc7);
			OutMips[i] = MoveTemp(Bc7);
		}
		OutW = W;
		OutH = H;
		return true;
	}

	// Read the FFI's last-error message into an FString (UTF-8, not NUL-terminated — terminate it here).
	FString LastFfiError()
	{
		uint8 Buf[512];
		const size_t N = logi_geo_last_error_message(Buf, sizeof(Buf) - 1);
		// Clamp the NUL index (audit C++ L4): defends against a misbehaving FFI returning a length >= the
		// buffer (e.g. the required size rather than bytes written), which would overflow Buf by one byte.
		Buf[FMath::Min(N, sizeof(Buf) - 1)] = 0;
		return FString(UTF8_TO_TCHAR(reinterpret_cast<const char*>(Buf)));
	}
}
#endif // LOGI_BEVY_HAVE_GEO

ALogiBakedGeoTerrain::ALogiBakedGeoTerrain()
{
	PrimaryActorTick.bCanEverTick = true;
	PrimaryActorTick.bStartWithTickEnabled = true;

	// A bare scene root. Every tile gets its OWN pooled render component, attached here — there is deliberately
	// no shared ProceduralMeshComponent: a section-per-tile shared component forces an O(n) whole-component
	// scene-proxy rebuild on every tile add/remove/material-set, which is what drove the multi-second hitches.
	USceneComponent* SceneRoot = CreateDefaultSubobject<USceneComponent>(TEXT("Root"));
	RootComponent = SceneRoot;

	// Game-thread inbox for worker-decoded tile atlases. Shared (ThreadSafe) so a worker finishing after the
	// actor is torn down delivers into a still-alive queue rather than a freed actor.
	DecodeQueue = MakeShared<TQueue<FLogiTileDecode, EQueueMode::Mpsc>, ESPMode::ThreadSafe>();
}

bool ALogiBakedGeoTerrain::EnsureWorld()
{
#if LOGI_BEVY_HAVE_GEO
	if (BakedWorld != nullptr || GlobeWorld != nullptr)
	{
		return true; // exactly one is live for the current Source
	}
	if (bCreateAttempted)
	{
		return false; // a prior create failed — don't retry every tick (cleared on ReloadTerrain)
	}
	bCreateAttempted = true;

	// Defense-in-depth (C++ audit): make the create idempotent regardless of caller. The guard above means
	// both handles are already null here today, so this is a no-op now — but if a future Source-switch path
	// ever reaches a create with a live handle, releasing first destroys it instead of leaking it. Cheap;
	// destroy(nullptr) is a no-op on the Rust side.
	ReleaseWorld();

	if (!LogiBevy_IsTerrainAbiOk())
	{
		UE_LOG(LogLogiBevy, Warning, TEXT("Streamed Tiles Terrain: Geo DLL ABI not OK; skipping."));
		return false;
	}

	// ---- Google 3D Tiles (Cesium ion) -------------------------------------------------------------
	if (Source == ETerrainSource::GoogleTiles)
	{
		const FString Token = ReadIonToken();
		if (Token.IsEmpty())
		{
			UE_LOG(LogLogiBevy, Error,
				TEXT("Streamed Tiles Terrain: no Cesium ion token. Set $env:CESIUM_ION_TOKEN or write the token to '%s'."),
				*FPaths::Combine(FPaths::ProjectIntermediateDir(), TEXT("CesiumIonToken.txt")));
			return false;
		}
		LogiGlobeCreateInfo Info{};
		Info.ion_asset_id = static_cast<uint64>(GlobeIonAssetId);
		Info.origin_longitude_deg = OriginLongitude;
		Info.origin_latitude_deg = OriginLatitude;
		Info.origin_height_m = OriginHeight;
		Info.max_tile_distance_m = GlobeMaxTileDistanceM;
		Info.workers = static_cast<uint32>(FMath::Clamp(GlobeFetchWorkers, 1, 64)); // unified with the Rust FFI clamp(1, 64)
		Info.flash_cache_budget_mb = static_cast<uint32>(FMath::Max(0, GlobeFlashCacheBudgetMB));
		Info.cache_budget_bytes = bGlobeDiskCache
			? static_cast<uint64>(FMath::Max(1, GlobeCacheBudgetGB)) * 1024ull * 1024ull * 1024ull
			: 0ull;
		// Runtime fetch-backend knob (ABI v11). The enum is laid out to match LOGI_GLOBE_FETCH_BACKEND_* (0=Auto,
		// 1=ureq, 2=Cyper), so a straight cast selects the lane; Auto picks the CyperSource HTTP/3/QUIC lane for Google P3DT.
		Info.fetch_backend = static_cast<uint32>(GlobeFetchBackend);
		const FString CacheDir = FPaths::Combine(FPaths::ProjectSavedDir(), TEXT("P3DTCache"));
		// Keep the UTF-8 converters alive across the call.
		const FTCHARToUTF8 TokenUtf8(*Token);
		const FTCHARToUTF8 CacheDirUtf8(*CacheDir);
		GlobeWorld = logi_globe_world_create(&Info, TokenUtf8.Get(), bGlobeDiskCache ? CacheDirUtf8.Get() : nullptr);
		if (GlobeWorld == nullptr)
		{
			UE_LOG(LogLogiBevy, Error, TEXT("logi_globe_world_create failed: %s"), *LastFfiError());
			return false;
		}
		bUseNaniteLaneResolved = false; // photoreal tiles are textured proc-mesh, never Nanite
		UE_LOG(LogLogiBevy, Log,
			TEXT("Streamed Tiles Terrain: GOOGLE TILES (ion asset %lld, origin %.4f,%.4f,%.0f m; disk cache %s)."),
			GlobeIonAssetId, OriginLatitude, OriginLongitude, OriginHeight, bGlobeDiskCache ? TEXT("ON") : TEXT("off"));
		AnchorSkyToPlanet();
		return true;
	}

	// ---- Procedural / baked store (our mesher) ----------------------------------------------------
	const uint32 Meshing = (MeshingMode == EBakedTerrainMeshing::TIN) ? 1u : 0u;
	const float TinErr = FMath::Max(0.0f, TinErrorM);
	const float Skirt = FMath::Max(0.0f, SkirtDepthM);

	if (Source == ETerrainSource::Procedural)
	{
		LogiBakedProceduralInfo PInfo{};
		PInfo.seed = static_cast<uint64>(FMath::Max(0, ProceduralSeed));
		PInfo.origin_lon_deg = OriginLongitude;
		PInfo.origin_lat_deg = OriginLatitude;
		PInfo.origin_height_m = OriginHeight;
		PInfo.radius_m = FMath::Max(1.0, ProceduralRadiusM);
		PInfo.leaf_level = static_cast<uint32>(FMath::Clamp(ProceduralLeafLevel, 0, 10));
		PInfo.grid_n = static_cast<uint32>(FMath::Clamp(ProceduralGridN, 2, 513));
		PInfo.meshing_mode = Meshing;
		PInfo.tin_max_error_m = TinErr;
		PInfo.skirt_depth_m = Skirt;
		BakedWorld = logi_baked_world_create_procedural(&PInfo);
		if (BakedWorld == nullptr)
		{
			UE_LOG(LogLogiBevy, Error, TEXT("logi_baked_world_create_procedural failed."));
			return false;
		}
		UE_LOG(LogLogiBevy, Log,
			TEXT("Streamed Tiles Terrain: PROCEDURAL (seed %d, origin %.5f,%.5f, radius %.0f m, leaf %d, grid %d, meshing %s)."),
			ProceduralSeed, OriginLatitude, OriginLongitude, PInfo.radius_m, ProceduralLeafLevel,
			ProceduralGridN, Meshing == 1 ? TEXT("TIN") : TEXT("Uniform"));
	}
	else // ETerrainSource::BakedTiles
	{
		const FString StoreDir = FPaths::IsRelative(TileStoreDir)
			? FPaths::Combine(FPaths::ProjectSavedDir(), TileStoreDir)
			: TileStoreDir;
		LogiBakedCreateInfo Info{};
		Info.meshing_mode = Meshing;
		Info.tin_max_error_m = TinErr;
		Info.skirt_depth_m = Skirt;
		const FTCHARToUTF8 StoreUtf8(*StoreDir);
		BakedWorld = logi_baked_world_create(StoreUtf8.Get(), &Info);
		if (BakedWorld == nullptr)
		{
			UE_LOG(LogLogiBevy, Error,
				TEXT("logi_baked_world_create failed for '%s' (bake it, or switch Source to Procedural)."), *StoreDir);
			return false;
		}
		UE_LOG(LogLogiBevy, Log, TEXT("Streamed Tiles Terrain: BAKED store '%s', meshing %s, skirt %.1f m."), *StoreDir,
			Meshing == 1 ? TEXT("TIN") : TEXT("Uniform"), Skirt);
	}

#if LOGI_GEO_NANITE_LANE
	bUseNaniteLaneResolved = bUseNaniteMeshLane; // Nanite tessellation is editor-only (ADR-0008)
#else
	bUseNaniteLaneResolved = false;
#endif
	return true;
#else
	return false;
#endif
}

void ALogiBakedGeoTerrain::EnsureRenderComponents()
{
	// Resolve the source material once per world (gated on an explicit attempt flag, not material validity,
	// so a missing/unauthored asset doesn't re-run LoadObject every step).
	if (bMaterialResolveAttempted)
	{
		return;
	}
	bMaterialResolveAttempted = true;

	if (Source == ETerrainSource::GoogleTiles)
	{
		// Per-tile texturing needs a material with a "BaseColorTex" parameter (authored asset). Until it
		// exists the tiles draw with the proc-mesh default (untextured but visible); warn just once.
		UMaterialInterface* Mat = LoadObject<UMaterialInterface>(
			nullptr, TEXT("/Game/Globe/M_LogiGlobeTile.M_LogiGlobeTile"), nullptr, LOAD_NoWarn | LOAD_Quiet);
		if (Mat != nullptr)
		{
			GlobeBaseMaterial.Reset(Mat);
#if WITH_EDITOR
			EnsureGlobeMaterialFocusPull(Cast<UMaterial>(Mat)); // wire the LOD-reveal focus-pull (once, in-memory)
#endif
		}
		else if (!bWarnedNoGlobeMaterial)
		{
			UE_LOG(LogLogiBevy, Warning,
				TEXT("Streamed Tiles Terrain: material /Game/Globe/M_LogiGlobeTile not found; Google tiles render untextured until it is authored."));
			bWarnedNoGlobeMaterial = true;
		}
		// Optional LIT base for Force-Lit / Auto-lit tiles. Absent by default (P3DT is unlit), so this is
		// usually null and GlobeMaterialForTile keeps using the unlit material.
		if (UMaterialInterface* Lit = LoadObject<UMaterialInterface>(
				nullptr, TEXT("/Game/Globe/M_LogiGlobeTileLit.M_LogiGlobeTileLit"), nullptr, LOAD_NoWarn | LOAD_Quiet))
		{
			GlobeLitMaterial.Reset(Lit);
		}
		// Hole-clip MASKED variant: prefer an authored asset (shipping-grade, saved), else build it at runtime
		// (editor/PIE only). Without either, a coarse stand-in over loaded finer tiles renders un-clipped (the
		// overlap returns for it) — GlobeMaterialForTile falls back to the opaque base.
		if (UMaterialInterface* Clip = LoadObject<UMaterialInterface>(
				nullptr, TEXT("/Game/Globe/M_LogiGlobeTileClip.M_LogiGlobeTileClip"), nullptr, LOAD_NoWarn | LOAD_Quiet))
		{
			GlobeClipMaterial.Reset(Clip);
		}
#if WITH_EDITOR
		else if (UMaterialInterface* RuntimeClip = GetOrCreateGlobeClipMaterial())
		{
			GlobeClipMaterial.Reset(RuntimeClip);
		}
#endif
		return;
	}

	// Procedural / baked: the displacement MID when the toggle + material are set, else the default terrain
	// material (untextured-but-visible fallback if that asset is unauthored).
	UMaterialInterface* Mat = nullptr;
	if (bUseDisplacementMaterial && !DisplacementMaterial.IsNull())
	{
		if (UMaterialInterface* Disp = DisplacementMaterial.LoadSynchronous())
		{
			if (UMaterialInstanceDynamic* MID = UMaterialInstanceDynamic::Create(Disp, this))
			{
				MID->SetScalarParameterValue(TEXT("DisplacementScale"), DisplacementScale);
				Mat = MID;
			}
		}
	}
	if (Mat == nullptr)
	{
		Mat = LoadObject<UMaterialInterface>(
			nullptr, TEXT("/Game/Terrain/M_LogiBakedTerrain.M_LogiBakedTerrain"), nullptr, LOAD_NoWarn | LOAD_Quiet);
	}
	if (Mat != nullptr)
	{
		TerrainMaterial.Reset(Mat);
	}
}

// === Per-tile pooled render resources ====================================================================
// Each tile owns its own component so an add/remove only rebuilds that tile's tiny proxy (O(1)) instead of the
// whole shared-component proxy (O(resident)); idle resources are recycled, so steady-state streaming allocates
// no UObjects. These helpers are all game-thread.

ULogiTileMeshComponent* ALogiBakedGeoTerrain::AcquireProcComp()
{
	while (ProcCompPool.Num() > 0)
	{
		TStrongObjectPtr<ULogiTileMeshComponent> Pooled = ProcCompPool.Pop(EAllowShrinking::No);
		if (ULogiTileMeshComponent* Comp = Pooled.Get())
		{
			Comp->SetTileVisible(true); // already attached + registered; in-place show (C4c — no proxy recreate)
			return Comp;
		}
	}
	// Pool empty — make one. Tile verts are absolute world cm, so the component IGNORES the actor transform
	// (the rule the old shared mesh used). RF_Transient: never saved with the level.
	ULogiTileMeshComponent* Comp = NewObject<ULogiTileMeshComponent>(this, NAME_None, RF_Transient);
	Comp->SetupAttachment(RootComponent);
	Comp->SetMobility(EComponentMobility::Movable);
	Comp->SetAbsolute(true, true, true);
	// Terrain render shell — never a navigation obstacle (ADR-0007). Without this, EACH per-tile component is
	// pushed into the navigation octree on register (1000s of empty-bounds adds + warning spam on the game
	// thread during a stream burst); the old single shared component made that negligible.
	Comp->SetCanEverAffectNavigation(false);
	Comp->RegisterComponent();
	Comp->SetRelativeLocation(FVector::ZeroVector);
	return Comp;
}

#if WITH_EDITOR
void ALogiBakedGeoTerrain::EnsureGlobeMaterialFocusPull(UMaterial* M)
{
	// Wire the globe tile material's LOD-reveal FOCUS-PULL once per session (in-memory + recompiled — the editor
	// Python material API can't reach these properties). NOTE: this used to ALSO wire a `DepthBiasCm` → Pixel
	// Depth Offset to push a coarse tile behind a finer one wherever they overlapped. That PDO is REMOVED: the
	// geometric hole-clip discards a coarse stand-in's fragments inside the tight decoded-mesh bounds of the
	// finer tiles drawn over it (ApplyTileClip below), so there is no coplanar coarse-over-fine geometry to
	// depth-separate — and a PDO large enough to separate real terrain (a coarse mesh sits metres above a fine
	// one on convex ground) would shift the scene depth SkyAtmosphere reads (the blue tile-quilt). Overlap is
	// removed GEOMETRICALLY and off-screen tiles are dropped by frustum culling — neither is papered over with a
	// depth hack. Idempotent within a session via the static latch. Editor-only — `RecompileMaterial` needs the
	// shader compiler.
	static bool bWired = false;
	if (bWired || M == nullptr)
	{
		return;
	}
	bWired = true;

	// LOD reveal via a FOCUS-PULL (rack-focus). The material stays OPAQUE and ALWAYS samples the real atlas — only the
	// sampled MIP changes, soft → sharp as FadeAlpha 0→1. A refined-in tile is shown only once it is fully TEXTURED
	// (the Rust texture gate, ABI v9, holds its coarse parent drawn over it until then — invisible LOD loading), so the
	// reveal never shows gray and never reveals sky: a tile starts at ~its coarse parent's apparent resolution and
	// crisps up, hiding the LOD jump. Force Opaque first (a prior session may have saved Masked/dither wiring). If the
	// authored BaseColorTex sampler isn't found, leave the emissive as authored (tiles snap to sharp, still their
	// atlas — never white/gray).
	M->BlendMode = BLEND_Opaque;
	UMaterialExpressionTextureSampleParameter2D* BaseTex = nullptr;
	for (UMaterialExpression* MatEx : M->GetExpressions())
	{
		if (UMaterialExpressionTextureSampleParameter2D* TS = Cast<UMaterialExpressionTextureSampleParameter2D>(MatEx))
		{
			if (TS->ParameterName == FName(TEXT("BaseColorTex"))) { BaseTex = TS; break; }
		}
	}
	UMaterialExpressionScalarParameter* Fade = Cast<UMaterialExpressionScalarParameter>(
		UMaterialEditingLibrary::CreateMaterialExpression(M, UMaterialExpressionScalarParameter::StaticClass(), -760, 660));
	UMaterialExpressionLinearInterpolate* MipLerp = Cast<UMaterialExpressionLinearInterpolate>(
		UMaterialEditingLibrary::CreateMaterialExpression(M, UMaterialExpressionLinearInterpolate::StaticClass(), -470, 760));
	if (BaseTex != nullptr && Fade != nullptr && MipLerp != nullptr)
	{
		Fade->ParameterName = FName(TEXT("FadeAlpha"));
		Fade->DefaultValue = 1.0f; // rest state: no per-tile override ⇒ sample mip 0 (fully sharp)
		// MipValue = lerp(SoftBias, 0, FadeAlpha): FadeAlpha 0 ⇒ SoftBias (soft), 1 ⇒ 0 (sharp). ConstA/ConstB carry
		// the endpoints, so only FadeAlpha needs wiring (no extra Constant nodes).
		MipLerp->ConstA = GlobeFadeSoftBias; // soft start (a few mips coarse ≈ the coarse parent's detail)
		MipLerp->ConstB = 0.0f;              // sharp end
		UMaterialEditingLibrary::ConnectMaterialExpressions(Fade, FString(), MipLerp, TEXT("Alpha"));
		// Sample the atlas at that mip bias (wire the MipValue input struct directly — its pin name varies by mode)
		// and emit it as emissive (P3DT bakes its own lighting ⇒ unlit). A failed wire just leaves mip 0 → instant
		// sharp, never gray/white.
		BaseTex->MipValueMode = TMVM_MipBias;
		BaseTex->MipValue.Expression = MipLerp;
		UMaterialEditingLibrary::ConnectMaterialProperty(BaseTex, TEXT("RGB"), MP_EmissiveColor);
		UE_LOG(LogLogiBevy, Log, TEXT("Globe tile material: wired FadeAlpha focus-pull (soft->sharp mip bias) for the LOD reveal."));
	}
	else
	{
		UE_LOG(LogLogiBevy, Warning, TEXT("Globe tile material: focus-pull wiring skipped (BaseColorTex sampler not found); tiles snap, no reveal."));
	}

	UMaterialEditingLibrary::RecompileMaterial(M);
}
#endif // WITH_EDITOR

UMaterialInstanceDynamic* ALogiBakedGeoTerrain::AcquireMid(UMaterialInterface* Base)
{
	// Per-parent bucket (audit 2026-06-10 C3): a MID can't be re-parented, and the hole-clip lane recycles
	// BOTH the opaque and the masked base into the pool — the old flat pool popped-and-DROPPED every
	// non-matching MID while hunting a match, destroying pooled MIDs of the other base exactly during LOD
	// convergence (when both kinds churn). Bucketed, acquire is O(1) and never destroys a usable MID.
	if (Base == nullptr) { return nullptr; }
	if (TArray<TStrongObjectPtr<UMaterialInstanceDynamic>>* Bucket = MidPool.Find(TObjectKey<UMaterialInterface>(Base)))
	{
		while (Bucket->Num() > 0)
		{
			TStrongObjectPtr<UMaterialInstanceDynamic> Pooled = Bucket->Pop(EAllowShrinking::No);
			if (UMaterialInstanceDynamic* Mid = Pooled.Get())
			{
				return Mid; // caller re-points BaseColorTex
			}
		}
	}
	return UMaterialInstanceDynamic::Create(Base, this);
}

void ALogiBakedGeoTerrain::RecycleMid(TStrongObjectPtr<UMaterialInstanceDynamic>&& Mid)
{
	if (UMaterialInstanceDynamic* M = Mid.Get())
	{
		MidPool.FindOrAdd(TObjectKey<UMaterialInterface>(M->Parent)).Add(MoveTemp(Mid));
	}
}

UMaterialInterface* ALogiBakedGeoTerrain::GlobeMaterialForTile(uint32 TileFlags, bool bWantClip)
{
#if LOGI_BEVY_HAVE_GEO
	// Hole-clip: a coarse stand-in drawn over loaded finer tiles uses the MASKED clip variant (unlit — P3DT is all
	// unlit) so the renderer can discard its fragments inside the child footprints. Only when one is available; if
	// neither authored nor runtime-built, fall through to the opaque base (the tile renders un-clipped — the
	// coplanar overlap returns for it, but no crash).
	if (bWantClip)
	{
		if (UMaterialInterface* Clip = GlobeClipMaterial.Get())
		{
			return Clip;
		}
	}
	bool bWantUnlit;
	switch (GoogleTileLighting)
	{
	case EGoogleTileLighting::ForceUnlit: bWantUnlit = true; break;
	case EGoogleTileLighting::ForceLit:   bWantUnlit = false; break;
	case EGoogleTileLighting::Auto:
	default:                              bWantUnlit = (TileFlags & LOGI_GLOBE_TILE_FLAG_UNLIT) != 0; break;
	}
	if (!bWantUnlit)
	{
		if (UMaterialInterface* Lit = GlobeLitMaterial.Get())
		{
			return Lit;
		}
		// Lit requested but no lit material authored — fall back to the unlit base (better than the gray
		// default) and warn once. Author /Game/Globe/M_LogiGlobeTileLit to enable lit Google-tile rendering.
		if (!bWarnedNoLitGlobeMaterial)
		{
			UE_LOG(LogLogiBevy, Warning,
				TEXT("Streamed Tiles Terrain: lit Google-tile rendering requested but /Game/Globe/M_LogiGlobeTileLit is not authored; using the unlit material."));
			bWarnedNoLitGlobeMaterial = true;
		}
	}
	return GlobeBaseMaterial.Get();
#else
	(void)TileFlags;
	(void)bWantClip;
	return nullptr;
#endif
}

void ALogiBakedGeoTerrain::RecycleTile(FTileEntry& Entry)
{
	if (ULogiTileMeshComponent* Comp = Entry.ProcComp.Get())
	{
		Comp->ClearTileGeometry();                                  // free the geometry (O(1): drops this tile's proxy)
		Comp->SetTileVisible(false); // in-place flag flip (C4c) — the proxy is already gone
		ProcCompPool.Add(MoveTemp(Entry.ProcComp));                 // keep it registered + GC-rooted for reuse
	}
	if (Entry.Material.IsValid())
	{
		RecycleMid(MoveTemp(Entry.Material)); // bucketed by parent (C3)
	}
	if (Entry.DebugMid.IsValid())
	{
		DebugMidPool.Add(MoveTemp(Entry.DebugMid)); // pooled debug heatmap MID (debug views only)
	}
	if (UTexture2D* Tex = Entry.Texture.Get())
	{
		const FTexturePlatformData* PD = Tex->GetPlatformData();
		const uint32 Key = (PD != nullptr) ? TilePoolKey(PD->SizeX, PD->SizeY) : 0u;
		ReturnTextureToPool(Key, MoveTemp(Entry.Texture)); // bounded by GlobeMaxPooledTextures
	}
	// Nanite static meshes are unique per tile content (editor-only lane) — not pooled; destroy.
	if (UStaticMeshComponent* Comp = Entry.NaniteComp.Get())
	{
		Comp->DestroyComponent();
	}
}

void ALogiBakedGeoTerrain::ReturnTextureToPool(uint32 PoolKey, TStrongObjectPtr<UTexture2D>&& Tex)
{
	if (!Tex.IsValid()) { return; }
	// Bound the pooled-atlas VRAM. These textures are NeverStream (they bypass UE's texture streamer), so an
	// unbounded pool would grow VRAM without limit across a long fly-around (memory audit L3/L4). Past the cap,
	// drop the texture and let GC reclaim its RHI; reuse still covers the steady-state working set, and the
	// resident set is bounded separately by the cull radius.
	if (GlobeMaxPooledTextures <= 0 || PooledTextureCount >= GlobeMaxPooledTextures)
	{
		return; // Tex goes out of scope here → GC reclaims the UObject + its RHI
	}
	TexturePool.FindOrAdd(PoolKey).Add(MoveTemp(Tex));
	++PooledTextureCount;
}

void ALogiBakedGeoTerrain::DestroyAllTiles()
{
	for (TPair<uint64, FTileEntry>& Pair : Tiles)
	{
		if (ULogiTileMeshComponent* Comp = Pair.Value.ProcComp.Get()) { Comp->DestroyComponent(); }
		if (UStaticMeshComponent* Comp = Pair.Value.NaniteComp.Get()) { Comp->DestroyComponent(); }
	}
	Tiles.Reset(); // drops all per-tile MID/texture strong refs
	PendingTexture.Reset();
	TexturedKeys.Reset();      // un-report everything to the Rust gate (the world is being torn down)
	bTexturedKeysDirty = true;
	// Reset the delta-reconcile baseline (ADR-0012): the next poll must be treated as a CHANGED frame so the lane
	// does a full reconcile against whatever the (possibly recreated) world publishes, rebuilding `Tiles` cleanly.
	LastFrameId = 0;
	bDeltaInSync = false;
}

void ALogiBakedGeoTerrain::ClearPools()
{
	for (TStrongObjectPtr<ULogiTileMeshComponent>& C : ProcCompPool)
	{
		if (ULogiTileMeshComponent* Comp = C.Get()) { Comp->DestroyComponent(); }
	}
	ProcCompPool.Reset();
	MidPool.Reset();      // MIDs + textures: drop the strong refs, let GC reclaim
	DebugMidPool.Reset();
	ActiveDebugView = EGeoDebugView::None; // tiles are gone; the next step re-binds debug MIDs if a view is on
	TexturePool.Reset();
	PooledTextureCount = 0;
	PendingBinds.Reset();           // drop uploads awaiting confirmation — the world is being rebuilt
	AwaitingTextureConfirm.Reset();
	DecodingKeys.Reset();
	DepartedGrace.Reset();          // retained departed tiles die with the world (their components recycle above)
	// LOD cross-fade: drop the fading set too (the world is being rebuilt).
	FadingKeys.Reset();
	TexturedKeys.Reset();          // the renderer's report to the Rust gate — empty for the rebuilt world
	bTexturedKeysDirty = true;     // push the empty set next step (prime the gate fresh)
	// Hand any in-flight workers a fresh inbox: a decode still running holds the OLD shared queue and delivers
	// into it (freed when the last worker drops it), so stale results can't reach the rebuilt state.
	DecodeQueue = MakeShared<TQueue<FLogiTileDecode, EQueueMode::Mpsc>, ESPMode::ThreadSafe>();
}

#if LOGI_BEVY_HAVE_GEO
// Game-thread: ensure a freshly-created texture's platform-data mip CHAIN exists with the right per-mip dimensions
// and a correctly-SIZED (but deliberately UNINITIALISED) BulkData allocation for each mip — WITHOUT copying any pixel
// bytes (audit T4: the per-mip pixel Memcpy is what we move off the game thread). CreateTransient already allocated
// mip 0; this appends mips 1..N-1 and (re)sizes each BulkData so the subsequent UpdateResource() builds a correctly-
// laid-out RHI texture + full mip chain. The bytes are left uninitialised: UpdateResource's render-thread CreateTexture
// seed-uploads this garbage once (a single tile's worth, ON THE RENDER THREAD), and EnqueueTileMipUploadRT immediately
// overwrites every mip with the real BC7 data. Lock-THEN-Realloc matches CreateTransient: a freshly-added mip has no
// allocation, so Lock alone returns null; Realloc allocates (or no-ops at the same size) and — crucially — does NOT
// memset, so this stays O(allocations), not O(bytes), on the game thread. `NumMips` is the full chain length.
void ALogiBakedGeoTerrain::AllocateMipDescriptors(UTexture2D* Tex, int32 W, int32 H, int32 NumMips)
{
	FTexturePlatformData* PD = (Tex != nullptr) ? Tex->GetPlatformData() : nullptr;
	if (PD == nullptr) { return; }
	for (int32 i = 0; i < NumMips; ++i)
	{
		const int32 MipW = FMath::Max(1, W >> i);
		const int32 MipH = FMath::Max(1, H >> i);
		// BC7 (PF_BC7): 16 bytes per 4x4 block, ceil-divided so a non-multiple-of-4 mip's edge block counts.
		const int64 Bytes = static_cast<int64>(FMath::Max(1, (MipW + 3) / 4)) * FMath::Max(1, (MipH + 3) / 4) * 16;
		FTexture2DMipMap* Mip;
		if (i < PD->Mips.Num())
		{
			Mip = &PD->Mips[i];
		}
		else
		{
			Mip = new FTexture2DMipMap(static_cast<uint32>(MipW), static_cast<uint32>(MipH), 1);
			PD->Mips.Add(Mip); // TIndirectArray takes ownership
		}
		Mip->BulkData.Lock(LOCK_READ_WRITE);
		Mip->BulkData.Realloc(Bytes); // allocate only — no pixel copy (the bytes land on the render thread, T4)
		Mip->BulkData.Unlock();
	}
}

// Enqueue the BC7 mip-byte upload onto the RENDER THREAD (audit T4). The game thread has already created/acquired the
// UTexture2D and made its RHI resource live (CreateTransient + UpdateResource for a fresh texture, or a pooled texture
// whose RHI is already resident); here it only hands off a pointer to the texture's render resource + a SHARED ref to
// the BC7 bytes, then returns — the per-mip Memcpy now happens on the render thread.
//
// LIFETIME (the load-bearing part): the BC7 bytes live in `Mips`, a thread-safe shared pointer captured BY VALUE in the
// render lambda. So the bytes are guaranteed to outlive the asynchronous upload no matter when it runs (it may execute
// after the next game tick has already polled/drained again) — the chain is freed only when the render thread drops this
// last reference at the end of the command. This mirrors the T1 mesh proxy's FLogiTilePayload and the JPEG-copy in
// DispatchTileDecode. We capture the FTextureResource* (a render-thread-owned object) rather than the UTexture2D* (a
// UObject — must never be touched off the game thread); the UObject is kept alive meanwhile by the caller's
// TStrongObjectPtr (FTileEntry::Texture or the pool), and a UObject teardown enqueues the resource's ReleaseResource
// AFTER this command (render commands are FIFO), so the resource is valid for the duration of this command (re-validated
// defensively below).
void ALogiBakedGeoTerrain::EnqueueTileMipUploadRT(UTexture2D* Tex, const TSharedRef<FLogiTileMipChain, ESPMode::ThreadSafe>& Mips,
	const TSharedRef<FLogiTexPipeStats, ESPMode::ThreadSafe>& Stats)
{
	FTextureResource* Resource = (Tex != nullptr) ? Tex->GetResource() : nullptr;
	if (Resource == nullptr)
	{
		Stats->UploadNoRhi.fetch_add(1, std::memory_order_relaxed);
		Mips->UploadState.store(-1, std::memory_order_release);
		return;
	}

	ENQUEUE_RENDER_COMMAND(LogiGeoTileMipUpload)(
		[Resource, Mips, Stats](FRHICommandListImmediate& RHICmdList)
		{
			TRACE_CPUPROFILER_EVENT_SCOPE(LogiGeoTileTexUploadRT);

			// The RHI texture is created by FStreamableTextureResource::InitRHI (enqueued by the game thread's
			// UpdateResource, so it runs BEFORE this FIFO-ordered command) and persists until the resource is released
			// on this same thread — so by the time this runs it is valid. Re-fetch + null-check anyway (defence in
			// depth): if the resource was torn down before this ran, GetTexture2DRHI() returns null and we drop the upload.
			FRHITexture* TexRHI = Resource->GetTexture2DRHI();
			if (TexRHI == nullptr)
			{
				Stats->UploadNoRhi.fetch_add(1, std::memory_order_relaxed);
				Mips->UploadState.store(-1, std::memory_order_release);
				return;
			}

			const EPixelFormat Format = TexRHI->GetFormat();      // PF_BC7 (block-compressed)
			const FPixelFormatInfo& FmtInfo = GPixelFormats[Format];
			const int32 RHIMips = static_cast<int32>(TexRHI->GetNumMips());
			const int32 W = Mips->W;
			const int32 H = Mips->H;
			const int32 Num = FMath::Min(Mips->Mips.Num(), RHIMips); // never write past the RHI mip chain
			// Track whether EVERY RHI mip received its real bytes. If any is skipped below (or the RHI has more mips
			// than we have source for), the texture is NOT fully resident and must not be shown — a fresh texture
			// would show its uninitialised seed (black), a pooled one the previous tile's atlas (wrong-atlas).
			bool bAllMipsUploaded = (Num == RHIMips);
			if (!bAllMipsUploaded) { Stats->UploadMipMismatch.fetch_add(1, std::memory_order_relaxed); }

			for (int32 i = 0; i < Num; ++i)
			{
				const TArray64<uint8>& Src = Mips->Mips[i];
				if (Src.Num() == 0)
				{
					Stats->UploadShortMip.fetch_add(1, std::memory_order_relaxed);
					bAllMipsUploaded = false;
					continue;
				}
				const int32 MipW = FMath::Max(1, W >> i);
				const int32 MipH = FMath::Max(1, H >> i);

				// Tight source pitch/size for this BC7 mip from the (exported) pixel-format block info: a "row" is one
				// row of blocks, so SrcPitch = ceil(MipW/BlockSizeX) * BlockBytes and rows = ceil(MipH/BlockSizeY).
				// For PF_BC7 that is ceil(MipW/4)*16 — exactly what the worker's CompressRgbaToBC7 emitted, so TightSize
				// equals Src.Num() for a well-formed mip. (We compute this here rather than call the engine's
				// FTexture2DResource::CalculateTightPackedMipSize, which is a non-exported static and won't link.)
				const uint32 BlocksX = FMath::DivideAndRoundUp<uint32>(MipW, FmtInfo.BlockSizeX);
				const uint32 BlocksY = FMath::DivideAndRoundUp<uint32>(MipH, FmtInfo.BlockSizeY);
				const uint32 SrcPitch = BlocksX * static_cast<uint32>(FmtInfo.BlockBytes);
				const int64 TightSize = static_cast<int64>(SrcPitch) * BlocksY;
				if (TightSize > Src.Num())
				{
					Stats->UploadShortMip.fetch_add(1, std::memory_order_relaxed);
					bAllMipsUploaded = false;
					continue; // short/truncated source — never read past the decode output
				}

				// Lock the live RHI mip for write. DestStride is the GPU row pitch (for BC7 a "row" is one row of 4x4
				// blocks); it may be PADDED above the tight source pitch (e.g. D3D12 aligns row pitch), so we copy
				// block-row by block-row with CopyTextureData2D — the same block-aware primitive the engine uses to
				// upload compressed mips (FTexture2DResource::CreateTexture). A flat Memcpy would corrupt rows whenever
				// DestStride != SrcPitch. This is the per-tile pixel Memcpy — now ON THE RENDER THREAD (the T4 win).
				uint32 DestStride = 0;
				void* Dest = RHICmdList.LockTexture2D(TexRHI, static_cast<uint32>(i), RLM_WriteOnly, DestStride,
					/*bLockWithinMiptail=*/false);
				if (Dest == nullptr)
				{
					Stats->UploadLockFailed.fetch_add(1, std::memory_order_relaxed);
					bAllMipsUploaded = false;
					continue; // LockTexture2D failed (upload-heap pressure) — mip left unwritten
				}
				CopyTextureData2D(Src.GetData(), Dest, MipH, Format, SrcPitch, DestStride);
				RHICmdList.UnlockTexture2D(TexRHI, static_cast<uint32>(i), /*bLockWithinMiptail=*/false);
			}
			// Publish the upload RESULT for the game thread (ProcessPendingTextureBinds): 1 = fully resident (safe to
			// bind + show), -1 = a mip was dropped (retry the upload; never show the partial/stale texture).
			Mips->UploadState.store(bAllMipsUploaded ? 1 : -1, std::memory_order_release);
			// `Mips` (shared ref) and the captured `Resource` go out of scope here — the BC7 CPU bytes are freed now
			// that they are resident in the RHI texture. Nothing else holds them.
		});
}

UTexture2D* ALogiBakedGeoTerrain::AcquireTexture(const TSharedRef<FLogiTileMipChain, ESPMode::ThreadSafe>& Mips)
{
	const int32 W = Mips->W;
	const int32 H = Mips->H;
	if (W <= 0 || H <= 0 || Mips->Mips.Num() == 0) { return nullptr; }
	const uint32 PoolKey = TilePoolKey(W, H);
	if (TArray<TStrongObjectPtr<UTexture2D>>* Bucket = TexturePool.Find(PoolKey))
	{
		while (Bucket->Num() > 0)
		{
			TStrongObjectPtr<UTexture2D> Pooled = Bucket->Pop(EAllowShrinking::No);
			--PooledTextureCount; // popped from the pool (whether reused below or dropped as unusable)
			UTexture2D* Tex = Pooled.Get();
			// Same dims => same mip count + a still-live RHI texture: overwrite the existing RHI mips IN PLACE on the
			// render thread (no UObject churn, no UpdateResource, and no seed upload — the resource is already
			// resident). The game thread does zero pixel work; the BC7 Memcpy runs in the enqueued render command.
			if (Tex != nullptr && Tex->GetPlatformData() != nullptr
				&& Tex->GetPlatformData()->Mips.Num() == Mips->Mips.Num() && Tex->GetResource() != nullptr)
			{
				EnqueueTileMipUploadRT(Tex, Mips, TexStats);
				return Tex;
			}
		}
	}
	// None pooled for this size — make one. Mip 0 from CreateTransient; AllocateMipDescriptors appends 1..N-1 and
	// sizes each BulkData (no pixel copy — T4). SRGB + NeverStream + (editor) TMGS_NoMipmaps keep the whole chain
	// GPU-resident with mip 0 pinned (audit item #2). PF_BC7: the mips arrive pre-compressed to BC7 (2x BC1's VRAM,
	// half of uncompressed; the GPU samples them natively). UpdateResource() then builds the RHI texture + full mip
	// chain (seed-uploading the uninitialised BulkData once, on the render thread), and EnqueueTileMipUploadRT
	// overwrites every mip with the real BC7 bytes — also on the render thread. The game thread does only the UObject
	// alloc + mip-descriptor allocation + handoff, never the per-mip pixel Memcpy (audit T4).
	UTexture2D* Tex = UTexture2D::CreateTransient(W, H, PF_BC7);
	if (Tex == nullptr) { return nullptr; }
	Tex->SRGB = true;
	Tex->NeverStream = true;
	// ANISOTROPIC filtering (audit: grazing-angle ground blur). At an aerial-oblique view the ground is sampled
	// at a shallow angle, so its texels project to long, thin screen footprints — isotropic/low-aniso filtering
	// then reads a too-coarse mip along the minor axis and the GROUND blurs while head-on BUILDINGS stay sharp
	// (the exact reported symptom). `Filter = TF_Default` + the WORLD LOD group resolve to an anisotropic sampler
	// (capped by `r.MaxAnisotropy`, raised to 8 in DefaultEngine.ini); setting the group explicitly guarantees it
	// regardless of CreateTransient's defaults. Set BEFORE UpdateResource so it's baked into the RHI sampler.
	Tex->Filter = TF_Default;
	Tex->LODGroup = TEXTUREGROUP_World;
	// CLAMP addressing (the 2026-06-12 ocean grid-seam fix). CreateTransient leaves AddressX/Y at TA_Wrap, so a
	// border sample at UV≈0/1 bilinear-blends the tile's OPPOSITE edge texels — a thin wrong-color line along
	// every tile boundary, faint over uniform ocean and wider at the deep mips a grazing orbital view samples
	// (half a texel at mip k covers 2^k source texels). Each tile's atlas is self-contained (UVs span [0,1],
	// no tiling repeat), so clamp is unconditionally correct. Set BEFORE UpdateResource (baked into the RHI
	// sampler); the material samples with SSM_FromTextureAsset, so the texture's address mode is what binds.
	Tex->AddressX = TA_Clamp;
	Tex->AddressY = TA_Clamp;
#if WITH_EDITORONLY_DATA
	Tex->MipGenSettings = TMGS_NoMipmaps; // "no GENERATION" — we supply the chain; pins LOD bias to 0
#endif
	AllocateMipDescriptors(Tex, W, H, Mips->Mips.Num());
	Tex->UpdateResource();
	EnqueueTileMipUploadRT(Tex, Mips, TexStats);
	return Tex;
}

void ALogiBakedGeoTerrain::DispatchTileDecode(uint64 Key, const uint8* Jpeg, int64 Len)
{
	if (Jpeg == nullptr || Len <= 0 || !DecodeQueue.IsValid() || DecodingKeys.Contains(Key)) { return; }
	// Resolve the ImageWrapper module once, on the game thread (module load isn't thread-safe). The pointer is
	// stable for the session, so the workers may use it.
	if (ImageWrapperModule == nullptr)
	{
		ImageWrapperModule = &FModuleManager::LoadModuleChecked<IImageWrapperModule>(TEXT("ImageWrapper"));
	}
	DecodingKeys.Add(Key);
	TexStats->DecodeDispatched.fetch_add(1, std::memory_order_relaxed);
	// Copy the JPEG out of the frame-transient FFI buffer (the next poll may overwrite it), then decode on a
	// worker. The task captures only POD + the copied bytes + the shared queue + the module pointer — never
	// `this` — so it is safe regardless of the actor's lifetime.
	TArray<uint8> Bytes(Jpeg, static_cast<int32>(Len));
	TSharedPtr<TQueue<FLogiTileDecode, EQueueMode::Mpsc>, ESPMode::ThreadSafe> Queue = DecodeQueue;
	IImageWrapperModule* Module = ImageWrapperModule;
	TSharedRef<FLogiTexPipeStats, ESPMode::ThreadSafe> Stats = TexStats;
	Async(EAsyncExecution::ThreadPool, [Key, Bytes = MoveTemp(Bytes), Queue, Module, Stats]()
	{
		FLogiTileDecode Out;
		Out.Key = Key;
		DecodeJpegToBgraMips(Module, Bytes.GetData(), Bytes.Num(), Out.W, Out.H, Out.Mips); // Mips empty on failure
		(Out.Mips.Num() > 0 ? Stats->DecodeJobOk : Stats->DecodeJobEmpty).fetch_add(1, std::memory_order_relaxed);
		Queue->Enqueue(MoveTemp(Out)); // always enqueue so the game thread clears the in-flight key
	});
}

void ALogiBakedGeoTerrain::DrainDecodedTextures(double DeadlineSec)
{
	// FIRST: confirm-or-retry the deferred uploads parked in prior steps — bind the atlases whose bytes are now
	// proven GPU-resident (and retry/recycle the rest). Runs every step (including camera-still), so a tile's atlas
	// lands the step after its upload completes.
	ProcessPendingTextureBinds();

	if (!DecodeQueue.IsValid()) { return; }
	const int32 UploadBudget = FMath::Max(1, GlobeTextureUploadsPerStep);
	int32 UploadsThisStep = 0;
	FLogiTileDecode Done;
	while (DecodeQueue->Dequeue(Done))
	{
		DecodingKeys.Remove(Done.Key);
		if (Done.Mips.Num() == 0) { continue; }                          // decode failed — tile stays untextured (re-driven)
		FTileEntry* Entry = Tiles.Find(Done.Key);
		if (Entry == nullptr || !Entry->ProcComp.IsValid())
		{
			TexStats->DrainDropEvicted.fetch_add(1, std::memory_order_relaxed);
			continue; // evicted before the decode finished
		}
		if (Entry->Texture.IsValid() || AwaitingTextureConfirm.Contains(Done.Key))
		{
			TexStats->DrainDropDup.fetch_add(1, std::memory_order_relaxed);
			continue; // already textured / a bind is pending
		}
		// Decode SUCCEEDED (non-empty mips). Clear the retry counter: GMaxTextureAttempts is there to retire a
		// genuinely-undecodable atlas (decode keeps returning EMPTY, caught at the `Mips.Num()==0` continue above) —
		// it must NOT accrue on upload-pressure cycles (decode-OK, then a dropped GPU upload under a LockTexture2D
		// burst). Without this reset, a perfectly decodable tile in a sustained upload flood burns the cap one
		// re-decode at a time and strands on the placeholder. Resetting here means only true decode failures count.
		Entry->TextureAttempts = 0;
		// Move the worker-produced BC7 chain into a thread-safe shared payload (no copy) so AcquireTexture can hand
		// it to the render thread, which owns the upload + frees the bytes when done (audit T4 lifetime discipline).
		TSharedRef<FLogiTileMipChain, ESPMode::ThreadSafe> Chain = MakeShared<FLogiTileMipChain, ESPMode::ThreadSafe>();
		Chain->W = Done.W;
		Chain->H = Done.H;
		Chain->Mips = MoveTemp(Done.Mips);
		// AcquireTexture creates or reuses the UTexture2D and ENQUEUES the GPU upload, but we do NOT bind it here:
		// the bind waits until the upload is CONFIRMED resident (ProcessPendingTextureBinds), so a dropped upload is
		// never shown as garbage, black, or another tile's atlas — the tile keeps its honest gray placeholder until
		// the real bytes land. Park the texture + its mip chain (which carries the upload's atomic result flag).
		UTexture2D* Tex = AcquireTexture(Chain);
		if (Tex == nullptr) { continue; }
		FPendingTextureBind Bind;
		Bind.Key = Done.Key;
		Bind.Texture.Reset(Tex);
		Bind.Chain = Chain;
		PendingBinds.Add(MoveTemp(Bind));
		AwaitingTextureConfirm.Add(Done.Key);
		// Per-STEP upload budget: stop enqueuing once we hit the cap, so a region-entry burst can't flood the D3D12
		// upload heap and cause the correlated LockTexture2D failures behind the corruption band. The surplus
		// decodes stay queued for the next step.
		if (++UploadsThisStep >= UploadBudget) { break; }
		// Per-frame elapsed-time budget: bail once we cross the deadline (after at least one this step) so a tiny
		// budget still makes forward progress.
		if (DeadlineSec > 0.0 && FPlatformTime::Seconds() >= DeadlineSec) { break; }
	}
}

// Confirm-or-retry the deferred atlas uploads from prior steps. The decode path parks each acquired texture in
// PendingBinds with its render-thread upload in flight; here — once per step, BEFORE draining new decodes — we read
// each upload's atomic result and only THEN bind the atlas to its tile. This is the gate that stops a dropped upload
// (LockTexture2D failing under a burst, a short mip, etc.) from being shown as black or another tile's atlas: until
// the bytes are proven resident the tile keeps its honest gray placeholder, and a failed upload is retried or recycled.
void ALogiBakedGeoTerrain::ProcessPendingTextureBinds()
{
	if (PendingBinds.Num() == 0) { return; }
	constexpr uint8 MaxUploadRetries = 3;
	for (int32 i = PendingBinds.Num() - 1; i >= 0; --i)
	{
		FPendingTextureBind& B = PendingBinds[i];
		const int32 State = B.Chain.IsValid() ? B.Chain->UploadState.load(std::memory_order_acquire) : -1;
		if (State == 0) { continue; } // the upload command hasn't completed yet — re-check next step

		const uint64 Key = B.Key;
		FTileEntry* Entry = Tiles.Find(Key);
		if (Entry == nullptr || !Entry->ProcComp.IsValid())
		{
			// Tile evicted while its upload was in flight: return the texture to the pool and drop the bind.
			if (UTexture2D* Tex = B.Texture.Get())
			{
				const FTexturePlatformData* PD = Tex->GetPlatformData();
				ReturnTextureToPool((PD != nullptr) ? TilePoolKey(PD->SizeX, PD->SizeY) : 0u, MoveTemp(B.Texture));
			}
			AwaitingTextureConfirm.Remove(Key);
			PendingBinds.RemoveAtSwap(i, EAllowShrinking::No);
			continue;
		}

		if (State == 1)
		{
			// CONFIRMED resident — now it is safe to show. Claim the texture + point the tile's MID at the atlas.
			TexStats->UploadConfirmed.fetch_add(1, std::memory_order_relaxed);
			UTexture2D* Tex = B.Texture.Get();
			Entry->Texture = MoveTemp(B.Texture);
			PendingTexture.Remove(Key); // atlas is up — stop the bounded re-drive tracking it
			// Reuse the build-time MID (the gray placeholder); only acquire one if the base hadn't resolved at build.
			UMaterialInstanceDynamic* Mid = Entry->Material.Get();
			if (Mid == nullptr)
			{
				if (UMaterialInterface* Base = GlobeBaseMaterial.Get())
				{
					Mid = AcquireMid(Base);
					if (Mid != nullptr)
					{
						Entry->Material.Reset(Mid);
						if (!IsDebugViewActive()) { Entry->ProcComp->SetTileMaterial(Mid); } // recreate-on-swap (see SetTileMaterial)
					}
				}
			}
			if (Mid != nullptr) { Mid->SetTextureParameterValue(TEXT("BaseColorTex"), Tex); }
			// Atlas resident + bound ⇒ the tile is texture-READY: show it (it was built HIDDEN as a "prepare" tile),
			// start the focus-pull reveal, and report it to the Rust texture gate so its held coarse parent can drop.
			MarkTileTextured(Key, *Entry);
			AwaitingTextureConfirm.Remove(Key);
			PendingBinds.RemoveAtSwap(i, EAllowShrinking::No);
			continue;
		}

		// State == -1: a mip was dropped (LockTexture2D failed under pressure, or a short/empty mip). The decode +
		// texture are fine — only the GPU copy failed — so RETRY just the upload a bounded number of times.
		if (B.UploadRetries < MaxUploadRetries && B.Texture.IsValid() && B.Chain.IsValid())
		{
			++B.UploadRetries;
			TexStats->UploadRetried.fetch_add(1, std::memory_order_relaxed);
			B.Chain->UploadState.store(0, std::memory_order_release);
			EnqueueTileMipUploadRT(B.Texture.Get(), B.Chain.ToSharedRef(), TexStats);
			continue; // still pending; re-checked next step
		}
		// Gave up on THIS upload after the retry cap: return the texture to the pool but leave the tile HIDDEN — do
		// NOT surrender it onto the placeholder. A dropped upload is heap PRESSURE (a LockTexture2D burst during a
		// region-entry flood), which is TRANSIENT, and the decode itself succeeded. Reporting it textured here would
		// un-hide it on the null-texture placeholder (a flat patch beside sharp neighbours) AND drop its real-textured
		// coarse parent — the worse of both. Instead keep it hidden under that coarse parent and keep its key in
		// PendingTexture: DriveTileTextures re-drives it (a successful decode resets TextureAttempts, so upload
		// pressure never burns the cap) until the upload lands for real. Only a genuinely-undecodable atlas — decode
		// keeps failing — reaches the cap and surrenders, and that happens in DriveTileTextures, not here.
		TexStats->UploadSurrendered.fetch_add(1, std::memory_order_relaxed);
		if (UTexture2D* Tex = B.Texture.Get())
		{
			const FTexturePlatformData* PD = Tex->GetPlatformData();
			ReturnTextureToPool((PD != nullptr) ? TilePoolKey(PD->SizeX, PD->SizeY) : 0u, MoveTemp(B.Texture));
		}
		AwaitingTextureConfirm.Remove(Key);
		PendingBinds.RemoveAtSwap(i, EAllowShrinking::No);
	}
}
#endif // LOGI_BEVY_HAVE_GEO

// === Debug visualization + telemetry =====================================================================
// In-editor tooling over the Rust-streamed tiles: white→red "quality" heatmaps (real textures suppressed)
// that switch live WITHOUT rebuilding the world, plus a live telemetry overlay + Details readout. The
// heatmap material is built at runtime (editor / PIE only — it needs the shader compiler); outside the
// editor these helpers no-op and tiles render normally. None of this is FFI-gated (it's pure engine), so it
// lives outside the LOGI_BEVY_HAVE_GEO block above.

namespace
{
	// Short label for the telemetry status line.
	const TCHAR* GeoDebugViewName(EGeoDebugView V)
	{
		switch (V)
		{
		case EGeoDebugView::RefinementDepth:  return TEXT("Refinement Depth");
		case EGeoDebugView::GeometricError:   return TEXT("Geometric Error");
		case EGeoDebugView::StreamingState:   return TEXT("Streaming State");
		case EGeoDebugView::ScreenSpaceError: return TEXT("Screen-Space Error");
		case EGeoDebugView::None:
		default:                              return TEXT("None");
		}
	}

	// A compact 5-stop Turbo-ish ramp (blue→cyan→green→yellow→red) for the richer palette option.
	FLinearColor GeoTurboColor(float T)
	{
		static const FLinearColor Stops[5] = {
			FLinearColor(0.0f, 0.0f, 1.0f), FLinearColor(0.0f, 1.0f, 1.0f), FLinearColor(0.0f, 1.0f, 0.0f),
			FLinearColor(1.0f, 1.0f, 0.0f), FLinearColor(1.0f, 0.0f, 0.0f) };
		T = FMath::Clamp(T, 0.0f, 1.0f) * 4.0f;
		const int32 i = FMath::Clamp(static_cast<int32>(T), 0, 3);
		return FMath::Lerp(Stops[i], Stops[i + 1], T - static_cast<float>(i));
	}
}

bool ALogiBakedGeoTerrain::IsDebugViewActive()
{
	// Short-circuits on None so we never build the material just to answer "is a debug view on?".
	return DebugView != EGeoDebugView::None && GetOrCreateDebugMaterial() != nullptr;
}

UMaterialInterface* ALogiBakedGeoTerrain::GetOrCreateDebugMaterial()
{
#if WITH_EDITOR
	if (UMaterialInterface* Existing = DebugMaterial.Get())
	{
		return Existing;
	}
	UMaterial* Mat = NewObject<UMaterial>(this, NAME_None, RF_Transient);
	if (Mat == nullptr)
	{
		return nullptr;
	}
	// Unlit, two-sided, opaque: a flat SELF-LIT colour with no texture sampling — exactly the "show quality,
	// not imagery" requirement. The colour is a named vector parameter each per-tile MID overrides.
	Mat->MaterialDomain = MD_Surface;
	Mat->SetShadingModel(MSM_Unlit);
	Mat->BlendMode = BLEND_Opaque;
	Mat->TwoSided = true;
	UMaterialExpressionVectorParameter* Col = NewObject<UMaterialExpressionVectorParameter>(Mat);
	Col->ParameterName = TEXT("DebugColor");
	Col->DefaultValue = FLinearColor::White;
	Mat->GetExpressionCollection().AddExpression(Col);
	Mat->GetEditorOnlyData()->EmissiveColor.Connect(0, Col);
	Mat->PostEditChange(); // compile (async in-editor; tiles briefly show the default until it finishes)
	DebugMaterial.Reset(Mat);
	return Mat;
#else
	return nullptr; // runtime material compilation needs the editor; debug viz is editor / PIE only
#endif
}

#if WITH_EDITOR
UMaterialInterface* ALogiBakedGeoTerrain::GetOrCreateGlobeClipMaterial()
{
	if (UMaterialInterface* Existing = GlobeClipMaterial.Get())
	{
		return Existing;
	}
	UMaterial* Mat = NewObject<UMaterial>(this, NAME_None, RF_Transient);
	if (Mat == nullptr)
	{
		return nullptr;
	}
	// Unlit + MASKED: emissive = the atlas (P3DT bakes its own lighting), OpacityMask = the hole-clip test. A
	// masked surface writes scene depth ONLY where the mask passes — so a clipped fragment leaves depth untouched
	// (the finer tile behind shows through) and SkyAtmosphere reads a clean depth, exactly what the PDO could not
	// give. NOTE: the clip OBBs are in render-space world cm (the actor sits at the world origin, the ENU origin),
	// so the test uses Absolute World Position — the same space the tile mesh vertices live in.
	Mat->MaterialDomain = MD_Surface;
	Mat->SetShadingModel(MSM_Unlit);
	Mat->BlendMode = BLEND_Masked;
	Mat->TwoSided = true;
	UMaterialEditorOnlyData* Ed = Mat->GetEditorOnlyData();

	// Emissive = BaseColorTex.RGB — the per-tile MID binds the atlas, exactly as the opaque unlit base displays it.
	UMaterialExpressionTextureSampleParameter2D* Tex = NewObject<UMaterialExpressionTextureSampleParameter2D>(Mat);
	Tex->ParameterName = TEXT("BaseColorTex");
	Mat->GetExpressionCollection().AddExpression(Tex);
	Ed->EmissiveColor.Connect(0, Tex);

	UMaterialExpressionWorldPosition* WorldPos = NewObject<UMaterialExpressionWorldPosition>(Mat);
	Mat->GetExpressionCollection().AddExpression(WorldPos);

	UMaterialExpressionScalarParameter* Count = NewObject<UMaterialExpressionScalarParameter>(Mat);
	Count->ParameterName = TEXT("ClipBoxCount");
	Count->DefaultValue = 0.0f;
	Mat->GetExpressionCollection().AddExpression(Count);

	// OpacityMask = 0 (discard) when the world position is inside ANY clip OBB, else 1. Each box is center + 3
	// half-extent axis vectors; inside iff |dot(p-center, axisK)| <= dot(axisK, axisK) for all three axes K. The
	// loop is bounded by ClipBoxCount (0 ⇒ always 1 ⇒ fully opaque), so a tile with no clip set renders normally.
	UMaterialExpressionCustom* Clip = NewObject<UMaterialExpressionCustom>(Mat);
	Clip->OutputType = CMOT_Float1;
	Clip->Description = TEXT("HoleClipMask");
	Clip->Code = TEXT(
		"float3 C[8]  = { C0.xyz,  C1.xyz,  C2.xyz,  C3.xyz,  C4.xyz,  C5.xyz,  C6.xyz,  C7.xyz  };\n"
		"float3 AX[8] = { AX0.xyz, AX1.xyz, AX2.xyz, AX3.xyz, AX4.xyz, AX5.xyz, AX6.xyz, AX7.xyz };\n"
		"float3 AY[8] = { AY0.xyz, AY1.xyz, AY2.xyz, AY3.xyz, AY4.xyz, AY5.xyz, AY6.xyz, AY7.xyz };\n"
		"float3 AZ[8] = { AZ0.xyz, AZ1.xyz, AZ2.xyz, AZ3.xyz, AZ4.xyz, AZ5.xyz, AZ6.xyz, AZ7.xyz };\n"
		"float m = 1.0;\n"
		"int n = (int)round(Count);\n"
		"[unroll] for (int i = 0; i < 8; ++i)\n"
		"{\n"
		"    if (i >= n) break;\n"
		"    float3 d = WorldPos - C[i];\n"
		"    bool inside = abs(dot(d, AX[i])) <= dot(AX[i], AX[i])\n"
		"               && abs(dot(d, AY[i])) <= dot(AY[i], AY[i])\n"
		"               && abs(dot(d, AZ[i])) <= dot(AZ[i], AZ[i]);\n"
		"    if (inside) m = 0.0;\n"
		"}\n"
		"return m;");

	// Wire the Custom node inputs: WorldPos + ClipBoxCount + the LOGI_CLIP_MAX×4 box-data vector parameters the
	// per-tile MID sets (ClipBox{i}_{Center,AxisX,AxisY,AxisZ}); HLSL names are WorldPos / Count / C{i} / A{X,Y,Z}{i}.
	Clip->Inputs.Empty();
	auto AddExprInput = [&](const TCHAR* VarName, UMaterialExpression* Expr)
	{
		FCustomInput In;
		In.InputName = FName(VarName);
		In.Input.Connect(0, Expr);
		Clip->Inputs.Add(In);
	};
	auto AddVecInput = [&](const FString& VarName, const FString& ParamName)
	{
		UMaterialExpressionVectorParameter* V = NewObject<UMaterialExpressionVectorParameter>(Mat);
		V->ParameterName = FName(*ParamName);
		V->DefaultValue = FLinearColor(0.f, 0.f, 0.f, 0.f);
		Mat->GetExpressionCollection().AddExpression(V);
		AddExprInput(*VarName, V);
	};
	AddExprInput(TEXT("WorldPos"), WorldPos);
	AddExprInput(TEXT("Count"), Count);
	for (uint32 i = 0; i < LOGI_CLIP_MAX; ++i)
	{
		AddVecInput(FString::Printf(TEXT("C%u"), i), FString::Printf(TEXT("ClipBox%u_Center"), i));
		AddVecInput(FString::Printf(TEXT("AX%u"), i), FString::Printf(TEXT("ClipBox%u_AxisX"), i));
		AddVecInput(FString::Printf(TEXT("AY%u"), i), FString::Printf(TEXT("ClipBox%u_AxisY"), i));
		AddVecInput(FString::Printf(TEXT("AZ%u"), i), FString::Printf(TEXT("ClipBox%u_AxisZ"), i));
	}
	Mat->GetExpressionCollection().AddExpression(Clip);
	Ed->OpacityMask.Connect(0, Clip);

	Mat->PostEditChange(); // compile (async in-editor; tiles briefly render unclipped until it finishes)
	GlobeClipMaterial.Reset(Mat);
	return Mat;
}
#endif

UMaterialInstanceDynamic* ALogiBakedGeoTerrain::AcquireDebugMid()
{
	while (DebugMidPool.Num() > 0)
	{
		TStrongObjectPtr<UMaterialInstanceDynamic> Pooled = DebugMidPool.Pop(EAllowShrinking::No);
		if (UMaterialInstanceDynamic* Mid = Pooled.Get())
		{
			return Mid;
		}
	}
	UMaterialInterface* Base = GetOrCreateDebugMaterial();
	return (Base != nullptr) ? UMaterialInstanceDynamic::Create(Base, this) : nullptr;
}

UMeshComponent* ALogiBakedGeoTerrain::TileComponent(const FTileEntry& Entry) const
{
	if (ULogiTileMeshComponent* Comp = Entry.ProcComp.Get())
	{
		return Comp;
	}
	return Entry.NaniteComp.Get(); // Nanite lane (UStaticMeshComponent) — both derive from UMeshComponent
}

UMaterialInterface* ALogiBakedGeoTerrain::RealMaterialForTile(const FTileEntry& Entry) const
{
	// Globe tiles own a per-tile textured/placeholder MID; baked/procedural/Nanite tiles share TerrainMaterial.
	return Entry.Material.IsValid() ? static_cast<UMaterialInterface*>(Entry.Material.Get()) : TerrainMaterial.Get();
}

FLinearColor ALogiBakedGeoTerrain::DebugColorForTile(const FTileEntry& Entry, const FLogiStreamView& View) const
{
	float Q = 0.0f; // 0 = white (low quality), 1 = red (high quality)
	switch (DebugView)
	{
	case EGeoDebugView::RefinementDepth:
	{
		// red = deepest. A fixed scale if DebugMaxDepth is set, else the in-view [min, max] spread.
		const float MaxD = (DebugMaxDepth > 0) ? static_cast<float>(DebugMaxDepth)
		                                       : static_cast<float>(FMath::Max(DebugDepthMax, static_cast<uint16>(1)));
		const float MinD = (DebugMaxDepth > 0) ? 0.0f : static_cast<float>(DebugDepthMin);
		Q = (MaxD > MinD) ? (static_cast<float>(Entry.Depth) - MinD) / (MaxD - MinD) : 0.0f;
		break;
	}
	case EGeoDebugView::GeometricError:
	{
		// red = finest (lowest error). Log scale over the in-view [min, max] error range.
		const float Lo = FMath::Max(DebugGeoErrMin, KINDA_SMALL_NUMBER);
		const float Hi = FMath::Max(DebugGeoErrMax, Lo * 1.001f);
		const float Ge = FMath::Clamp(Entry.GeometricError, Lo, Hi);
		const float T = (FMath::Loge(Ge) - FMath::Loge(Lo)) / (FMath::Loge(Hi) - FMath::Loge(Lo)); // 0 fine..1 coarse
		Q = 1.0f - T;
		break;
	}
	case EGeoDebugView::StreamingState:
	{
		// red = full-res atlas resident; partial while warming through the decode attempts; white = none.
		if (Entry.Texture.IsValid())        { Q = 1.0f; }
		else if (Entry.TextureAttempts > 0) { Q = FMath::Min(0.85f, 0.2f * static_cast<float>(Entry.TextureAttempts)); }
		else                                { Q = 0.0f; }
		break;
	}
	case EGeoDebugView::ScreenSpaceError:
	{
		// Standard 3D Tiles SSE (px) = geometricError(cm) * viewportH / (2·dist(cm)·tan(fovY/2)); red = at/over
		// target. Distance is the camera → the tile's (world-cm) centre; geometric error is metres, so ×100 to cm.
		const double DistCm = FMath::Max((View.Loc - FVector(Entry.BoundsCenter)).Size(), 1.0);
		const double TanHalfFov = FMath::Max(FMath::Tan(static_cast<double>(View.FovY) * 0.5), 1e-4);
		const float Sse = static_cast<float>((static_cast<double>(Entry.GeometricError) * 100.0)
			* static_cast<double>(View.ViewportHeight) / (2.0 * DistCm * TanHalfFov));
		Q = FMath::Clamp(Sse / FMath::Max(DetailTargetPx, 1.0f), 0.0f, 1.0f);
		break;
	}
	case EGeoDebugView::None:
	default:
		break;
	}
	Q = FMath::Clamp(Q, 0.0f, 1.0f);
	return (DebugPalette == EGeoDebugPalette::Turbo) ? GeoTurboColor(Q) : FLinearColor(1.0f, 1.0f - Q, 1.0f - Q, 1.0f);
}

void ALogiBakedGeoTerrain::UpdateTileDebugDisplay(const FLogiStreamView& View)
{
	if (!IsDebugViewActive())
	{
		// View just turned off (or was never on): restore real materials on every tile once, then idle. When no
		// view has ever been active this is a single cheap branch per step — zero overhead for normal rendering.
		if (ActiveDebugView != EGeoDebugView::None)
		{
			for (TPair<uint64, FTileEntry>& Pair : Tiles)
			{
				RestoreTileMaterial(Pair.Value);
			}
			ActiveDebugView = EGeoDebugView::None;
		}
		return;
	}
	// A view is active. Bind each tile's debug MID (only when it actually changes — so the geometry re-upload a
	// SetMaterial triggers happens once per tile on entry, not every step) and refresh its colour (a cheap
	// parameter update — no proxy churn — so steady-state recolouring, incl. adaptive ranges, is essentially free).
	for (TPair<uint64, FTileEntry>& Pair : Tiles)
	{
		FTileEntry& E = Pair.Value;
		UMeshComponent* Comp = TileComponent(E);
		if (Comp == nullptr)
		{
			continue;
		}
		UMaterialInstanceDynamic* Mid = E.DebugMid.Get();
		if (Mid == nullptr)
		{
			Mid = AcquireDebugMid();
			if (Mid == nullptr)
			{
				continue; // no debug material (e.g. non-editor build) — leave the tile as-is
			}
			E.DebugMid.Reset(Mid);
		}
		if (Comp->GetMaterial(0) != Mid)
		{
			// In-place swap for tile components (C4c — a debug-view toggle re-bound EVERY tile, each a full
			// proxy recreate + re-upload); the Nanite static-mesh lane keeps the engine path.
			if (ULogiTileMeshComponent* TileComp = Cast<ULogiTileMeshComponent>(Comp))
			{
				TileComp->SetTileMaterial(Mid);
			}
			else
			{
				Comp->SetMaterial(0, Mid);
			}
		}
		Mid->SetVectorParameterValue(TEXT("DebugColor"), DebugColorForTile(E, View));
	}
	ActiveDebugView = DebugView;
}

void ALogiBakedGeoTerrain::RestoreTileMaterial(FTileEntry& Entry)
{
	if (UMeshComponent* Comp = TileComponent(Entry))
	{
		UMaterialInterface* Real = RealMaterialForTile(Entry);
		if (Comp->GetMaterial(0) != Real)
		{
			if (ULogiTileMeshComponent* TileComp = Cast<ULogiTileMeshComponent>(Comp))
			{
				TileComp->SetTileMaterial(Real); // recreate-on-swap (see SetTileMaterial)
			}
			else
			{
				Comp->SetMaterial(0, Real);
			}
		}
	}
	if (Entry.DebugMid.IsValid())
	{
		DebugMidPool.Add(MoveTemp(Entry.DebugMid));
	}
}

void ALogiBakedGeoTerrain::UpdateDebugTelemetry(const TCHAR* SourceLabel, int32 Rendered, int32 Resident,
	int32 InFlight, const FString& Failures, int32 CachedMB)
{
	// Gate the O(built) sweep (audit 2026-06-10 §2): with neither the overlay nor a debug view active, this
	// ran every step purely to refresh Details-panel fields — refresh those on a slow cadence instead (every
	// 30th step keeps the panel readable when the actor is selected), and every step only when consumed.
	if (!bShowDebugPanel && DebugView == EGeoDebugView::None && (HeartbeatCounter % 30u) != 0u)
	{
		return;
	}
	// One O(resident) pass: textured count + the depth & geometric-error spread the heatmaps normalize over.
	int32 Textured = 0;
	uint16 DepthLo = TNumericLimits<uint16>::Max();
	uint16 DepthHi = 0;
	float GeoLo = TNumericLimits<float>::Max();
	float GeoHi = 0.0f;
	for (const TPair<uint64, FTileEntry>& Pair : Tiles)
	{
		const FTileEntry& E = Pair.Value;
		if (E.Texture.IsValid())
		{
			++Textured;
		}
		DepthLo = FMath::Min(DepthLo, E.Depth);
		DepthHi = FMath::Max(DepthHi, E.Depth);
		if (E.GeometricError > 0.0f)
		{
			GeoLo = FMath::Min(GeoLo, E.GeometricError);
			GeoHi = FMath::Max(GeoHi, E.GeometricError);
		}
	}
	const int32 Built = Tiles.Num();
	DebugDepthMin = (Built > 0) ? DepthLo : 0;
	DebugDepthMax = DepthHi;
	DebugGeoErrMin = (GeoHi > 0.0f) ? GeoLo : 0.0f;
	DebugGeoErrMax = GeoHi;

	int32 PoolTex = 0;
	for (const TPair<uint32, TArray<TStrongObjectPtr<UTexture2D>>>& Bucket : TexturePool)
	{
		PoolTex += Bucket.Value.Num();
	}
	int32 PoolMids = 0;
	for (const TPair<TObjectKey<UMaterialInterface>, TArray<TStrongObjectPtr<UMaterialInstanceDynamic>>>& Bucket : MidPool)
	{
		PoolMids += Bucket.Value.Num();
	}

	DbgStatus = FString::Printf(TEXT("%s  |  View: %s"), SourceLabel, GeoDebugViewName(DebugView));
	DbgTilesRendered = Rendered;
	DbgTilesResident = Resident;
	DbgTilesBuilt = Built;
	DbgTilesInFlight = InFlight;
	DbgTilesDecoding = DecodingKeys.Num();
	DbgTilesTextured = Textured;
	DbgPools = FString::Printf(TEXT("%d / %d / %d"), ProcCompPool.Num(), PoolMids, PoolTex);
	DbgGeoStepMs = FString::Printf(TEXT("%.2f / %.2f"), DbgGeoStepAvgMs, DbgGeoStepPeakMs);
	DbgDepthRange = (Built > 0) ? FString::Printf(TEXT("%u - %u"), DebugDepthMin, DebugDepthMax) : TEXT("-");
	DbgGeoErrorRange = (GeoHi > 0.0f) ? FString::Printf(TEXT("%.2f - %.1f m"), DebugGeoErrMin, DebugGeoErrMax) : TEXT("-");
	DbgFailures = Failures;
	DbgCachedMB = CachedMB;

	// --- Cache / pool / texture-pipeline subtrees (the diagnosis panel, 2026-06-11) ----------------------
	// One v16 telemetry-block read + a handful of Printfs — same cost class as the rows above.
	auto HitMiss = [](uint64 Hits, uint64 Misses) {
		const uint64 Total = Hits + Misses;
		return Total == 0
			? FString(TEXT("0 / 0"))
			: FString::Printf(TEXT("%llu / %llu (%.1f%%)"), (unsigned long long)Hits, (unsigned long long)Misses,
				100.0 * static_cast<double>(Hits) / static_cast<double>(Total));
	};
#if LOGI_BEVY_HAVE_GEO
	LogiGlobeTelemetry T = {};
	if (GlobeWorld != nullptr && logi_globe_world_telemetry(GlobeWorld, &T) == sizeof(T))
	{
		DbgFlashCache = FString::Printf(TEXT("%s | %llu MB"), *HitMiss(T.flash_hits, T.flash_misses),
			(unsigned long long)(T.flash_bytes / (1024 * 1024)));
		DbgDiskCache = FString::Printf(TEXT("%s | %llu MB"), *HitMiss(T.disk_hits, T.disk_misses),
			(unsigned long long)(T.disk_bytes / (1024 * 1024)));
		DbgPoolSlots = FString::Printf(TEXT("%llu / %d / %llu"), (unsigned long long)T.free_slots, Resident,
			(unsigned long long)T.max_resident);
		DbgPoolEvict = FString::Printf(TEXT("%llu / %llu"), (unsigned long long)T.evicted_total,
			(unsigned long long)T.pool_exhausted_total);
		DbgHolds = FString::Printf(TEXT("%llu / %llu / %llu / %llu / %llu / %llu"),
			(unsigned long long)T.hold_kick, (unsigned long long)T.hold_dead, (unsigned long long)T.hold_loading,
			(unsigned long long)T.hold_leaf_under_sse, (unsigned long long)T.hold_pending_subtree,
			(unsigned long long)T.fetch_skip_grafted);
	}
#endif
	DbgRetained = DepartedGrace.Num();
	DbgTexPipeline = FString::Printf(TEXT("%u / %u / %u"),
		TexStats->DecodeDispatched.load(std::memory_order_relaxed),
		TexStats->DecodeJobOk.load(std::memory_order_relaxed),
		TexStats->UploadConfirmed.load(std::memory_order_relaxed));
	DbgTexDrops = FString::Printf(TEXT("%u / %u"),
		TexStats->DrainDropEvicted.load(std::memory_order_relaxed),
		TexStats->DrainDropDup.load(std::memory_order_relaxed));
}

void ALogiBakedGeoTerrain::RecordGeoStepMs(double Ms)
{
	GeoStepSumMs += Ms;
	GeoStepWindowMaxMs = FMath::Max(GeoStepWindowMaxMs, Ms);
	if (++GeoStepN >= 30)
	{
		DbgGeoStepAvgMs = GeoStepSumMs / static_cast<double>(GeoStepN);
		DbgGeoStepPeakMs = GeoStepWindowMaxMs;
		UE_LOG(LogLogiBevy, Log, TEXT("Geo step game-thread: avg %.2f ms, max %.2f ms over %d steps (built=%d)"),
			DbgGeoStepAvgMs, DbgGeoStepPeakMs, GeoStepN, Tiles.Num());
		GeoStepSumMs = 0.0;
		GeoStepWindowMaxMs = 0.0;
		GeoStepN = 0;
	}
}

void ALogiBakedGeoTerrain::SyncDebugPanelRegistration()
{
	if (bShowDebugPanel && !DebugPanelHandle.IsValid())
	{
		DebugPanelHandle = UDebugDrawService::Register(TEXT("Game"),
			FDebugDrawDelegate::CreateUObject(this, &ALogiBakedGeoTerrain::DrawDebugPanel));
	}
	else if (!bShowDebugPanel && DebugPanelHandle.IsValid())
	{
		UDebugDrawService::Unregister(DebugPanelHandle);
		DebugPanelHandle.Reset();
	}
}

void ALogiBakedGeoTerrain::DrawDebugPanel(UCanvas* Canvas, APlayerController* /*PC*/)
{
	if (!bShowDebugPanel || Canvas == nullptr)
	{
		return;
	}
#if WITH_EDITOR
	const FString Label = GetActorLabel();
#else
	const FString Label = GetName();
#endif
	TArray<FString> Lines;
	Lines.Reserve(8);
	Lines.Add(FString::Printf(TEXT("[Geo] %s"), *Label));
	Lines.Add(DbgStatus);
	Lines.Add(FString::Printf(TEXT("rendered %d   resident %d   built %d"), DbgTilesRendered, DbgTilesResident, DbgTilesBuilt));
	Lines.Add(FString::Printf(TEXT("in-flight %d   decoding %d   textured %d"), DbgTilesInFlight, DbgTilesDecoding, DbgTilesTextured));
	Lines.Add(FString::Printf(TEXT("pools(c/m/t) %s   disk %d MB"), *DbgPools, DbgCachedMB));
	Lines.Add(FString::Printf(TEXT("geo step(avg/max) %s ms   depth %s   gerr %s"), *DbgGeoStepMs, *DbgDepthRange, *DbgGeoErrorRange));
	Lines.Add(FString::Printf(TEXT("give-ups(fetch/decode/graft/url) %s"), *DbgFailures));

	UFont* Font = (GEngine != nullptr) ? GEngine->GetSmallFont() : nullptr;
	const float X = 64.0f;
	const float Y0 = 96.0f;
	const float LineH = 14.0f;
	const float BoxW = 560.0f;
	const float BoxH = LineH * static_cast<float>(Lines.Num()) + 14.0f;

	FCanvasTileItem Box(FVector2D(X - 10.0f, Y0 - 8.0f), FVector2D(BoxW, BoxH), FLinearColor(0.0f, 0.0f, 0.0f, 0.55f));
	Box.BlendMode = SE_BLEND_Translucent;
	Canvas->DrawItem(Box);

	float Y = Y0;
	for (const FString& L : Lines)
	{
		FCanvasTextItem Text(FVector2D(X, Y), FText::FromString(L), Font, FLinearColor::White);
		Text.EnableShadow(FLinearColor::Black);
		Canvas->DrawItem(Text);
		Y += LineH;
	}
}

void ALogiBakedGeoTerrain::Tick(float DeltaTime)
{
	Super::Tick(DeltaTime);
#if LOGI_BEVY_HAVE_GEO
	UWorld* World = GetWorld();
	if (World == nullptr)
	{
		return;
	}
	// Run the geo step EVERY frame, not on a ~30 Hz batch. The old throttle bunched all the build/upload work
	// into one frame every ~33 ms — a heavy frame surrounded by idle ones (the periodic stutter). Instead the
	// build loop now bounds itself by an elapsed-MS budget (`GeoBuildBudgetMs`), so each frame does a *little*
	// work and the load spreads smoothly, using idle frame time. The FFI poll/set_view are cheap and return
	// "no new frame" on frames the stream thread hasn't produced one, so per-frame stepping costs ~nothing idle.
	(void)DeltaTime;

	// Reconcile the live telemetry overlay with the actor's toggle (cheap; only acts when it changed). Done
	// before the world-create guard so the panel still shows (idle counters) while a world is failing to load.
	SyncDebugPanelRegistration();

	if (!EnsureWorld())
	{
		return;
	}
	EnsureRenderComponents();

	FLogiStreamView View;
	if (!LogiResolveStreamView(World, View))
	{
		return; // no viewport this tick
	}

	// Time the whole geo step (both lanes) for the live panel — replaces the former in-globe static timer.
	const double GeoStepT0 = FPlatformTime::Seconds();
	if (Source == ETerrainSource::GoogleTiles)
	{
		StepAndBuildGlobe(View);
	}
	else
	{
		StepAndBuildBaked(View);
	}
	RecordGeoStepMs((FPlatformTime::Seconds() - GeoStepT0) * 1000.0);
#else
	(void)DeltaTime;
	SyncDebugPanelRegistration(); // keep the overlay toggle responsive even without the Geo FFI compiled in
#endif
}

void ALogiBakedGeoTerrain::StepAndBuildBaked(const FLogiStreamView& View)
{
#if LOGI_BEVY_HAVE_GEO
	// SSE-LOD selection over the baked pyramid (coarse far, fine near). A single-level store renders all.
	const float Sse = FMath::Clamp(DetailTargetPx, 1.0f, 256.0f);
	int32 BuildBudget = FMath::Max(1, TileBuildsPerStep); // spread builds over steps (anti-spike)

	// The baked AOI is origin-local (a small square at the world origin), so the f32-cm camera the baked
	// select ABI takes has ample precision here — the planetary-precision f64 path is the globe lane.
	LogiBakedFrame Frame{};
	if (logi_baked_world_select(BakedWorld, static_cast<float>(View.Loc.X), static_cast<float>(View.Loc.Y),
			static_cast<float>(View.Loc.Z), View.FovY, View.ViewportHeight, Sse, &Frame) != LOGI_GEO_OK
		|| Frame.tiles == nullptr)
	{
		return; // no frame this step — leave resident tiles in place (no eviction)
	}

	ResidentScratch.Empty(Frame.tile_count);
	for (uint32 i = 0; i < Frame.tile_count; ++i)
	{
		const LogiBakedTile& T = Frame.tiles[i];
		const uint64 Key = T.tile_key;
		ResidentScratch.Add(Key);

		if (Tiles.Contains(Key))
		{
			continue; // already built and still resident (tracked via ResidentScratch)
		}
		if (T.vertex_count == 0 || T.vertices == nullptr)
		{
			continue;
		}
		if (BuildBudget <= 0)
		{
			continue; // over the per-step build budget — builds on a later step (stays selected in Rust)
		}
		--BuildBudget;

		FTileEntry Entry;
		Entry.Depth = static_cast<uint16>(T.depth);                   // refinement quality for the debug heatmaps
		Entry.GeometricError = T.geometric_error;
		Entry.BoundsCenter = FVector3f(T.bounds_center[0], T.bounds_center[1], T.bounds_center[2]);
#if LOGI_GEO_NANITE_LANE
		if (bUseNaniteLaneResolved)
		{
			UStaticMesh* SM = LogiBuildTileNaniteMesh(this, T.vertices, T.vertex_stride, T.vertices_len,
				T.vertex_count, T.indices, T.index_count, TerrainMaterial.Get(), TEXT("LogiBakedGeo"));
			if (SM == nullptr)
			{
				continue;
			}
			UStaticMeshComponent* Comp = NewObject<UStaticMeshComponent>(this, NAME_None, RF_Transient);
			Comp->SetupAttachment(RootComponent);
			Comp->SetAbsolute(true, true, true); // ignore actor transform; verts are absolute world cm
			Comp->SetMobility(EComponentMobility::Movable);
			Comp->SetCanEverAffectNavigation(false); // terrain render shell — keep tiles out of the nav octree (ADR-0007)
			Comp->RegisterComponent();
			Comp->SetRelativeLocation(FVector::ZeroVector); // verts are absolute UE cm (origin at world 0)
			Comp->SetStaticMesh(SM);
			if (UMaterialInterface* Mat = TerrainMaterial.Get())
			{
				Comp->SetMaterial(0, Mat);
			}
			Entry.NaniteComp.Reset(Comp);
		}
		else
#endif
		{
			// Per-tile component: handing it new geometry rebuilds THIS tile's proxy (O(1)), never the whole set.
			// The de-interleave of the 32-byte stream + the RHI upload run on the RENDER THREAD (audit T1); the
			// game thread here only does a flat byte copy + an O(1) proxy hand-off.
			ULogiTileMeshComponent* Comp = AcquireProcComp();
			Comp->SetTileGeometry(T.vertices, T.vertex_stride, T.vertices_len, T.vertex_count,
				T.indices, T.index_count,
				FVector3f(T.bounds_center[0], T.bounds_center[1], T.bounds_center[2]), T.bounds_radius);
			if (UMaterialInterface* Mat = TerrainMaterial.Get())
			{
				Comp->SetTileMaterial(Mat); // override-only — SetTileGeometry's pending recreate reads it (C4c)
			}
			Entry.ProcComp.Reset(Comp);
		}
		Tiles.Add(Key, MoveTemp(Entry));
	}

	EvictDeparted(ResidentScratch);

	// Live telemetry + debug heatmaps. The baked/procedural lane has no async streaming, so the in-flight /
	// failure / disk-cache counters are not applicable (0 / "-"); the depth + geometric-error ranges are
	// computed over the resident tiles inside UpdateDebugTelemetry.
	UpdateDebugTelemetry(Source == ETerrainSource::Procedural ? TEXT("Procedural") : TEXT("Baked Tiles"),
		static_cast<int32>(Frame.tile_count), static_cast<int32>(Frame.tile_count), /*InFlight=*/0,
		/*Failures=*/TEXT("-"), /*CachedMB=*/0);
	UpdateTileDebugDisplay(View);
#else
	(void)View;
#endif
}

void ALogiBakedGeoTerrain::StepAndBuildGlobe(const FLogiStreamView& View)
{
#if LOGI_BEVY_HAVE_GEO
	const double StepT0 = FPlatformTime::Seconds(); // whole-step geo game-thread cost; also the per-frame budget origin
	const double GeoDeadline = StepT0 + static_cast<double>(GeoBuildBudgetMs) * 0.001; // the whole geo slice
	// Guaranteed BUILD floor (audit D): cap the texture DRAIN at half the slice so a burst of finished atlas
	// decodes can't consume the whole budget and starve mesh builds to ZERO this frame — the tile BUILD is the
	// binding throughput constraint behind "fine tiles arrive but never reach the screen". The build phase
	// below still passes the FULL GeoDeadline, so when the drain finishes early the builds claim the remainder;
	// the floor only bites when the drain would otherwise overrun the whole slice.
	const double DrainDeadline = StepT0 + static_cast<double>(GeoBuildBudgetMs) * 0.001 * 0.5;

	// Apply any tile atlases finished by worker decodes since the last step. This only creates the UTexture2D +
	// assigns the tile's MID (O(1) each) — the JPEG decode + mip build already happened off the game thread.
	// Runs FIRST so already-decoded textures land promptly; it yields the moment it crosses the shared deadline.
	DrainDecodedTextures(DrainDeadline);

	// Advance the LOD cross-fade (bloom-in + held-tile recycle) EVERY step — including the camera-still frame-id skip
	// below — so in-flight fades always complete. dt from the real-time clock, clamped so a hitch can't snap a fade.
	const float FadeDt = (LastFadeStepSeconds > 0.0) ? static_cast<float>(FMath::Min(StepT0 - LastFadeStepSeconds, 0.05)) : 0.0f;
	LastFadeStepSeconds = StepT0;
	if (FadingKeys.Num() > 0) { TickTileFades(FadeDt); }

	// Drain the departed-grace cache EVERY step too (both reconcile paths + the frame-id skip): expired retained
	// tiles recycle here, bounded — the grace cache is self-draining, never a reconcile backlog.
	SweepRetainedTiles(StepT0);

	// Planetary scale: the globe step takes the f64 world-cm camera directly (no f32 cast). The C++ side clamps
	// the actor's Detail Target to 1–64 px here; Rust then uses this max_sse as-is (it passes it through untouched).
	const double MaxSsePixels = FMath::Clamp(static_cast<double>(DetailTargetPx), 1.0, 64.0);
	const int32 BuildBudget = FMath::Max(1, TileBuildsPerStep);
	const int32 EvictBudget = FMath::Max(1, TileEvictionsPerStep); // gates the O(Δ) delta path (delta evicts can't defer)

	// ADR-0012 poll-based FFI: push the newest camera to the Rust stream thread (which advances + decodes
	// + rebases OFF this game thread), then pull the latest published frame. Neither call blocks on
	// streaming — the heavy work no longer runs on the game thread. The build loop below already dedups by
	// tile key, so re-reading the same frame across steps only rebuilds genuinely new tiles.
	const double Aspect = static_cast<double>(View.ViewportWidth) / FMath::Max(static_cast<double>(View.ViewportHeight), 1.0);
	const LogiGlobeView GlobeView{
		View.Loc.X, View.Loc.Y, View.Loc.Z,
		View.Dir.X, View.Dir.Y, View.Dir.Z,
		View.Up.X, View.Up.Y, View.Up.Z,
		Aspect, static_cast<double>(View.FovY), static_cast<double>(View.ViewportHeight), MaxSsePixels};
	logi_globe_world_set_view(GlobeWorld, &GlobeView);
	// Planet-true sun: keep the DirectionalLight at the camera's zenith (no-op while parked / unanchored).
	UpdateSunZenith(View.Loc);
	// Report the tiles we've fully TEXTURED to the Rust texture gate (only when the set changed). The selector keeps a
	// tile's coarse parent rendered until the tile is in this set, so a refining tile never shows before it's textured
	// (invisible LOD loading). Pushed BEFORE poll so this step's selection sees the latest textures.
	if (bTexturedKeysDirty)
	{
		TexturedKeysScratch.Reset();
		TexturedKeysScratch.Reserve(TexturedKeys.Num());
		for (uint64 K : TexturedKeys) { TexturedKeysScratch.Add(K); }
		logi_globe_world_set_textured_keys(GlobeWorld, TexturedKeysScratch.GetData(), static_cast<uint32>(TexturedKeysScratch.Num()));
		bTexturedKeysDirty = false;
	}
	LogiGlobeFrame Frame{};
	if (logi_globe_world_poll(GlobeWorld, &Frame) != LOGI_GEO_OK || (Frame.tile_count > 0 && Frame.tiles == nullptr))
	{
		return; // nothing published yet — leave resident tiles in place (no eviction)
	}

	// One v16 telemetry-block read serves the failure log, the heartbeat, and the debug panel below (the
	// old per-counter accessors were deleted with ABI v16; the size return is the layout handshake).
	LogiGlobeTelemetry Telemetry = {};
	const bool bHaveTelemetry = logi_globe_world_telemetry(GlobeWorld, &Telemetry) == sizeof(Telemetry);

	// Diagnostic: surface WHY a tile stays stuck at a coarse (blurry) ancestor. Two distinct classes:
	//  - refinement couldn't DESCEND (graft_failed: a child tileset.json manifest didn't graft → its subtree
	//    never appears; url_resolve_failed: a content URI didn't resolve) — the tile is never even fetched, so
	//    no SSE target can sharpen it. These were SILENT before.
	//  - a tile we DID try to load gave up (fetch_failed: bytes never arrived; decode_failed: didn't parse).
	// Logged only when a count CHANGES, so it pinpoints the moment + kind without spamming the per-frame step.
	if (bHaveTelemetry)
	{
		// Packed into the v13-era change-detection members (two u64s) — only the comparison encoding, the
		// values themselves now come from the telemetry block.
		const uint64 Failed = ((Telemetry.decode_failed_total & 0xFFFFFFFFu) << 32) | (Telemetry.fetch_failed_total & 0xFFFFFFFFu);
		const uint64 Refine = ((Telemetry.graft_failed_total & 0xFFFFFFFFu) << 32) | (Telemetry.url_resolve_failed_total & 0xFFFFFFFFu);
		// Per-INSTANCE change detection (audit 2026-06-10 C8): function statics were shared across actor
		// instances and persisted across PIE sessions (cross-talk + spurious first-change logs).
		if (Failed != LastFailedCounts || Refine != LastRefineCounts)
		{
			UE_LOG(LogLogiBevy, Warning,
				TEXT("Geo refine/load failures: graft_failed=%llu (manifest didn't graft) url_resolve_failed=%llu | fetch_failed=%llu decode_failed=%llu | resident=%u in_flight=%u"),
				(unsigned long long)Telemetry.graft_failed_total, (unsigned long long)Telemetry.url_resolve_failed_total,
				(unsigned long long)Telemetry.fetch_failed_total, (unsigned long long)Telemetry.decode_failed_total,
				Frame.resident_tiles, Frame.in_flight);
			LastFailedCounts = Failed;
			LastRefineCounts = Refine;
		}
	}

	// Heartbeat (every 30 frames): the live streaming state. `render` = tiles in this published frame;
	// `resident` = decoded tiles Rust holds; `in_flight` = outstanding fetches RIGHT NOW (the "how many
	// requests are loading" number). Steady-high `in_flight` while the camera is still ⇒ throughput-bound
	// (raise fetch workers / in-flight cap); `in_flight` ~0 with a coarse scene ⇒ the selection settled
	// (under-refining — SSE/frustum, not loading). Also exposes the TRUE render count vs "textures built".
	{
		if (++HeartbeatCounter % 30u == 0u && bHaveTelemetry)
		{
			// Cache effectiveness rides the heartbeat (audit L1 — the counters existed with no consumer,
			// leaving "are we even using the cache?" unanswerable from a session log). Each is hits/misses.
			UE_LOG(LogLogiBevy, Log,
				TEXT("Geo stream: render=%u resident=%u in_flight=%u | mem hit/miss/mb=%llu/%llu/%llu disk hit/miss/mb=%llu/%llu/%llu")
				TEXT(" | pool free=%llu cap=%llu evicted=%llu starved=%llu | holds k/d/l/leaf/sub/graft=%llu/%llu/%llu/%llu/%llu/%llu | panics=%llu")
				TEXT(" | tex disp=%u ok=%u empty=%u conf=%u drop(evict/dup)=%u/%u fail(rhi/mip/short/lock)=%u/%u/%u/%u retry=%u recyc=%u"),
				Frame.tile_count, Frame.resident_tiles, Frame.in_flight,
				(unsigned long long)Telemetry.flash_hits, (unsigned long long)Telemetry.flash_misses,
				(unsigned long long)(Telemetry.flash_bytes / (1024 * 1024)),
				(unsigned long long)Telemetry.disk_hits, (unsigned long long)Telemetry.disk_misses,
				(unsigned long long)(Telemetry.disk_bytes / (1024 * 1024)),
				(unsigned long long)Telemetry.free_slots, (unsigned long long)Telemetry.max_resident,
				(unsigned long long)Telemetry.evicted_total, (unsigned long long)Telemetry.pool_exhausted_total,
				(unsigned long long)Telemetry.hold_kick, (unsigned long long)Telemetry.hold_dead,
				(unsigned long long)Telemetry.hold_loading, (unsigned long long)Telemetry.hold_leaf_under_sse,
				(unsigned long long)Telemetry.hold_pending_subtree, (unsigned long long)Telemetry.fetch_skip_grafted,
				(unsigned long long)Telemetry.step_panics,
				TexStats->DecodeDispatched.load(std::memory_order_relaxed), TexStats->DecodeJobOk.load(std::memory_order_relaxed),
				TexStats->DecodeJobEmpty.load(std::memory_order_relaxed), TexStats->UploadConfirmed.load(std::memory_order_relaxed),
				TexStats->DrainDropEvicted.load(std::memory_order_relaxed), TexStats->DrainDropDup.load(std::memory_order_relaxed),
				TexStats->UploadNoRhi.load(std::memory_order_relaxed), TexStats->UploadMipMismatch.load(std::memory_order_relaxed),
				TexStats->UploadShortMip.load(std::memory_order_relaxed), TexStats->UploadLockFailed.load(std::memory_order_relaxed),
				TexStats->UploadRetried.load(std::memory_order_relaxed), TexStats->UploadSurrendered.load(std::memory_order_relaxed));
		}
	}

	// ADR-0012 delta reconcile. The work this poll needs is read from the frame id + the per-poll delta:
	//  - frame_id UNCHANGED ⇒ the scene is identical to the last poll (camera still / streaming settled): skip the
	//    whole reconcile (O(1)) — unless a prior over-budget pass left a backlog, then drain it from the full list.
	//  - frame_id CHANGED, in sync, and the delta fits this frame's budgets ⇒ apply the O(Δ) delta (recycle
	//    removed[], build added[]). Deltas are never repeated, so we take this path ONLY when we can apply it FULLY.
	//  - otherwise (first frame / large burst / backlog) ⇒ re-scan the full visible list, which defers safely.
	const bool bChanged = (Frame.frame_id != LastFrameId);
	// Pure O(1) steady state = an unchanged frame that is already in sync; ANY other path reconciles, which also
	// means a coarse stand-in's clip set may have changed and needs a refresh (captured before the block mutates
	// bDeltaInSync).
	const bool bSteadyStateSkip = !bChanged && bDeltaInSync;
	if (!bChanged)
	{
		if (!bDeltaInSync)
		{
			ReconcileGlobeFull(Frame, BuildBudget, GeoDeadline); // drain a prior backlog; LastFrameId stays put
		}
		// else: identical scene, fully built — the O(1) steady-state path (nothing to do).
	}
	else
	{
		const bool bDeltaOk = bDeltaInSync
			&& Frame.added_count <= static_cast<uint32>(BuildBudget)
			&& Frame.removed_count <= static_cast<uint32>(EvictBudget);
		if (bDeltaOk)
		{
			ApplyGlobeDelta(Frame); // recycle removed[] + build ALL of added[] (bounded by the gate) — stays in sync
			bDeltaInSync = true;
		}
		else
		{
			ReconcileGlobeFull(Frame, BuildBudget, GeoDeadline); // first frame / burst / resync
		}
		LastFrameId = Frame.frame_id;
	}

	// Hole-clip: a coarse stand-in's clip set grows as its children texture in (RETAINED tiles the delta path never
	// revisits), so refresh every visible tile's clip params from the frame whenever the scene changed — skipped
	// entirely in the O(1) unchanged-frame steady state.
	if (!bSteadyStateSkip)
	{
		RefreshTileClips(Frame);
	}

	// Bounded atlas re-drive for built-but-untextured tiles (decode failures / not-yet-arrived). PendingTexture is
	// tiny in steady state; it reaches a tile by key via logi_globe_world_tile_by_key (the delta path never visits
	// already-resident tiles), so this never re-scans the frame.
	DriveTileTextures(BuildBudget);

	// Live telemetry + debug heatmaps. Pack the streaming give-up counters (already read into the v16 block
	// above) for the panel; the geo-step game-thread cost is now timed in Tick (RecordGeoStepMs), so this lane
	// no longer keeps its own static avg/max timer. The depth + geometric-error ranges are computed over the
	// resident tiles inside UpdateDebugTelemetry; UpdateTileDebugDisplay then (re)colours them if a view is on.
	{
		const FString Failures = FString::Printf(TEXT("%llu / %llu / %llu / %llu"),
			(unsigned long long)Telemetry.fetch_failed_total,
			(unsigned long long)Telemetry.decode_failed_total,
			(unsigned long long)Telemetry.graft_failed_total,
			(unsigned long long)Telemetry.url_resolve_failed_total);
		const int32 CachedMB = static_cast<int32>(Telemetry.disk_bytes / (1024 * 1024));
		UpdateDebugTelemetry(TEXT("Google Tiles"), static_cast<int32>(Frame.tile_count),
			static_cast<int32>(Frame.resident_tiles), static_cast<int32>(Frame.in_flight), Failures, CachedMB);
	}
	UpdateTileDebugDisplay(View);
#else
	(void)View;
#endif
}

#if LOGI_BEVY_HAVE_GEO
// Per-tile atlas-decode retry cap, shared by ReconcileGlobeFull (full-pass re-drive) and DriveTileTextures
// (steady-state re-drive) — both gate Entry->TextureAttempts against it, so it lives in one place to keep them
// in lock-step. Past this many dispatches a tile is shown soft / by surrender (a permanently-undecodable atlas).
static constexpr uint8 GMaxTextureAttempts = 4;

// A tile's atlas is bound (or it has none / its decode surrendered) ⇒ it is texture-READY. Show it (it was built
// HIDDEN as a "prepare" tile) and report it to the Rust texture gate via logi_globe_world_set_textured_keys so the
// selector lets it replace its coarse parent. The reveal is a focus-pull: FadeAlpha ramps 0→1 (soft→sharp mip bias)
// over GlobeFadeInSec, starting at ~the coarse parent's apparent detail so the LOD swap is imperceptible. Idempotent:
// re-marking a shown tile just re-confirms its key (no second reveal).
void ALogiBakedGeoTerrain::MarkTileTextured(uint64 Key, FTileEntry& Entry)
{
	// Visibility is NOT flipped here (atomic-reveal follow-up, 2026-06-10). This used to show the tile the
	// moment its atlas bound — a LOCAL lifecycle that matched the old partial-reveal selection (Rust dropped
	// or clipped the coarse parent as each child textured). Under the ATOMIC reveal, Rust keeps rendering the
	// kicked parent until the WHOLE sibling set is textured, so a locally-shown child sat UNDER the parent —
	// sustained coarse-over-fine z-fighting. Visibility now mirrors the published HIDDEN flag exclusively
	// (`ApplyTileVisibility`, driven by the Rust gate — ADR-0005: Rust owns the cut; this side only reports
	// readiness): this method just reports the texture so the gate can promote, and arms the reveal fade
	// (started when the tile actually SHOWS).
	(void)Entry;
	if (!TexturedKeys.Contains(Key))
	{
		TexturedKeys.Add(Key);
		bTexturedKeysDirty = true; // a poll-time push tells the gate this tile is ready (its coarse parent can drop)
	}
}

// Mirror one published tile's HIDDEN flag onto its component, arming the focus-pull reveal on the
// hidden->shown edge. The single home of globe-tile visibility: BuildGlobeTile (initial state),
// RefreshTileClips (retained tiles each changed frame — the delta path never revisits them), and
// ReconcileGlobeFull (full passes) all route here, so C++ can never show a tile the Rust gate hasn't
// promoted (and the parent->children swap lands make-before-break within one poll).
void ALogiBakedGeoTerrain::ApplyTileVisibility(uint64 Key, FTileEntry& Entry, uint32 TileFlags)
{
	const bool bShow = (TileFlags & LOGI_GLOBE_TILE_FLAG_HIDDEN) == 0;
	if (bShow && Entry.FadeMode == EGlobeFade::WaitTexture)
	{
		// First reveal of this build: start the focus-pull (soft -> sharp) AT THE SHOW, not at texture-bind —
		// under the atomic reveal a tile can sit textured-but-hidden while its siblings finish.
		if (bGlobeTileFade)
		{
			Entry.FadeMode = EGlobeFade::FadingIn;
			Entry.FadeT = 0.0f;
			FadingKeys.Add(Key);
			if (UMaterialInstanceDynamic* Mid = Entry.Material.Get())
			{
				Mid->SetScalarParameterValue(TEXT("FadeAlpha"), 0.0f);
			}
		}
		else
		{
			Entry.FadeMode = EGlobeFade::None;
		}
	}
	if (ULogiTileMeshComponent* Comp = Entry.ProcComp.Get())
	{
		Comp->SetTileVisible(bShow); // in-place (C4c); no-op when unchanged
	}
}

// Build ONE globe tile into a fresh pooled component + placeholder MID, and kick off its atlas decode. Shared by
// the delta (added[]) and full-reconcile paths; the caller guarantees Key is not already in `Tiles`. The geometry
// hand-off is a flat byte copy + an O(1) proxy hand-off — the de-interleave + RHI upload run on the RENDER THREAD.
void ALogiBakedGeoTerrain::BuildGlobeTile(const LogiGlobeTile& T)
{
	if (FTileEntry* Existing = Tiles.Find(T.tile_key))
	{
		// A retained (departed-grace) tile re-entering via the delta added[] path: still fully built — and, if it
		// kept its texture, still textured. Cancel its grace and re-show per the published flag; no rebuild, no
		// re-decode. (The full-reconcile path dedups before calling, so this guard is the delta path's.)
		DepartedGrace.Remove(T.tile_key);
		ApplyTileVisibility(T.tile_key, *Existing, T.flags);
		return;
	}
	if (T.vertex_count == 0 || T.vertices == nullptr)
	{
		return; // a contentless selected node — nothing to build; the streamer refines past it
	}
	ULogiTileMeshComponent* Comp = AcquireProcComp();
	Comp->SetTileGeometry(T.vertices, T.vertex_stride, T.vertices_len, T.vertex_count,
		T.indices, T.index_count,
		FVector3f(T.bounds_center[0], T.bounds_center[1], T.bounds_center[2]), T.bounds_radius);

	FTileEntry Entry;
	Entry.ProcComp.Reset(Comp);
	Entry.Depth = static_cast<uint16>(T.depth);                   // refinement quality for the debug heatmaps
	Entry.GeometricError = T.geometric_error;
	Entry.BoundsCenter = FVector3f(T.bounds_center[0], T.bounds_center[1], T.bounds_center[2]);

	// Assign a NEUTRAL placeholder material now, before the atlas lands: an unbound MID of the globe base material
	// draws that material's own flat colour rather than the engine's gray CHECKER default a material-less proxy
	// falls back to. BaseColorTex is cleared because a pooled MID may still carry a previous tile's atlas.
	// DrainDecodedTextures reuses THIS SAME MID. Base chosen by GlobeMaterialForTile (unlit vs lit).
	if (UMaterialInterface* Base = GlobeMaterialForTile(T.flags, /*bWantClip=*/false))
	{
		if (UMaterialInstanceDynamic* Mid = AcquireMid(Base))
		{
			Mid->SetTextureParameterValue(TEXT("BaseColorTex"), nullptr);
			// Geometric hole-clip (replaces the removed Pixel Depth Offset): the tile is built on the OPAQUE base
			// here; ApplyTileClip (below) swaps it to the MASKED clip variant and sets the per-box OBB params IF
			// this tile is a coarse stand-in over loaded finer tiles, so the renderer discards the coplanar overlap
			// instead of depth-fighting it (a PDO large enough to separate real terrain shifts SkyAtmosphere depth).
			Comp->SetTileMaterial(Mid); // override-only — SetTileGeometry's pending recreate reads it (C4c)
			Entry.Material.Reset(Mid);
		}
	}
	// Invisible LOD loading: a NEW tile is a "prepare" tile — the Rust texture gate is holding its coarse parent drawn
	// OVER this tile's footprint until we report it textured, so the swap never flashes gray or sky. Build it HIDDEN
	// and start FadeAlpha at 0 (soft); MarkTileTextured shows it + starts the focus-pull reveal once its atlas binds.
	// Fade off ⇒ FadeAlpha stays at the material default (1, sharp), and the tile still only shows once ready.
	Entry.FadeT = 0.0f;
	Entry.FadeMode = EGlobeFade::WaitTexture; // hidden until textured (or marked ready just below for a textureless tile)
	if (UMaterialInstanceDynamic* Mid = Entry.Material.Get())
	{
		Mid->SetScalarParameterValue(TEXT("FadeAlpha"), bGlobeTileFade ? 0.0f : 1.0f);
	}
	if (T.texture_jpeg_len > 0)
	{
		Entry.TextureAttempts = 1; // counts the dispatch below toward the per-tile retry cap
	}
	FTileEntry& Stored = Tiles.Add(T.tile_key, MoveTemp(Entry));
	// Initial visibility mirrors the published HIDDEN flag (usually hidden — a "prepare" tile the gate is
	// still covering with its coarse parent; visible on a resync re-add of an already-promoted tile).
	ApplyTileVisibility(T.tile_key, Stored, T.flags);
	ApplyTileClip(Stored, T); // swap to the masked clip variant + set OBB params if this tile is a coarse stand-in

	// Photoreal atlas: decode the JPEG + build the mip chain on a WORKER (off the game thread). Track the tile in
	// PendingTexture so DriveTileTextures can bound-retry it if that decode fails or is slow. A textureless tile has
	// nothing to decode ⇒ it is texture-ready NOW: show it + report it to the gate so its coarse parent can drop.
	if (T.texture_jpeg_len > 0)
	{
		DispatchTileDecode(T.tile_key, T.texture_jpeg, static_cast<int64>(T.texture_jpeg_len));
		PendingTexture.Add(T.tile_key);
	}
	else
	{
		MarkTileTextured(T.tile_key, Stored);
	}
}

// Geometric hole-clip: set this tile's clip OBB params on its MID, swapping the Opaque base <-> the MASKED clip
// base on the 0<->nonzero transition (preserving the bound atlas + fade). A cheap param-only update otherwise.
void ALogiBakedGeoTerrain::ApplyTileClip(FTileEntry& Entry, const LogiGlobeTile& T)
{
	const uint32 NewCount = FMath::Min<uint32>(T.clip_box_count, LOGI_CLIP_MAX);
	// Clip only tiles with a BOUND atlas: the masked clip MID samples BaseColorTex for its emissive, so
	// swapping an atlas-less tile (surrendered after the retry cap, or a rare textureless content) onto it
	// renders the material's flat default colour with wedge cutouts — the "flat light-blue clipped tiles"
	// in the 2026-06-10 ground screenshots. An unclipped stand-in (mild overdraw) beats a flat blue one.
	const bool bWantClip = NewCount > 0 && GlobeClipMaterial.IsValid() && Entry.Texture.IsValid();
	const bool bWasClip = Entry.ClipBoxCount > 0;

	// Opaque <-> Masked is a BASE-MATERIAL change (BlendMode is per-material, not a MID param), so it needs a fresh
	// MID over the other base — only on the 0<->nonzero edge, not when the clip set merely changes size. Preserve
	// the tile's bound atlas + current fade so the swap is invisible.
	if (bWantClip != bWasClip)
	{
		UMaterialInterface* Base = bWantClip ? GlobeClipMaterial.Get() : GlobeMaterialForTile(T.flags, /*bWantClip=*/false);
		if (UMaterialInstanceDynamic* NewMid = AcquireMid(Base))
		{
			NewMid->SetTextureParameterValue(TEXT("BaseColorTex"), Entry.Texture.Get());
			NewMid->SetScalarParameterValue(TEXT("FadeAlpha"), bGlobeTileFade ? Entry.FadeT : 1.0f);
			if (ULogiTileMeshComponent* Comp = Entry.ProcComp.Get())
			{
				// Base swap = proxy recreate (SetTileMaterial): a material POINTER change cannot apply in
				// place — the scene's cached static batch bakes the old material's render proxy in, and the
				// re-cache path never re-runs DrawStaticElements (engine-verified 5.8). Clip-edge transitions
				// are rare per tile; visibility flips (the common churn) stay in-place via SetTileVisible.
				Comp->SetTileMaterial(NewMid);
			}
			if (UMaterialInstanceDynamic* OldMid = Entry.Material.Get())
			{
				RecycleMid(TStrongObjectPtr<UMaterialInstanceDynamic>(OldMid)); // bucketed by parent (C3)
			}
			Entry.Material.Reset(NewMid);
		}
	}

	// Push the clip OBBs (no proxy churn) whenever clipping: center + 3 half-extent axes per box, in render
	// space. The masked material discards fragments inside any box; ClipBoxCount bounds its per-pixel loop.
	// Skipped when the applied set is BIT-IDENTICAL (the usual RefreshTileClips re-walk while a stand-in waits
	// for more children): 17 render-thread param updates per clipping tile per poll for nothing was the only
	// real cost left in this pass. CRC over the raw OBB bytes — tiny (≤192 B), and only for clipping tiles.
	const uint32 NewHash =
		bWantClip ? FCrc::MemCrc32(T.clip_boxes, NewCount * sizeof(LogiClipBox), NewCount) : 0;
	if (bWantClip && (bWantClip != bWasClip || NewHash != Entry.ClipHash))
	{
		if (UMaterialInstanceDynamic* Mid = Entry.Material.Get())
		{
			// Param names are stable per slot — build the FName table ONCE instead of a Printf + FName-registry
			// round trip per param per call (16 of them) on every clip update.
			struct FClipParamNames
			{
				FName Count;
				FName Center[LOGI_CLIP_MAX];
				FName AxisX[LOGI_CLIP_MAX];
				FName AxisY[LOGI_CLIP_MAX];
				FName AxisZ[LOGI_CLIP_MAX];
				FClipParamNames()
				{
					Count = FName(TEXT("ClipBoxCount"));
					for (uint32 i = 0; i < LOGI_CLIP_MAX; ++i)
					{
						Center[i] = FName(*FString::Printf(TEXT("ClipBox%u_Center"), i));
						AxisX[i] = FName(*FString::Printf(TEXT("ClipBox%u_AxisX"), i));
						AxisY[i] = FName(*FString::Printf(TEXT("ClipBox%u_AxisY"), i));
						AxisZ[i] = FName(*FString::Printf(TEXT("ClipBox%u_AxisZ"), i));
					}
				}
			};
			static const FClipParamNames Names;
			Mid->SetScalarParameterValue(Names.Count, static_cast<float>(NewCount));
			for (uint32 i = 0; i < NewCount; ++i)
			{
				const LogiClipBox& B = T.clip_boxes[i];
				Mid->SetVectorParameterValue(Names.Center[i], FLinearColor(B.center[0], B.center[1], B.center[2]));
				Mid->SetVectorParameterValue(Names.AxisX[i],
					FLinearColor(B.half_axes[0][0], B.half_axes[0][1], B.half_axes[0][2]));
				Mid->SetVectorParameterValue(Names.AxisY[i],
					FLinearColor(B.half_axes[1][0], B.half_axes[1][1], B.half_axes[1][2]));
				Mid->SetVectorParameterValue(Names.AxisZ[i],
					FLinearColor(B.half_axes[2][0], B.half_axes[2][1], B.half_axes[2][2]));
			}
		}
	}
	// Record what was APPLIED, not what was requested: with the clip material unset, recording the requested
	// count would leave bWasClip stuck true and re-run the base-swap branch (a fresh MID acquire) EVERY poll
	// for every would-clip tile.
	Entry.ClipBoxCount = bWantClip ? static_cast<uint8>(NewCount) : 0;
	Entry.ClipHash = NewHash;
}

// Refresh every visible tile's clip params from the frame: the delta path never revisits a RETAINED coarse
// stand-in whose clip set grows as its children texture in, so a per-poll pass keeps them current. Skipped in the
// O(1) unchanged-frame steady state (clip sets are identical there); a not-yet-built backlog tile is set when it
// builds; an already-recycled one is simply absent.
void ALogiBakedGeoTerrain::RefreshTileClips(const LogiGlobeFrame& Frame)
{
	for (uint32 i = 0; i < Frame.tile_count; ++i)
	{
		const LogiGlobeTile& T = Frame.tiles[i];
		if (FTileEntry* Entry = Tiles.Find(T.tile_key))
		{
			// Mirror the published HIDDEN flag (the Rust gate's promote/demote — the delta path never revisits
			// retained tiles, and a visibility flip always comes with a changed frame id, so this pass is the
			// flip's delivery). SetTileVisible no-ops when unchanged — steady state pays a flag compare.
			ApplyTileVisibility(T.tile_key, *Entry, T.flags);
			// Touch the MID only when this tile clips now or did last poll — the fully-refined common case is a
			// pure skip (0 ⇒ 0), so steady state pays nothing even on the rare frame this pass runs.
			if (T.clip_box_count > 0 || Entry->ClipBoxCount > 0)
			{
				ApplyTileClip(*Entry, T);
			}
		}
	}
}

// Retire ONE tile by key (the delta removed[] path): same departed-grace semantics as EvictDeparted — hide in
// place, retain component/texture/TexturedKeys until SweepRetainedTiles expires it (2026-06-10 oscillation fix;
// the delta removal not being repeated is fine — the sweep runs every step regardless of reconcile path).
void ALogiBakedGeoTerrain::RecycleByKey(uint64 Key)
{
	if (FTileEntry* Entry = Tiles.Find(Key))
	{
		if (!DepartedGrace.Contains(Key))
		{
			DepartedGrace.Add(Key, FPlatformTime::Seconds());
			if (ULogiTileMeshComponent* Comp = Entry->ProcComp.Get())
			{
				Comp->SetTileVisible(false); // in-place hide; stale coarse surface gone now, teardown later
			}
		}
		FadingKeys.Remove(Key);
		return;
	}
	// No entry (already swept, or never built): just clear the bookkeeping.
	FadingKeys.Remove(Key);
	PendingTexture.Remove(Key);
	if (TexturedKeys.Remove(Key) > 0) { bTexturedKeysDirty = true; }
}

// O(Δ) delta apply: recycle the departed tiles, then build the newly-selected ones. The caller gated the counts
// to this frame's budgets, so we apply the delta IN FULL — a deferred add/remove would be lost (deltas are never
// repeated). Each removal is an O(1) pool recycle; each add a flat copy + O(1) proxy hand-off.
void ALogiBakedGeoTerrain::ApplyGlobeDelta(const LogiGlobeFrame& Frame)
{
	for (uint32 i = 0; i < Frame.removed_count; ++i)
	{
		RecycleByKey(Frame.removed_keys[i]);
	}
	for (uint32 i = 0; i < Frame.added_count; ++i)
	{
		const uint32 Idx = Frame.added_indices[i];
		if (Idx < Frame.tile_count)
		{
			BuildGlobeTile(Frame.tiles[Idx]);
		}
	}
}

// Full reconcile (first frame / large burst / backlog resync): re-scan the whole visible list, build new tiles
// within budget, re-show + texture-re-drive existing ones, evict departed ones. Sets bDeltaInSync iff it applied
// EVERYTHING (no build deferred past the budget AND no eviction deferred) — only then is the O(Δ) delta path safe
// next frame; any deferral keeps the lane on this path until the backlog drains.
void ALogiBakedGeoTerrain::ReconcileGlobeFull(const LogiGlobeFrame& Frame, int32 BuildBudget, double GeoDeadline)
{
	int32 TextureRedriveBudget = FMath::Max(1, TileBuildsPerStep);
	bool bBuildDeferred = false;
	// Collect the new tiles to BUILD, then build them priority-sorted below (audit D build-priority) instead of
	// in arbitrary frame order — so a budget-limited backlog spends its slots on the most-valuable tiles first.
	TArray<uint32> ToBuild;
	ToBuild.Reserve(Frame.tile_count);

	ResidentScratch.Empty(Frame.tile_count);
	for (uint32 i = 0; i < Frame.tile_count; ++i)
	{
		const LogiGlobeTile& T = Frame.tiles[i];
		const uint64 Key = T.tile_key;
		ResidentScratch.Add(Key);

		if (FTileEntry* Existing = Tiles.Find(Key))
		{
			// Re-show ONLY a tile that's actually texture-READY. Re-showing unconditionally would un-hide a
			// still-loading "prepare" tile (built HIDDEN by the invisible-loading gate) before its atlas binds →
			// it draws untextured (flat), which the atmosphere then washes out. TexturedKeys.Contains is true
			// exactly when MarkTileTextured has run (atlas bound, or textureless/surrendered), so prepare tiles
			// stay hidden while genuinely-evicted-then-re-entered textured tiles still re-show.
			ApplyTileVisibility(Key, *Existing, T.flags); // mirror the published HIDDEN flag (the Rust gate)
			// RE-DRIVE a missing atlas (decode failed / starved / not yet arrived — or an external texture that
			// only became available now). Bounded per frame + per tile; PendingTexture also tracks it for the
			// steady-state retry.
			if (!Existing->Texture.IsValid() && T.texture_jpeg_len > 0
				&& Existing->TextureAttempts < GMaxTextureAttempts
				&& !DecodingKeys.Contains(Key) && !AwaitingTextureConfirm.Contains(Key)
				&& TextureRedriveBudget > 0)
			{
				--TextureRedriveBudget;
				++Existing->TextureAttempts;
				DispatchTileDecode(Key, T.texture_jpeg, static_cast<int64>(T.texture_jpeg_len));
				PendingTexture.Add(Key);
			}
			continue;
		}
		if (T.vertex_count == 0 || T.vertices == nullptr)
		{
			continue;
		}
		ToBuild.Add(i); // defer the build to the priority-sorted pass below
	}
	// Build COARSEST-first (highest geometric_error): with a limited budget the region's COVERAGE lands before
	// its fine detail (no holes while fine tiles trickle in), matching the top-down graft order. The E fetch
	// concentration already makes the resident set the focal tiles; this just orders the build WITHIN it, so a
	// backlogged frame spends its slots on coverage rather than an arbitrary frame-order slice.
	ToBuild.Sort([&Frame](const uint32& A, const uint32& B) {
		return Frame.tiles[A].geometric_error > Frame.tiles[B].geometric_error;
	});
	for (const uint32 BuildIdx : ToBuild)
	{
		if (BuildBudget <= 0 || FPlatformTime::Seconds() >= GeoDeadline)
		{
			bBuildDeferred = true; // budget/deadline hit → the remaining (finer) tiles defer to next frame's backlog
			break;
		}
		--BuildBudget;
		BuildGlobeTile(Frame.tiles[BuildIdx]);
	}

	const bool bEvictDeferred = EvictDeparted(ResidentScratch);

	// In sync only if every selected tile is built AND every departed tile fully recycled. Any deferral ⇒ backlog,
	// so the delta path stays disabled until a later full pass clears it.
	bDeltaInSync = !bBuildDeferred && !bEvictDeferred;
}

// Bounded atlas re-drive for built-but-untextured tiles (PendingTexture): retry a SMALL number of decodes per
// frame, reaching each tile's current atlas bytes by key (logi_globe_world_tile_by_key) — no frame re-scan. A
// tile leaves the set once textured, evicted, or its retry cap is hit (a permanently-undecodable atlas).
void ALogiBakedGeoTerrain::DriveTileTextures(int32 Budget)
{
	if (Budget <= 0 || PendingTexture.Num() == 0 || GlobeWorld == nullptr)
	{
		return;
	}
	TArray<uint64, TInlineAllocator<32>> Done; // keys to drop (textured / evicted / gave up) — removed after the loop
	int32 Dispatched = 0;
	for (uint64 Key : PendingTexture)
	{
		FTileEntry* Entry = Tiles.Find(Key);
		if (Entry == nullptr || Entry->Texture.IsValid())
		{
			Done.Add(Key); // evicted, or the atlas already landed — drop it
			continue;
		}
		if (DecodingKeys.Contains(Key) || AwaitingTextureConfirm.Contains(Key))
		{
			continue; // a decode is in flight, or its upload is awaiting GPU-resident confirmation — wait for it
		}
		if (Entry->TextureAttempts >= GMaxTextureAttempts)
		{
			// Gave up after the retry cap. Show + report it anyway (texture-ready by surrender) so the Rust gate stops
			// holding its coarse parent — one soft tile beats a region pinned coarse forever.
			MarkTileTextured(Key, *Entry);
			Done.Add(Key);
			continue;
		}
		if (Dispatched >= Budget)
		{
			break; // bounded per frame
		}
		// Fetch THIS tile's current atlas bytes (valid until the next poll) and re-dispatch a decode.
		LogiGlobeTile T = {};
		if (logi_globe_world_tile_by_key(GlobeWorld, Key, &T) == LOGI_GEO_OK && T.texture_jpeg_len > 0)
		{
			++Entry->TextureAttempts;
			++Dispatched;
			DispatchTileDecode(Key, T.texture_jpeg, static_cast<int64>(T.texture_jpeg_len));
		}
	}
	for (uint64 Key : Done)
	{
		PendingTexture.Remove(Key);
	}
}

void ALogiBakedGeoTerrain::TickTileFades(float DeltaSeconds)
{
	if (DeltaSeconds <= 0.0f) { return; }
	const float FadeInSec = FMath::Max(0.05f, GlobeFadeInSec);
	const float EaseExp = FMath::Clamp(GlobeFadeEaseExp, 1.0f, 6.0f);

	// Bloom IN: advance each fading tile's FadeAlpha 0→1 on an ease-out curve and push it to the MID. The push is an
	// in-place material-uniform-buffer update — the static cached draw command picks it up with NO proxy rebuild
	// (same mechanism as the per-step DebugColor recolor), so this is cheap. Iterating FadingKeys keeps it
	// O(fading), so a settled scene (nothing fading) costs nothing.
	if (FadingKeys.Num() > 0)
	{
		TArray<uint64, TInlineAllocator<32>> DoneFading;
		for (uint64 Key : FadingKeys)
		{
			FTileEntry* E = Tiles.Find(Key);
			if (E == nullptr) { DoneFading.Add(Key); continue; } // evicted while fading — drop it from the set
			if (E->FadeMode == EGlobeFade::WaitTexture)
			{
				// The atlas will never arrive (decode gave up) ⇒ bloom in on the placeholder rather than stay
				// invisible; otherwise keep waiting (the held coarse tile covers it meanwhile).
				if (PendingTexture.Contains(Key) || AwaitingTextureConfirm.Contains(Key) || DecodingKeys.Contains(Key)) { continue; }
				E->FadeMode = EGlobeFade::FadingIn;
			}
			if (E->FadeMode != EGlobeFade::FadingIn) { DoneFading.Add(Key); continue; }
			E->FadeT = FMath::Min(1.0f, E->FadeT + DeltaSeconds / FadeInSec);
			const float Eased = 1.0f - FMath::Pow(1.0f - E->FadeT, EaseExp); // ease-out: quick to commit, soft to settle
			if (UMaterialInstanceDynamic* Mid = E->Material.Get())
			{
				Mid->SetScalarParameterValue(TEXT("FadeAlpha"), Eased);
			}
			if (E->FadeT >= 1.0f) { E->FadeMode = EGlobeFade::None; DoneFading.Add(Key); } // fully in — stop touching it
		}
		for (uint64 Key : DoneFading) { FadingKeys.Remove(Key); }
	}
}
#endif // LOGI_BEVY_HAVE_GEO

bool ALogiBakedGeoTerrain::EvictDeparted(const TSet<uint64>& Resident)
{
	const double Now = FPlatformTime::Seconds();
	// Tiles back in the cut leave grace untouched-by-recycle: still built, still textured, nothing to redo.
	// (ReconcileGlobeFull's Existing branch re-shows them via the published flag in the same pass.)
	for (TMap<uint64, double>::TIterator It(DepartedGrace); It; ++It)
	{
		if (Resident.Contains(It->Key))
		{
			It.RemoveCurrent();
		}
	}
	DepartedScratch.Reset();
	for (const TPair<uint64, FTileEntry>& Pair : Tiles)
	{
		if (!Resident.Contains(Pair.Key) && !DepartedGrace.Contains(Pair.Key))
		{
			DepartedScratch.Add(Pair.Key);
		}
	}
	// RETIRE, don't recycle (2026-06-10 oscillation fix — see TileRetainSeconds): hide the departed tile in
	// place and start its grace. Crucially its TexturedKeys report is KEPT — the instant recycle's un-texturing
	// is what re-held ancestors in the Rust gate and flipped the cut back, re-decoding the flap edge forever.
	// Hiding now (not at recycle) still removes a de-selected coarse ancestor's stale surface immediately, so
	// the old hide-defer's z-fight protection is preserved. The actual teardown happens in SweepRetainedTiles.
	for (uint64 Key : DepartedScratch)
	{
		DepartedGrace.Add(Key, Now);
		if (FTileEntry* Entry = Tiles.Find(Key))
		{
			if (ULogiTileMeshComponent* Comp = Entry->ProcComp.Get())
			{
				Comp->SetTileVisible(false); // in-place hide (C4c — the engine path re-uploaded per flip)
			}
		}
	}
	// Retiring is complete handling for this frame (the sweep self-drains every step) — no reconcile backlog.
	return false;
}

void ALogiBakedGeoTerrain::SweepRetainedTiles(double Now)
{
	if (DepartedGrace.Num() == 0)
	{
		return;
	}
	// Collect due keys: grace expired, or — over budget — oldest first regardless of age. O(retained) scan per
	// step (retained is budget-capped); the sort only runs in the over-budget case, on the small scratch.
	DepartedScratch.Reset();
	const double Cutoff = Now - static_cast<double>(FMath::Max(0.0f, TileRetainSeconds));
	for (const TPair<uint64, double>& Pair : DepartedGrace)
	{
		if (Pair.Value <= Cutoff)
		{
			DepartedScratch.Add(Pair.Key);
		}
	}
	const int32 OverBudget = DepartedGrace.Num() - FMath::Max(0, TileRetainBudget);
	if (OverBudget > DepartedScratch.Num())
	{
		// Need more than the age-expired set: take the oldest OverBudget entries outright.
		TArray<TPair<uint64, double>> ByAge;
		ByAge.Reserve(DepartedGrace.Num());
		for (const TPair<uint64, double>& Pair : DepartedGrace) { ByAge.Add(Pair); }
		ByAge.Sort([](const TPair<uint64, double>& A, const TPair<uint64, double>& B) { return A.Value < B.Value; });
		DepartedScratch.Reset();
		for (int32 i = 0; i < OverBudget; ++i) { DepartedScratch.Add(ByAge[i].Key); }
	}
	// Spread teardown across steps: each recycle is O(1) but unregistering hundreds in one frame is a spike.
	// Leftovers stay hidden in grace and go on following steps. This is the ONE place a departed tile's
	// TexturedKeys report is withdrawn — long after the cut moved on, so the gate sees a quiet, batched change.
	int32 EvictBudget = FMath::Max(1, TileEvictionsPerStep);
	for (uint64 Key : DepartedScratch)
	{
		if (EvictBudget-- <= 0)
		{
			break;
		}
		DepartedGrace.Remove(Key);
		FTileEntry Entry;
		if (Tiles.RemoveAndCopyValue(Key, Entry))
		{
			RecycleTile(Entry);
		}
		FadingKeys.Remove(Key);
		PendingTexture.Remove(Key);
		if (TexturedKeys.Remove(Key) > 0) { bTexturedKeysDirty = true; } // un-report it (a return re-textures)
	}
}

// (The HoldFadingOut "park the refined-away parent" lane was deleted here: it had no remaining caller — the
// Rust texture gate + the geometric hole-clip keep the coarse stand-in rendered and clipped until its
// replacements are textured, so departed tiles recycle immediately in RecycleByKey / EvictDeparted.)

bool ALogiBakedGeoTerrain::GetBenchMetrics(FLogiGeoBenchMetrics& Out)
{
	Out = FLogiGeoBenchMetrics{};
	// Engine-frame costs first (always available). Thread times are cycle counters; GPU time arrives via the
	// RHI's end-of-frame query. Draw calls/primitives are the RHI's last-completed-frame counters for GPU 0.
	Out.DrawCalls = GNumDrawCallsRHI[0];
	Out.PrimitivesDrawn = GNumPrimitivesDrawnRHI[0];
	Out.GameThreadMs = FPlatformTime::ToMilliseconds(GGameThreadTime);
	Out.RenderThreadMs = FPlatformTime::ToMilliseconds(GRenderThreadTime);
	Out.GpuMs = FPlatformTime::ToMilliseconds(RHIGetGPUFrameCycles());
	Out.GeoStepAvgMs = DbgGeoStepAvgMs;
	Out.GeoStepPeakMs = DbgGeoStepPeakMs;

	// Per-tile aggregates over the built set: O(built) payload-size reads, no render-thread traffic.
	for (const TPair<uint64, FTileEntry>& Pair : Tiles)
	{
		if (const ULogiTileMeshComponent* Comp = Pair.Value.ProcComp.Get())
		{
			uint64 Verts = 0, Tris = 0;
			Comp->GetTileGeometryStats(Verts, Tris);
			Out.BuiltVerts += Verts;
			Out.BuiltTris += Tris;
			if (Comp->IsVisible())
			{
				++Out.VisibleComps;
				Out.VisibleVerts += Verts;
				Out.VisibleTris += Tris;
			}
		}
	}
	Out.Built = Tiles.Num();
	Out.Retained = DepartedGrace.Num();
	Out.Decoding = DecodingKeys.Num();
	Out.PendingTexture = PendingTexture.Num();
	Out.PendingBinds = PendingBinds.Num();
	Out.AwaitingConfirm = AwaitingTextureConfirm.Num();
	Out.Fading = FadingKeys.Num();
	Out.Textured = TexturedKeys.Num();
	Out.FrameId = LastFrameId;
	// Texture-pipeline outcome counters (lock-free; relaxed is fine — these are statistics, not a handshake).
	Out.DecodeDispatched = TexStats->DecodeDispatched.load(std::memory_order_relaxed);
	Out.DecodeJobOk = TexStats->DecodeJobOk.load(std::memory_order_relaxed);
	Out.DecodeJobEmpty = TexStats->DecodeJobEmpty.load(std::memory_order_relaxed);
	Out.DrainDropEvicted = TexStats->DrainDropEvicted.load(std::memory_order_relaxed);
	Out.DrainDropDup = TexStats->DrainDropDup.load(std::memory_order_relaxed);
	Out.UploadConfirmed = TexStats->UploadConfirmed.load(std::memory_order_relaxed);
	Out.UploadNoRhi = TexStats->UploadNoRhi.load(std::memory_order_relaxed);
	Out.UploadMipMismatch = TexStats->UploadMipMismatch.load(std::memory_order_relaxed);
	Out.UploadShortMip = TexStats->UploadShortMip.load(std::memory_order_relaxed);
	Out.UploadLockFailed = TexStats->UploadLockFailed.load(std::memory_order_relaxed);
	Out.UploadRetried = TexStats->UploadRetried.load(std::memory_order_relaxed);
	Out.UploadSurrendered = TexStats->UploadSurrendered.load(std::memory_order_relaxed);

#if LOGI_BEVY_HAVE_GEO
	if (GlobeWorld != nullptr)
	{
		// NEVER poll here: logi_globe_world_poll is the single-consumer frame intake with valid-until-next-poll
		// semantics — an extra poll would consume a New frame and advance the FFI's delta baseline behind
		// StepAndBuildGlobe's back (its next poll would read `Same` with EMPTY deltas and mark the frame applied
		// without applying it — silent reconcile corruption). The per-frame numbers come from the step's own
		// cached telemetry; only the v16 telemetry block is read here (a brief lock, no frame intake).
		Out.Rendered = DbgTilesRendered;
		Out.Resident = DbgTilesResident;
		Out.InFlight = DbgTilesInFlight;
		LogiGlobeTelemetry T = {};
		if (logi_globe_world_telemetry(GlobeWorld, &T) != sizeof(T))
		{
			return false; // layout handshake failed — DLL/header mismatch; the bench must not record garbage
		}
		Out.DecodeFailed = static_cast<uint32>(T.decode_failed_total);
		Out.FetchFailed = static_cast<uint32>(T.fetch_failed_total);
		Out.GraftFailed = static_cast<uint32>(T.graft_failed_total);
		Out.UrlResolveFailed = static_cast<uint32>(T.url_resolve_failed_total);
		Out.HoldKick = static_cast<uint32>(T.hold_kick);
		Out.HoldDead = static_cast<uint32>(T.hold_dead);
		Out.HoldLoading = static_cast<uint32>(T.hold_loading);
		Out.HoldLeafUnderSse = static_cast<uint32>(T.hold_leaf_under_sse);
		Out.HoldPendingSubtree = static_cast<uint32>(T.hold_pending_subtree);
		Out.FetchSkipGrafted = static_cast<uint32>(T.fetch_skip_grafted);
		Out.FlashMB = static_cast<uint32>(T.flash_bytes / (1024 * 1024));
		Out.FreeSlots = static_cast<uint32>(T.free_slots);
		Out.ResidentCap = static_cast<uint32>(T.max_resident);
		Out.EvictedTotal = static_cast<uint32>(T.evicted_total);
		Out.PoolStarved = static_cast<uint32>(T.pool_exhausted_total);
		Out.FlashHits = T.flash_hits;
		Out.FlashMisses = T.flash_misses;
		Out.DiskHits = T.disk_hits;
		Out.DiskMisses = T.disk_misses;
		Out.DiskCachedMB = T.disk_bytes / (1024 * 1024);
		return true;
	}
#endif
	return false;
}

void ALogiBakedGeoTerrain::AnchorSkyToPlanet()
{
#if LOGI_BEVY_HAVE_GEO
	if (!bAnchorSkyToPlanet || GlobeWorld == nullptr)
	{
		return;
	}
	// [earth_center_x_cm, earth_center_y_cm, earth_center_z_cm, geocentric_radius_km] — Rust kernel
	// geodesy (ABI v15), the same ENU/UE mapping every streamed vertex was rebased with.
	double Anchor[4] = {0.0, 0.0, 0.0, 0.0};
	if (logi_globe_world_sky_anchor(GlobeWorld, Anchor, 4) != 4)
	{
		return;
	}
	const FVector EarthCenter(Anchor[0], Anchor[1], Anchor[2]);
	const float RadiusKm = static_cast<float>(Anchor[3]);
	SkyEarthCenterUE = EarthCenter;
	bSkyAnchored = true; // the sun-zenith update keys off the same planet model
	for (TActorIterator<ASkyAtmosphere> It(GetWorld()); It; ++It)
	{
		ASkyAtmosphere* Sky = *It;
		USkyAtmosphereComponent* Comp = Sky->GetComponent();
		if (Comp == nullptr)
		{
			continue;
		}
		// PlanetCenterAtComponentTransform + the actor AT the earth center + the origin's geocentric radius:
		// the atmosphere (and the VolumetricCloud layer, which follows the atmosphere's planet) now curves
		// with the streamed WGS84 globe instead of a fixed sphere pinned at the UE origin. The residual is
		// the sphere-vs-ellipsoid difference (±~7 km by latitude) — absorbed later by the ellipsoid-depth AP
		// pass; the gross ±15 km misplacement (clouds on station cameras, detached limb) is gone.
		Sky->Modify(); // transactional in-editor (undoable; marks the level dirty so the anchor can be saved)
		if (USceneComponent* Root = Sky->GetRootComponent(); Root != nullptr && Root->Mobility != EComponentMobility::Movable)
		{
			// SetActorLocation refuses Static actors in PIE/game worlds; the atmosphere isn't light-baked,
			// so Movable costs nothing and the anchor works in every world type.
			Root->SetMobility(EComponentMobility::Movable);
		}
		Sky->SetActorLocation(EarthCenter);
		Comp->TransformMode = ESkyAtmosphereTransformMode::PlanetCenterAtComponentTransform;
		Comp->SetBottomRadius(RadiusKm); // marks the render state dirty (re-creates the sky proxy)
		Comp->MarkRenderStateDirty();    // TransformMode has no setter — make sure the proxy re-reads it
		UE_LOG(LogLogiBevy, Log,
			TEXT("Sky anchored to planet: %s center=(%.0f, %.0f, %.0f) cm, ground radius %.4f km."),
			*Sky->GetName(), EarthCenter.X, EarthCenter.Y, EarthCenter.Z, RadiusKm);
	}
	// Exponential height fog is a FLAT-WORLD slab keyed to absolute UE Z — on the curved globe every region
	// away from the ENU origin sinks kilometres below z=0 and saturates to max-opacity fog (rio ≈ −5,600 km ⇒
	// the uniform veil burying the ground at every distant station, and the slab's flat boundary = the
	// sharp-edged white wedges in earlier shots). A planet has no horizontal fog slab: disable it with the
	// anchor and let the (now planet-true) aerial perspective carry the distance haze; a planet-aware
	// near-ground haze can return with the ellipsoid-depth AP pass.
	for (TActorIterator<AExponentialHeightFog> It(GetWorld()); It; ++It)
	{
		AExponentialHeightFog* Fog = *It;
		if (UExponentialHeightFogComponent* FogComp = Fog->GetComponent(); FogComp != nullptr && FogComp->IsVisible())
		{
			Fog->Modify();
			FogComp->SetVisibility(false);
			UE_LOG(LogLogiBevy, Log,
				TEXT("Sky anchored to planet: height fog '%s' disabled (flat-world slab on a curved globe)."),
				*Fog->GetName());
		}
	}
#endif
}

void ALogiBakedGeoTerrain::UpdateSunZenith(const FVector& CamUE)
{
	if (!bSkyAnchored || !bSunAtCameraZenith)
	{
		return;
	}
	const FVector Up = (CamUE - SkyEarthCenterUE).GetSafeNormal();
	if (Up.IsNearlyZero())
	{
		return;
	}
	if (!SunLightCache.IsValid())
	{
		for (TActorIterator<ADirectionalLight> It(GetWorld()); It; ++It)
		{
			SunLightCache = *It; // the level has one sun; first hit is it
			break;
		}
		if (!SunLightCache.IsValid())
		{
			return;
		}
	}
	ADirectionalLight* Sun = SunLightCache.Get();
	// Light direction = travel direction = straight DOWN the camera's geocentric up (local solar noon —
	// matches the ~noon sun P3DT bakes into its albedo). Skip sub-0.1° changes: a parked camera re-orients
	// nothing, so the atmosphere/cloud LUTs only churn while actually flying.
	const FVector Target = -Up;
	const FVector Current = Sun->GetActorRotation().Vector();
	if (FVector::DotProduct(Current, Target) > 0.9999985) // cos(0.1°)
	{
		return;
	}
	if (USceneComponent* Root = Sun->GetRootComponent(); Root != nullptr && Root->Mobility != EComponentMobility::Movable)
	{
		Root->SetMobility(EComponentMobility::Movable); // a planet-true sun is dynamic by definition
	}
	Sun->SetActorRotation(Target.Rotation());
}

void ALogiBakedGeoTerrain::ReleaseWorld()
{
#if LOGI_BEVY_HAVE_GEO
	if (BakedWorld != nullptr)
	{
		logi_baked_world_destroy(BakedWorld);
		BakedWorld = nullptr;
	}
	if (GlobeWorld != nullptr)
	{
		logi_globe_world_destroy(GlobeWorld);
		GlobeWorld = nullptr;
	}
#endif
}

void ALogiBakedGeoTerrain::ReloadTerrain()
{
	// A settings change may switch Source/material, so destroy every live tile AND the recycle pools (whose
	// pooled components/MIDs/textures may no longer match the new config) so the next tick rebuilds clean.
	DestroyAllTiles();
	ClearPools();
	TerrainMaterial.Reset();   // re-resolve (displacement toggle/material/scale, or the Source, may have changed)
	GlobeBaseMaterial.Reset();
	GlobeLitMaterial.Reset();
	GlobeClipMaterial.Reset();
	bMaterialResolveAttempted = false;
	bWarnedNoGlobeMaterial = false;
	bWarnedNoLitGlobeMaterial = false;
	ReleaseWorld();
	bCreateAttempted = false; // allow EnsureWorld to rebuild with the new settings
}

#if WITH_EDITOR
void ALogiBakedGeoTerrain::PostEditChangeProperty(FPropertyChangedEvent& PropertyChangedEvent)
{
	Super::PostEditChangeProperty(PropertyChangedEvent);

	const FName Name = (PropertyChangedEvent.MemberProperty != nullptr)
		? PropertyChangedEvent.GetMemberPropertyName()
		: PropertyChangedEvent.GetPropertyName();

	// HOT-APPLY set: view-only knobs that take effect on the NEXT FRAME without tearing down the Rust world.
	// Everything NOT in this set defines the world (Source / origin / meshing / procedural+Google params /
	// Nanite+displacement toggles / tile lighting) and still does a full ReloadTerrain — and so does an
	// unknown / multi-property edit (the safe default). This is the "only rebuild when necessary" rule.
	static const TSet<FName> HotApplyProps = {
		GET_MEMBER_NAME_CHECKED(ALogiBakedGeoTerrain, DebugView),
		GET_MEMBER_NAME_CHECKED(ALogiBakedGeoTerrain, DebugPalette),
		GET_MEMBER_NAME_CHECKED(ALogiBakedGeoTerrain, DebugMaxDepth),
		GET_MEMBER_NAME_CHECKED(ALogiBakedGeoTerrain, bShowDebugPanel),
		GET_MEMBER_NAME_CHECKED(ALogiBakedGeoTerrain, DetailTargetPx),
		GET_MEMBER_NAME_CHECKED(ALogiBakedGeoTerrain, TileBuildsPerStep),
		GET_MEMBER_NAME_CHECKED(ALogiBakedGeoTerrain, GeoBuildBudgetMs),
		GET_MEMBER_NAME_CHECKED(ALogiBakedGeoTerrain, TileEvictionsPerStep),
		GET_MEMBER_NAME_CHECKED(ALogiBakedGeoTerrain, DisplacementScale),
	};

	if (!HotApplyProps.Contains(Name))
	{
		ReloadTerrain(); // a world-defining edit (or an unknown one) — rebuild from the new settings
		return;
	}

	// Live-apply the few hot knobs that need an action right now; the rest (DetailTargetPx + the per-step
	// budgets/caps) are simply read fresh by the next Tick/step, and the debug knobs are applied by
	// UpdateTileDebugDisplay() / SyncDebugPanelRegistration() — crucially with NO teardown of the world.
	if (Name == GET_MEMBER_NAME_CHECKED(ALogiBakedGeoTerrain, DisplacementScale))
	{
		if (UMaterialInstanceDynamic* MID = Cast<UMaterialInstanceDynamic>(TerrainMaterial.Get()))
		{
			MID->SetScalarParameterValue(TEXT("DisplacementScale"), DisplacementScale);
		}
	}
}
#endif

void ALogiBakedGeoTerrain::EndPlay(const EEndPlayReason::Type EndPlayReason)
{
	if (DebugPanelHandle.IsValid())
	{
		UDebugDrawService::Unregister(DebugPanelHandle); // stop drawing the telemetry overlay
		DebugPanelHandle.Reset();
	}
	DestroyAllTiles();
	ClearPools();
	TerrainMaterial.Reset();
	GlobeBaseMaterial.Reset();
	GlobeLitMaterial.Reset();
	GlobeClipMaterial.Reset();
	DebugMaterial.Reset();
	ReleaseWorld();
	Super::EndPlay(EndPlayReason);
}

void ALogiBakedGeoTerrain::BeginDestroy()
{
	if (DebugPanelHandle.IsValid())
	{
		UDebugDrawService::Unregister(DebugPanelHandle); // safety net if EndPlay didn't run
		DebugPanelHandle.Reset();
	}
	ReleaseWorld(); // safety net if EndPlay didn't run (e.g. editor actor deletion)
	Super::BeginDestroy();
}
