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
#include "RenderingThread.h"                   // ENQUEUE_RENDER_COMMAND — defer the BC1 mip upload to the render thread (audit T4)
#include "RHICommandList.h"                    // FRHICommandListImmediate / LockTexture2D / UnlockTexture2D (render-thread mip upload)
#include "RenderUtils.h"                       // CopyTextureData2D + GPixelFormats/FPixelFormatInfo — block-aware (BC1) mip copy
#include "IImageWrapper.h"
#include "IImageWrapperModule.h"
#include "Modules/ModuleManager.h"
#include "Misc/Paths.h"
#include "Misc/FileHelper.h"
#include "ProfilingDebugging/CpuProfilerTrace.h" // TRACE_CPUPROFILER_EVENT_SCOPE (zero cost when not tracing)
#include "HAL/PlatformTime.h"                  // FPlatformTime — geo-step game-thread cost probe
#include "Async/Async.h"                       // Async(EAsyncExecution::ThreadPool, …) — off-thread atlas decode
#include "ispc_texcomp.h"                      // Intel ISPC CompressBlocksBC7 — runtime BC7 encode of the tile atlases

#if LOGI_GEO_NANITE_LANE
#include "Engine/StaticMesh.h"
#endif

#if WITH_EDITOR
#include "MaterialEditingLibrary.h"                     // UMaterialEditingLibrary — wire DepthBiasCm -> Pixel Depth Offset
#include "Materials/Material.h"                          // UMaterial
#include "Materials/MaterialExpressionScalarParameter.h" // the DepthBiasCm + FadeAlpha parameter nodes
#include "Materials/MaterialExpressionTextureSampleParameter2D.h" // find the authored BaseColorTex sampler
#include "Materials/MaterialExpressionLinearInterpolate.h"        // placeholder→atlas emissive develop
#include "Materials/MaterialExpressionConstant3Vector.h"          // the neutral placeholder colour
#include "SceneTypes.h"                                  // EMaterialProperty::MP_PixelDepthOffset / MP_OpacityMask
#endif

#if LOGI_BEVY_HAVE_GEO
#include "logi_geo_ffi.h"
// Defined in LogiBevyFfi.cpp: false once a Geo-DLL ABI mismatch is detected. The logi_baked_* / logi_globe_*
// exports live in the same delay-loaded logi_geo.dll, so consult this before any world call.
bool LogiBevy_IsTerrainAbiOk();
// Defined in LogiBevyFfi.cpp: true iff ispc_texcomp.dll (the BC7 encoder) loaded at module startup. The tile
// decode consults it to SKIP BC7 (→ a placeholder tile) instead of crashing if that delay-loaded DLL is absent.
bool LogiBevy_IsIspcTexCompOk();

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

	// Box-downsample a tightly packed BGRA8 image (SrcW x SrcH) into Dst (DstW x DstH = half each, min 1 px),
	// averaging each 2x2 source block with round-to-nearest. Odd source dims clamp the trailing sample to the
	// last row/column so the edge texel isn't dropped (no out-of-bounds read at SrcW/SrcH == 1). Averaging is
	// in sRGB byte space (not gamma-decoded): the cheap, standard runtime path — it slightly darkens mips at
	// high-contrast edges versus a gamma-correct filter, acceptable for photoreal aerial atlases. A 256-entry
	// sRGB->linear / linear->sRGB LUT pair is the upgrade if darkening ever shows on a flamegraph-justified path.
	void DownsampleBoxBGRA8(const uint8* Src, int32 SrcW, int32 SrcH, uint8* Dst, int32 DstW, int32 DstH)
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
				for (int32 c = 0; c < 4; ++c) // B, G, R, A (A is opaque-255 for these atlases, averages to 255)
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

	// Worker-side decode: a tile atlas (JPEG/PNG) -> BC1 + full box-filtered mip chain, as plain CPU byte
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

		// mip 0: the tightly packed W*H*4 BGRA8. Clamp to that size — a decoder returning padded rows could make
		// Raw.Num() exceed it, and an unclamped copy would overrun the destination.
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
			DownsampleBoxBGRA8(OutMips[i - 1].GetData(), SrcW, SrcH, OutMips[i].GetData(), DstW, DstH);
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
		Info.workers = static_cast<uint32>(FMath::Clamp(GlobeFetchWorkers, 1, 32));
		Info.mesh_cache_budget_mb = static_cast<uint32>(FMath::Max(0, GlobeMeshCacheBudgetMB));
		Info.cache_budget_bytes = bGlobeDiskCache
			? static_cast<uint64>(FMath::Max(1, GlobeCacheBudgetGB)) * 1024ull * 1024ull * 1024ull
			: 0ull;
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
			EnsureGlobeMaterialPdo(Cast<UMaterial>(Mat)); // wire depth-priority (once, in-memory) — see definition
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
			Comp->SetVisibility(true, /*bPropagateToChildren=*/false); // already attached + registered; just show it
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
void ALogiBakedGeoTerrain::EnsureGlobeMaterialPdo(UMaterial* M)
{
	// Depth-priority refinement (companion to the per-tile `DepthBiasCm` set in the globe build loop): wire a
	// `DepthBiasCm` scalar parameter into this material's PIXEL DEPTH OFFSET, so a COARSER tile (large geometric
	// error → large DepthBiasCm) is pushed BACK in depth and a FINER tile (small error) wins the depth test
	// wherever they overlap. The coarse ancestor that fills not-yet-loaded gaps then shows ONLY in genuine holes,
	// never laid OVER loaded fine tiles — the reported coarse-over-sharp overlap. Done in C++ because the editor
	// Python material API does NOT expose Pixel Depth Offset (its `MaterialProperty` enum lacks it). Applied
	// IN-MEMORY (not saved) + recompiled: re-wired each editor session, idempotent within one via the static
	// latch. Editor-only — `RecompileMaterial` needs the shader compiler (mirrors the runtime debug-viz material).
	static bool bWired = false;
	if (bWired || M == nullptr)
	{
		return;
	}
	bWired = true;
	UMaterialExpression* Ex = UMaterialEditingLibrary::CreateMaterialExpression(
		M, UMaterialExpressionScalarParameter::StaticClass(), -700, 500);
	UMaterialExpressionScalarParameter* P = Cast<UMaterialExpressionScalarParameter>(Ex);
	if (P == nullptr)
	{
		return;
	}
	P->ParameterName = FName(TEXT("DepthBiasCm"));
	P->DefaultValue = 0.0f;
	UMaterialEditingLibrary::ConnectMaterialProperty(P, TEXT(""), MP_PixelDepthOffset);

	// LOD fade via TEXTURE-DEVELOP (robust for our DECOUPLED geometry/texture pipeline). The opacity-dither approach
	// made a tile INVISIBLE until its atlas confirmed; since Rust streams geometry before C++ decodes the texture,
	// and the coarse cover is only briefly held, the invisible terrain revealed the white sky — the "white globe".
	// Instead, keep the material OPAQUE and lerp the EMISSIVE from a neutral placeholder → the atlas as FadeAlpha
	// 0→1: a loading tile shows the placeholder (opaque, like before the fade) and the gray→atlas SNAP becomes a
	// smooth develop. A tile is NEVER transparent, so it can never reveal the sky — structurally white-proof. Force
	// Opaque first (in case a prior session's Masked/dither wiring was saved into the asset). If the authored
	// BaseColorTex sampler can't be found, we skip the develop and leave the emissive untouched (tiles snap, but
	// still render their atlas — never white).
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
	UMaterialExpressionConstant3Vector* Gray = Cast<UMaterialExpressionConstant3Vector>(
		UMaterialEditingLibrary::CreateMaterialExpression(M, UMaterialExpressionConstant3Vector::StaticClass(), -760, 760));
	UMaterialExpressionLinearInterpolate* Lerp = Cast<UMaterialExpressionLinearInterpolate>(
		UMaterialEditingLibrary::CreateMaterialExpression(M, UMaterialExpressionLinearInterpolate::StaticClass(), -470, 700));
	if (BaseTex != nullptr && Fade != nullptr && Gray != nullptr && Lerp != nullptr)
	{
		Fade->ParameterName = FName(TEXT("FadeAlpha"));
		Fade->DefaultValue = 1.0f; // a tile with no per-tile override renders its atlas fully (rest state)
		Gray->Constant = FLinearColor(0.5f, 0.5f, 0.5f); // neutral placeholder (matches the engine default texture)
		UMaterialEditingLibrary::ConnectMaterialExpressions(Gray, FString(), Lerp, TEXT("A"));
		UMaterialEditingLibrary::ConnectMaterialExpressions(BaseTex, TEXT("RGB"), Lerp, TEXT("B"));
		UMaterialEditingLibrary::ConnectMaterialExpressions(Fade, FString(), Lerp, TEXT("Alpha"));
		UMaterialEditingLibrary::ConnectMaterialProperty(Lerp, FString(), MP_EmissiveColor); // replaces BaseColorTex→Emissive
		UE_LOG(LogLogiBevy, Log, TEXT("Globe tile material: wired FadeAlpha emissive develop (placeholder→atlas) for the LOD fade."));
	}
	else
	{
		UE_LOG(LogLogiBevy, Warning, TEXT("Globe tile material: develop wiring skipped (BaseColorTex sampler not found); tiles snap, no fade."));
	}

	UMaterialEditingLibrary::RecompileMaterial(M);
	UE_LOG(LogLogiBevy, Log, TEXT("Globe tile material: wired DepthBiasCm -> Pixel Depth Offset (depth-priority refinement)."));
}
#endif // WITH_EDITOR

UMaterialInstanceDynamic* ALogiBakedGeoTerrain::AcquireMid(UMaterialInterface* Base)
{
	while (MidPool.Num() > 0)
	{
		TStrongObjectPtr<UMaterialInstanceDynamic> Pooled = MidPool.Pop(EAllowShrinking::No);
		if (UMaterialInstanceDynamic* Mid = Pooled.Get())
		{
			// Reuse only a MID over the SAME parent: with the unlit/lit split a pooled MID may carry the other
			// base, and a MID can't be re-parented. A mismatch is dropped (GC reclaims) and we try the next /
			// create fresh. P3DT is uniform (all unlit), so in practice the pool holds one base and never churns.
			if (Mid->Parent == Base)
			{
				return Mid; // caller re-points BaseColorTex
			}
		}
	}
	return (Base != nullptr) ? UMaterialInstanceDynamic::Create(Base, this) : nullptr;
}

UMaterialInterface* ALogiBakedGeoTerrain::GlobeMaterialForTile(uint32 TileFlags)
{
#if LOGI_BEVY_HAVE_GEO
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
	return nullptr;
#endif
}

void ALogiBakedGeoTerrain::RecycleTile(FTileEntry& Entry)
{
	if (ULogiTileMeshComponent* Comp = Entry.ProcComp.Get())
	{
		Comp->ClearTileGeometry();                                  // free the geometry (O(1): drops this tile's proxy)
		Comp->SetVisibility(false, /*bPropagateToChildren=*/false);
		ProcCompPool.Add(MoveTemp(Entry.ProcComp));                 // keep it registered + GC-rooted for reuse
	}
	if (Entry.Material.IsValid())
	{
		MidPool.Add(MoveTemp(Entry.Material));
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
	// LOD cross-fade: tear down any held / fading tiles too (the world is being rebuilt).
	for (TPair<uint64, FTileEntry>& Pair : FadingOut)
	{
		if (ULogiTileMeshComponent* C = Pair.Value.ProcComp.Get()) { C->DestroyComponent(); }
	}
	FadingOut.Reset();
	FadingKeys.Reset();
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
// overwrites every mip with the real BC1 data. Lock-THEN-Realloc matches CreateTransient: a freshly-added mip has no
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

// Enqueue the BC1 mip-byte upload onto the RENDER THREAD (audit T4). The game thread has already created/acquired the
// UTexture2D and made its RHI resource live (CreateTransient + UpdateResource for a fresh texture, or a pooled texture
// whose RHI is already resident); here it only hands off a pointer to the texture's render resource + a SHARED ref to
// the BC1 bytes, then returns — the per-mip Memcpy now happens on the render thread.
//
// LIFETIME (the load-bearing part): the BC1 bytes live in `Mips`, a thread-safe shared pointer captured BY VALUE in the
// render lambda. So the bytes are guaranteed to outlive the asynchronous upload no matter when it runs (it may execute
// after the next game tick has already polled/drained again) — the chain is freed only when the render thread drops this
// last reference at the end of the command. This mirrors the T1 mesh proxy's FLogiTilePayload and the JPEG-copy in
// DispatchTileDecode. We capture the FTextureResource* (a render-thread-owned object) rather than the UTexture2D* (a
// UObject — must never be touched off the game thread); the UObject is kept alive meanwhile by the caller's
// TStrongObjectPtr (FTileEntry::Texture or the pool), and a UObject teardown enqueues the resource's ReleaseResource
// AFTER this command (render commands are FIFO), so the resource is valid for the duration of this command (re-validated
// defensively below).
void ALogiBakedGeoTerrain::EnqueueTileMipUploadRT(UTexture2D* Tex, const TSharedRef<FLogiTileMipChain, ESPMode::ThreadSafe>& Mips)
{
	FTextureResource* Resource = (Tex != nullptr) ? Tex->GetResource() : nullptr;
	if (Resource == nullptr) { Mips->UploadState.store(-1, std::memory_order_release); return; }

	ENQUEUE_RENDER_COMMAND(LogiGeoTileMipUpload)(
		[Resource, Mips](FRHICommandListImmediate& RHICmdList)
		{
			TRACE_CPUPROFILER_EVENT_SCOPE(LogiGeoTileTexUploadRT);

			// The RHI texture is created by FStreamableTextureResource::InitRHI (enqueued by the game thread's
			// UpdateResource, so it runs BEFORE this FIFO-ordered command) and persists until the resource is released
			// on this same thread — so by the time this runs it is valid. Re-fetch + null-check anyway (defence in
			// depth): if the resource was torn down before this ran, GetTexture2DRHI() returns null and we drop the upload.
			FRHITexture* TexRHI = Resource->GetTexture2DRHI();
			if (TexRHI == nullptr) { Mips->UploadState.store(-1, std::memory_order_release); return; }

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

			for (int32 i = 0; i < Num; ++i)
			{
				const TArray64<uint8>& Src = Mips->Mips[i];
				if (Src.Num() == 0) { bAllMipsUploaded = false; continue; }
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
				if (TightSize > Src.Num()) { bAllMipsUploaded = false; continue; } // short/truncated source — never read past the decode output

				// Lock the live RHI mip for write. DestStride is the GPU row pitch (for BC1 a "row" is one row of 4x4
				// blocks); it may be PADDED above the tight source pitch (e.g. D3D12 aligns row pitch), so we copy
				// block-row by block-row with CopyTextureData2D — the same block-aware primitive the engine uses to
				// upload compressed mips (FTexture2DResource::CreateTexture). A flat Memcpy would corrupt rows whenever
				// DestStride != SrcPitch. This is the per-tile pixel Memcpy — now ON THE RENDER THREAD (the T4 win).
				uint32 DestStride = 0;
				void* Dest = RHICmdList.LockTexture2D(TexRHI, static_cast<uint32>(i), RLM_WriteOnly, DestStride,
					/*bLockWithinMiptail=*/false);
				if (Dest == nullptr) { bAllMipsUploaded = false; continue; } // LockTexture2D failed (upload-heap pressure) — mip left unwritten
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
			// resident). The game thread does zero pixel work; the BC1 Memcpy runs in the enqueued render command.
			if (Tex != nullptr && Tex->GetPlatformData() != nullptr
				&& Tex->GetPlatformData()->Mips.Num() == Mips->Mips.Num() && Tex->GetResource() != nullptr)
			{
				EnqueueTileMipUploadRT(Tex, Mips);
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
#if WITH_EDITORONLY_DATA
	Tex->MipGenSettings = TMGS_NoMipmaps; // "no GENERATION" — we supply the chain; pins LOD bias to 0
#endif
	AllocateMipDescriptors(Tex, W, H, Mips->Mips.Num());
	Tex->UpdateResource();
	EnqueueTileMipUploadRT(Tex, Mips);
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
	// Copy the JPEG out of the frame-transient FFI buffer (the next poll may overwrite it), then decode on a
	// worker. The task captures only POD + the copied bytes + the shared queue + the module pointer — never
	// `this` — so it is safe regardless of the actor's lifetime.
	TArray<uint8> Bytes(Jpeg, static_cast<int32>(Len));
	TSharedPtr<TQueue<FLogiTileDecode, EQueueMode::Mpsc>, ESPMode::ThreadSafe> Queue = DecodeQueue;
	IImageWrapperModule* Module = ImageWrapperModule;
	Async(EAsyncExecution::ThreadPool, [Key, Bytes = MoveTemp(Bytes), Queue, Module]()
	{
		FLogiTileDecode Out;
		Out.Key = Key;
		DecodeJpegToBgraMips(Module, Bytes.GetData(), Bytes.Num(), Out.W, Out.H, Out.Mips); // Mips empty on failure
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
		if (Entry == nullptr || !Entry->ProcComp.IsValid()) { continue; } // evicted before the decode finished
		if (Entry->Texture.IsValid() || AwaitingTextureConfirm.Contains(Done.Key)) { continue; } // already textured / a bind is pending
		// Move the worker-produced BC1 chain into a thread-safe shared payload (no copy) so AcquireTexture can hand
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
						if (!IsDebugViewActive()) { Entry->ProcComp->SetMaterial(0, Mid); }
					}
				}
			}
			if (Mid != nullptr) { Mid->SetTextureParameterValue(TEXT("BaseColorTex"), Tex); }
			// Atlas is resident — START the LOD bloom: the tile dithers in from invisible to opaque (sharpening into
			// view) instead of popping. If fade is off, FadeMode is None and the tile was already opaque (instant).
			if (Entry->FadeMode == EGlobeFade::WaitTexture) { Entry->FadeMode = EGlobeFade::FadingIn; FadingKeys.Add(Key); }
			AwaitingTextureConfirm.Remove(Key);
			PendingBinds.RemoveAtSwap(i, EAllowShrinking::No);
			continue;
		}

		// State == -1: a mip was dropped (LockTexture2D failed under pressure, or a short/empty mip). The decode +
		// texture are fine — only the GPU copy failed — so RETRY just the upload a bounded number of times.
		if (B.UploadRetries < MaxUploadRetries && B.Texture.IsValid() && B.Chain.IsValid())
		{
			++B.UploadRetries;
			B.Chain->UploadState.store(0, std::memory_order_release);
			EnqueueTileMipUploadRT(B.Texture.Get(), B.Chain.ToSharedRef());
			continue; // still pending; re-checked next step
		}
		// Gave up after the retry cap: return the texture to the pool and leave the tile on its gray placeholder. It
		// stays in PendingTexture, so the steady-state re-drive eventually re-decodes + re-uploads it.
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
			Comp->SetMaterial(0, Mid);
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
			Comp->SetMaterial(0, Real);
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

	DbgStatus = FString::Printf(TEXT("%s  |  View: %s"), SourceLabel, GeoDebugViewName(DebugView));
	DbgTilesRendered = Rendered;
	DbgTilesResident = Resident;
	DbgTilesBuilt = Built;
	DbgTilesInFlight = InFlight;
	DbgTilesDecoding = DecodingKeys.Num();
	DbgTilesTextured = Textured;
	DbgPools = FString::Printf(TEXT("%d / %d / %d"), ProcCompPool.Num(), MidPool.Num(), PoolTex);
	DbgGeoStepMs = FString::Printf(TEXT("%.2f / %.2f"), DbgGeoStepAvgMs, DbgGeoStepPeakMs);
	DbgDepthRange = (Built > 0) ? FString::Printf(TEXT("%u - %u"), DebugDepthMin, DebugDepthMax) : TEXT("-");
	DbgGeoErrorRange = (GeoHi > 0.0f) ? FString::Printf(TEXT("%.2f - %.1f m"), DebugGeoErrMin, DebugGeoErrMax) : TEXT("-");
	DbgFailures = Failures;
	DbgCachedMB = CachedMB;
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
				Comp->SetMaterial(0, Mat);
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
	const double GeoDeadline = StepT0 + static_cast<double>(GeoBuildBudgetMs) * 0.001; // drain + build share this slice

	// Apply any tile atlases finished by worker decodes since the last step. This only creates the UTexture2D +
	// assigns the tile's MID (O(1) each) — the JPEG decode + mip build already happened off the game thread.
	// Runs FIRST so already-decoded textures land promptly; it yields the moment it crosses the shared deadline.
	DrainDecodedTextures(GeoDeadline);

	// Advance the LOD cross-fade (bloom-in + held-tile recycle) EVERY step — including the camera-still frame-id skip
	// below — so in-flight fades always complete. dt from the real-time clock, clamped so a hitch can't snap a fade.
	const float FadeDt = (LastFadeStepSeconds > 0.0) ? static_cast<float>(FMath::Min(StepT0 - LastFadeStepSeconds, 0.1)) : 0.0f;
	LastFadeStepSeconds = StepT0;
	if (FadingKeys.Num() > 0 || FadingOut.Num() > 0) { TickTileFades(FadeDt); }

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
	LogiGlobeFrame Frame{};
	if (logi_globe_world_poll(GlobeWorld, &Frame) != LOGI_GEO_OK || (Frame.tile_count > 0 && Frame.tiles == nullptr))
	{
		return; // nothing published yet — leave resident tiles in place (no eviction)
	}

	// Diagnostic: surface WHY a tile stays stuck at a coarse (blurry) ancestor. Two distinct classes:
	//  - refinement couldn't DESCEND (graft_failed: a child tileset.json manifest didn't graft → its subtree
	//    never appears; url_resolve_failed: a content URI didn't resolve) — the tile is never even fetched, so
	//    no SSE target can sharpen it. These were SILENT before.
	//  - a tile we DID try to load gave up (fetch_failed: bytes never arrived; decode_failed: didn't parse).
	// Logged only when a count CHANGES, so it pinpoints the moment + kind without spamming the per-frame step.
	{
		const uint64 Failed = logi_globe_world_failed_counts(GlobeWorld);
		const uint64 Refine = logi_globe_world_refine_failures(GlobeWorld);
		static uint64 LastFailed = 0;
		static uint64 LastRefine = 0;
		if (Failed != LastFailed || Refine != LastRefine)
		{
			const uint32 DecodeFailed = static_cast<uint32>(Failed >> 32);
			const uint32 FetchFailed = static_cast<uint32>(Failed & 0xFFFFFFFFu);
			const uint32 GraftFailed = static_cast<uint32>(Refine >> 32);
			const uint32 ResolveFailed = static_cast<uint32>(Refine & 0xFFFFFFFFu);
			UE_LOG(LogLogiBevy, Warning,
				TEXT("Geo refine/load failures: graft_failed=%u (manifest didn't graft) url_resolve_failed=%u | fetch_failed=%u decode_failed=%u | resident=%u in_flight=%u"),
				GraftFailed, ResolveFailed, FetchFailed, DecodeFailed, Frame.resident_tiles, Frame.in_flight);
			LastFailed = Failed;
			LastRefine = Refine;
		}
	}

	// Heartbeat (every 30 frames): the live streaming state. `render` = tiles in this published frame;
	// `resident` = decoded tiles Rust holds; `in_flight` = outstanding fetches RIGHT NOW (the "how many
	// requests are loading" number). Steady-high `in_flight` while the camera is still ⇒ throughput-bound
	// (raise fetch workers / in-flight cap); `in_flight` ~0 with a coarse scene ⇒ the selection settled
	// (under-refining — SSE/frustum, not loading). Also exposes the TRUE render count vs "textures built".
	{
		static uint32 HeartbeatN = 0;
		if (++HeartbeatN % 30u == 0u)
		{
			UE_LOG(LogLogiBevy, Log, TEXT("Geo stream: render=%u resident=%u in_flight=%u cached_mb=%llu"),
				Frame.tile_count, Frame.resident_tiles, Frame.in_flight,
				static_cast<unsigned long long>(logi_globe_world_cached_bytes(GlobeWorld) / (1024 * 1024)));
		}
	}

	// ADR-0012 delta reconcile. The work this poll needs is read from the frame id + the per-poll delta:
	//  - frame_id UNCHANGED ⇒ the scene is identical to the last poll (camera still / streaming settled): skip the
	//    whole reconcile (O(1)) — unless a prior over-budget pass left a backlog, then drain it from the full list.
	//  - frame_id CHANGED, in sync, and the delta fits this frame's budgets ⇒ apply the O(Δ) delta (recycle
	//    removed[], build added[]). Deltas are never repeated, so we take this path ONLY when we can apply it FULLY.
	//  - otherwise (first frame / large burst / backlog) ⇒ re-scan the full visible list, which defers safely.
	const bool bChanged = (Frame.frame_id != LastFrameId);
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

	// Bounded atlas re-drive for built-but-untextured tiles (decode failures / not-yet-arrived). PendingTexture is
	// tiny in steady state; it reaches a tile by key via logi_globe_world_tile_by_key (the delta path never visits
	// already-resident tiles), so this never re-scans the frame.
	DriveTileTextures(BuildBudget);

	// Live telemetry + debug heatmaps. Pull the streaming give-up counters once more (cheap atomics) and pack
	// them for the panel; the geo-step game-thread cost is now timed in Tick (RecordGeoStepMs), so this lane no
	// longer keeps its own static avg/max timer. The depth + geometric-error ranges are computed over the
	// resident tiles inside UpdateDebugTelemetry; UpdateTileDebugDisplay then (re)colours them if a view is on.
	{
		const uint64 Failed = logi_globe_world_failed_counts(GlobeWorld);
		const uint64 Refine = logi_globe_world_refine_failures(GlobeWorld);
		const FString Failures = FString::Printf(TEXT("%u / %u / %u / %u"),
			static_cast<uint32>(Failed & 0xFFFFFFFFu),  // fetch_failed
			static_cast<uint32>(Failed >> 32),          // decode_failed
			static_cast<uint32>(Refine >> 32),          // graft_failed
			static_cast<uint32>(Refine & 0xFFFFFFFFu)); // url_resolve_failed
		const int32 CachedMB = static_cast<int32>(logi_globe_world_cached_bytes(GlobeWorld) / (1024 * 1024));
		UpdateDebugTelemetry(TEXT("Google Tiles"), static_cast<int32>(Frame.tile_count),
			static_cast<int32>(Frame.resident_tiles), static_cast<int32>(Frame.in_flight), Failures, CachedMB);
	}
	UpdateTileDebugDisplay(View);
#else
	(void)View;
#endif
}

#if LOGI_BEVY_HAVE_GEO
// Build ONE globe tile into a fresh pooled component + placeholder MID, and kick off its atlas decode. Shared by
// the delta (added[]) and full-reconcile paths; the caller guarantees Key is not already in `Tiles`. The geometry
// hand-off is a flat byte copy + an O(1) proxy hand-off — the de-interleave + RHI upload run on the RENDER THREAD.
void ALogiBakedGeoTerrain::BuildGlobeTile(const LogiGlobeTile& T)
{
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
	if (UMaterialInterface* Base = GlobeMaterialForTile(T.flags))
	{
		if (UMaterialInstanceDynamic* Mid = AcquireMid(Base))
		{
			Mid->SetTextureParameterValue(TEXT("BaseColorTex"), nullptr);
			// Depth-priority refinement via Pixel Depth Offset (DepthBiasCm = geometric error in cm): a coarser
			// tile is pushed back more, so a finer tile wins the depth test where they overlap. No-op until
			// M_LogiGlobeTile exposes DepthBiasCm → Pixel Depth Offset.
			Mid->SetScalarParameterValue(TEXT("DepthBiasCm"), T.geometric_error * 100.0f);
			Comp->SetMaterial(0, Mid);
			Entry.Material.Reset(Mid);
		}
	}
	// LOD cross-fade init: a fading tile starts INVISIBLE (FadeAlpha 0) and blooms in once its atlas confirms (a
	// held coarse tile covers it meanwhile, so there's no gray flash); a textureless tile has nothing to wait for,
	// so it blooms from build. Fade off ⇒ FadeMode stays None and FadeAlpha keeps its material default (1, opaque).
	if (bGlobeTileFade)
	{
		Entry.FadeT = 0.0f;
		Entry.FadeMode = (T.texture_jpeg_len > 0) ? EGlobeFade::WaitTexture : EGlobeFade::FadingIn;
		// FadeAlpha 0 ⇒ the emissive shows the neutral placeholder; the tile is OPAQUE (never invisible) and develops
		// to its atlas once textured (WaitTexture → FadingIn on confirm). A textureless tile develops from build.
		// Only FadingIn tiles are ticked; a WaitTexture tile just shows the placeholder until its atlas arrives.
		if (Entry.FadeMode == EGlobeFade::FadingIn) { FadingKeys.Add(T.tile_key); }
		if (UMaterialInstanceDynamic* Mid = Entry.Material.Get())
		{
			Mid->SetScalarParameterValue(TEXT("FadeAlpha"), 0.0f);
		}
	}
	if (T.texture_jpeg_len > 0)
	{
		Entry.TextureAttempts = 1; // counts the dispatch below toward the per-tile retry cap
	}
	Tiles.Add(T.tile_key, MoveTemp(Entry));

	// Photoreal atlas: decode the JPEG + build the mip chain on a WORKER (off the game thread). Track the tile in
	// PendingTexture so DriveTileTextures can bound-retry it if that decode fails or is slow.
	if (T.texture_jpeg_len > 0)
	{
		DispatchTileDecode(T.tile_key, T.texture_jpeg, static_cast<int64>(T.texture_jpeg_len));
		PendingTexture.Add(T.tile_key);
	}
}

// Recycle ONE tile by key (the delta removed[] path): a full teardown to the pool, O(1). Unlike the full path's
// EvictDeparted, a delta removal is never repeated, so it is always a FULL recycle — never a hide-defer.
void ALogiBakedGeoTerrain::RecycleByKey(uint64 Key)
{
	FTileEntry Entry;
	if (Tiles.RemoveAndCopyValue(Key, Entry))
	{
		RecycleTile(Entry); // texture-develop keeps tiles opaque, so no held cover is needed — recycle immediately
	}
	FadingKeys.Remove(Key);
	PendingTexture.Remove(Key);
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
	constexpr uint8 MaxTextureAttempts = 4;
	int32 TextureRedriveBudget = FMath::Max(1, TileBuildsPerStep);
	bool bBuildDeferred = false;

	ResidentScratch.Empty(Frame.tile_count);
	for (uint32 i = 0; i < Frame.tile_count; ++i)
	{
		const LogiGlobeTile& T = Frame.tiles[i];
		const uint64 Key = T.tile_key;
		ResidentScratch.Add(Key);

		if (FTileEntry* Existing = Tiles.Find(Key))
		{
			// Re-show if an over-budget eviction frame hid it, then it re-entered before its full recycle.
			if (ULogiTileMeshComponent* Comp = Existing->ProcComp.Get())
			{
				Comp->SetVisibility(true, /*bPropagateToChildren=*/false);
			}
			// RE-DRIVE a missing atlas (decode failed / starved / not yet arrived — or an external texture that
			// only became available now). Bounded per frame + per tile; PendingTexture also tracks it for the
			// steady-state retry.
			if (!Existing->Texture.IsValid() && T.texture_jpeg_len > 0
				&& Existing->TextureAttempts < MaxTextureAttempts
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
		if (BuildBudget <= 0 || FPlatformTime::Seconds() >= GeoDeadline)
		{
			bBuildDeferred = true; // a selected tile is NOT built this frame → backlog; stay on the full path
			continue;
		}
		--BuildBudget;
		BuildGlobeTile(T);
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
	constexpr uint8 MaxTextureAttempts = 4;
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
		if (Entry->TextureAttempts >= MaxTextureAttempts)
		{
			Done.Add(Key); // gave up after the retry cap — leave it on its placeholder
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
			if (E == nullptr) { DoneFading.Add(Key); continue; } // evicted / held while fading — drop it from the set
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

	// Hold OUT: count down each parked coarse tile; recycle it once its finer replacements have bloomed in over it
	// (by which point it is fully occluded, so its removal is invisible).
	if (FadingOut.Num() > 0)
	{
		TArray<uint64, TInlineAllocator<32>> Expired;
		for (TPair<uint64, FTileEntry>& Pair : FadingOut)
		{
			Pair.Value.HoldRemaining -= DeltaSeconds;
			if (Pair.Value.HoldRemaining <= 0.0f) { Expired.Add(Pair.Key); }
		}
		for (uint64 Key : Expired)
		{
			FTileEntry Entry;
			if (FadingOut.RemoveAndCopyValue(Key, Entry)) { RecycleTile(Entry); }
		}
	}
}
#endif // LOGI_BEVY_HAVE_GEO

bool ALogiBakedGeoTerrain::EvictDeparted(const TSet<uint64>& Resident)
{
	DepartedScratch.Reset();
	for (const TPair<uint64, FTileEntry>& Pair : Tiles)
	{
		if (!Resident.Contains(Pair.Key))
		{
			DepartedScratch.Add(Pair.Key);
		}
	}
	// Spread teardown across steps: a camera move can depart hundreds of tiles at once, and unregistering them
	// all in one frame is its own spike. Each eviction is O(1) (recycle to the pool), so the cap is generous;
	// any leftover departed tiles keep rendering harmlessly and evict on the following steps. Returns whether any
	// teardown was DEFERRED (hidden, not recycled) — the globe delta path treats that as a backlog.
	int32 EvictBudget = FMath::Max(1, TileEvictionsPerStep);
	bool bDeferred = false;
	for (uint64 Key : DepartedScratch)
	{
		if (EvictBudget > 0)
		{
			FTileEntry Entry;
			if (!Tiles.RemoveAndCopyValue(Key, Entry))
			{
				continue;
			}
			--EvictBudget;
			RecycleTile(Entry); // texture-develop keeps tiles opaque — no held cover needed
			FadingKeys.Remove(Key);
			PendingTexture.Remove(Key);
		}
		else if (FTileEntry* Entry = Tiles.Find(Key))
		{
			// Over the per-frame teardown budget: HIDE the departed tile NOW (full recycle happens on a later
			// frame). A de-selected coarse ancestor would otherwise keep drawing OVER its fine children for the
			// few frames until its recycle (coarse-over-fine overlap / z-fight during a fast descent); hiding it
			// immediately removes the stale low-detail surface without the cost of a full teardown this frame.
			bDeferred = true;
			if (ULogiTileMeshComponent* Comp = Entry->ProcComp.Get())
			{
				Comp->SetVisibility(false, /*bPropagateToChildren=*/false);
			}
		}
	}
	return bDeferred;
}

void ALogiBakedGeoTerrain::HoldFadingOut(uint64 Key, FTileEntry&& Entry)
{
	// ONLY a TEXTURED tile can cover its replacements' bloom with real imagery. An UNtextured tile forced opaque
	// would render as a flat white/gray placeholder — and during a fast zoom-in, tiles get refined away before they
	// finish texturing, so holding those opaque paints the whole globe white. Recycle untextured tiles immediately
	// (their replacements simply bloom over whatever textured ancestor is still resident). Also bound the held set:
	// a fast pan departs hundreds of tiles/sec and holding them all would balloon VRAM — past the cap, recycle now.
	constexpr int32 MaxHeld = 512;
	if (!Entry.Texture.IsValid() || (FadingOut.Num() >= MaxHeld && !FadingOut.Contains(Key)))
	{
		RecycleTile(Entry);
		return;
	}
	// If this key is somehow already held (rapid remove→re-add→remove), recycle the older one first so its
	// component isn't leaked when the map entry is overwritten below.
	if (FTileEntry* Old = FadingOut.Find(Key)) { RecycleTile(*Old); }
	// Force it fully opaque so it actually COVERS the bloom (a textured tile refined away mid-bloom would otherwise
	// still be partly transparent → a hole); it no longer fades.
	if (UMaterialInstanceDynamic* Mid = Entry.Material.Get()) { Mid->SetScalarParameterValue(TEXT("FadeAlpha"), 1.0f); }
	Entry.FadeMode = EGlobeFade::None;
	Entry.HoldRemaining = FMath::Max(0.05f, GlobeParentHoldSec);
	FadingOut.Add(Key, MoveTemp(Entry));
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
