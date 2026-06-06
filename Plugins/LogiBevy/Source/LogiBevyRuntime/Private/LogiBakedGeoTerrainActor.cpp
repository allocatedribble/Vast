// Copyright LogiUE. All Rights Reserved.

#include "LogiBakedGeoTerrainActor.h"
#include "LogiBevyTypes.h"
#include "LogiTileRenderShared.h" // FLogiStreamView / LogiResolveStreamView / LogiBuildTile* (shared render core)
#include "Engine/World.h"
#include "Components/SceneComponent.h"
#include "Components/StaticMeshComponent.h"
#include "ProceduralMeshComponent.h"
#include "Materials/MaterialInterface.h"
#include "Materials/MaterialInstanceDynamic.h"
#include "Engine/Texture2D.h"
#include "TextureResource.h"
#include "IImageWrapper.h"
#include "IImageWrapperModule.h"
#include "Modules/ModuleManager.h"
#include "Misc/Paths.h"
#include "Misc/FileHelper.h"

#if LOGI_GEO_NANITE_LANE
#include "Engine/StaticMesh.h"
#endif

#if LOGI_BEVY_HAVE_GEO
#include "logi_geo_ffi.h"
// Defined in LogiBevyFfi.cpp: false once a Geo-DLL ABI mismatch is detected. The logi_baked_* / logi_globe_*
// exports live in the same delay-loaded logi_geo.dll, so consult this before any world call.
bool LogiBevy_IsTerrainAbiOk();

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

	// Decode a tile's embedded atlas (JPEG/PNG) into a transient BGRA8 texture. P3DT tiles are JPEG; PNG is
	// handled for generality. Returns nullptr on an unrecognised/invalid image.
	UTexture2D* DecodeTileTexture(const uint8* Data, int64 Len)
	{
		if (Data == nullptr || Len < 12) { return nullptr; }
		EImageFormat Fmt = EImageFormat::Invalid;
		if (Data[0] == 0xFF && Data[1] == 0xD8) { Fmt = EImageFormat::JPEG; }
		else if (Data[0] == 0x89 && Data[1] == 0x50 && Data[2] == 0x4E && Data[3] == 0x47) { Fmt = EImageFormat::PNG; }
		else { return nullptr; }

		IImageWrapperModule& Module = FModuleManager::LoadModuleChecked<IImageWrapperModule>(TEXT("ImageWrapper"));
		TSharedPtr<IImageWrapper> Wrapper = Module.CreateImageWrapper(Fmt);
		if (!Wrapper.IsValid() || !Wrapper->SetCompressed(Data, Len)) { return nullptr; }

		TArray64<uint8> Raw;
		if (!Wrapper->GetRaw(ERGBFormat::BGRA, 8, Raw)) { return nullptr; }
		const int32 W = Wrapper->GetWidth();
		const int32 H = Wrapper->GetHeight();
		if (W <= 0 || H <= 0 || Raw.Num() < static_cast<int64>(W) * H * 4) { return nullptr; }

		UTexture2D* Tex = UTexture2D::CreateTransient(W, H, PF_B8G8R8A8);
		if (Tex == nullptr) { return nullptr; }
		Tex->SRGB = true;
		if (FTexturePlatformData* PD = Tex->GetPlatformData())
		{
			// Clamp to the mip-0 allocation (W*H*4 BGRA8): a decoder that returns padded rows could make
			// Raw.Num() exceed it, and an unclamped memcpy would overflow the texture's heap allocation.
			const int64 MipBytes = static_cast<int64>(W) * static_cast<int64>(H) * 4;
			void* Mip = PD->Mips[0].BulkData.Lock(LOCK_READ_WRITE);
			FMemory::Memcpy(Mip, Raw.GetData(), FMath::Min<int64>(Raw.Num(), MipBytes));
			PD->Mips[0].BulkData.Unlock();
		}
		Tex->UpdateResource();
		return Tex;
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

	USceneComponent* SceneRoot = CreateDefaultSubobject<USceneComponent>(TEXT("Root"));
	RootComponent = SceneRoot;

	// Proc-mesh lane host: one shared component carrying a section per resident tile (the Nanite lane builds
	// per-tile static-mesh components at runtime instead).
	ProcMesh = CreateDefaultSubobject<UProceduralMeshComponent>(TEXT("LogiBakedGeoMesh"));
	ProcMesh->SetupAttachment(RootComponent);
	ProcMesh->SetMobility(EComponentMobility::Movable);
	// Tile verts are absolute world cm (origin at world 0), so the render mesh must IGNORE this actor's
	// transform — otherwise moving/scaling the placed actor shears the terrain out of view. The actor is a
	// host marker you drop into a level; the terrain stays pinned to its true geodetic world position.
	ProcMesh->SetAbsolute(true, true, true);
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
		Info._reserved = 0;
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
		}
		else if (!bWarnedNoGlobeMaterial)
		{
			UE_LOG(LogLogiBevy, Warning,
				TEXT("Streamed Tiles Terrain: material /Game/Globe/M_LogiGlobeTile not found; Google tiles render untextured until it is authored."));
			bWarnedNoGlobeMaterial = true;
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

void ALogiBakedGeoTerrain::Tick(float DeltaTime)
{
	Super::Tick(DeltaTime);
#if LOGI_BEVY_HAVE_GEO
	UWorld* World = GetWorld();
	if (World == nullptr)
	{
		return;
	}
	// ~30 Hz step throttle: meshes persist and render every frame; only the step/build cadence is capped.
	StepAccum += DeltaTime;
	constexpr float StepSeconds = 1.0f / 30.0f;
	if (StepAccum < StepSeconds)
	{
		return;
	}
	StepAccum = 0.0f;

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

	if (Source == ETerrainSource::GoogleTiles)
	{
		StepAndBuildGlobe(View);
	}
	else
	{
		StepAndBuildBaked(View);
	}
#else
	(void)DeltaTime;
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

	UProceduralMeshComponent* PMesh = ProcMesh;
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
			if (PMesh == nullptr)
			{
				continue;
			}
			const int32 Section = (FreeSections.Num() > 0) ? FreeSections.Pop(EAllowShrinking::No) : NextSection++;
			LogiBuildTileSection(PMesh, Section, T.vertices, T.vertex_stride, T.vertices_len, T.vertex_count,
				T.indices, T.index_count);
			if (UMaterialInterface* Mat = TerrainMaterial.Get())
			{
				PMesh->SetMaterial(Section, Mat);
			}
			Entry.Section = Section;
		}
		Tiles.Add(Key, MoveTemp(Entry));
	}

	EvictDeparted(ResidentScratch);
#else
	(void)View;
#endif
}

void ALogiBakedGeoTerrain::StepAndBuildGlobe(const FLogiStreamView& View)
{
#if LOGI_BEVY_HAVE_GEO
	UProceduralMeshComponent* PMesh = ProcMesh;
	if (PMesh == nullptr)
	{
		return;
	}
	// Planetary scale: the globe step takes the f64 world-cm camera directly (no f32 cast). SSE is capped at
	// 64 px by the globe lane regardless of the actor's Detail Target.
	const double MaxSsePixels = FMath::Clamp(static_cast<double>(DetailTargetPx), 1.0, 64.0);
	int32 BuildBudget = FMath::Max(1, TileBuildsPerStep);

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

	ResidentScratch.Empty(Frame.tile_count);
	for (uint32 i = 0; i < Frame.tile_count; ++i)
	{
		const LogiGlobeTile& T = Frame.tiles[i];
		const uint64 Key = T.tile_key;
		ResidentScratch.Add(Key);

		if (Tiles.Contains(Key))
		{
			continue; // already built and still resident
		}
		if (T.vertex_count == 0 || T.vertices == nullptr)
		{
			continue;
		}
		if (BuildBudget <= 0)
		{
			continue; // over budget this step — tile stays resident in Rust, builds on a later step
		}
		--BuildBudget;

		const int32 Section = (FreeSections.Num() > 0) ? FreeSections.Pop(EAllowShrinking::No) : NextSection++;
		LogiBuildTileSection(PMesh, Section, T.vertices, T.vertex_stride, T.vertices_len, T.vertex_count,
			T.indices, T.index_count);

		FTileEntry Entry;
		Entry.Section = Section;
		// Per-tile photoreal atlas (audit C++ L9: synchronous decode, bounded per step by Tile Builds Per
		// Step at the 30 Hz throttle — not a per-frame hitch, but a hero-view burst still costs game-thread
		// time; a worker-task decode is deferred until profiling shows it on the budget).
		if (UTexture2D* Tex = (T.texture_jpeg_len > 0)
				? DecodeTileTexture(T.texture_jpeg, static_cast<int64>(T.texture_jpeg_len))
				: nullptr)
		{
			Entry.Texture.Reset(Tex);
			if (UMaterialInterface* Base = GlobeBaseMaterial.Get())
			{
				if (UMaterialInstanceDynamic* MID = UMaterialInstanceDynamic::Create(Base, this))
				{
					MID->SetTextureParameterValue(TEXT("BaseColorTex"), Tex);
					Entry.Material.Reset(MID);
					PMesh->SetMaterial(Section, MID);
				}
			}
		}
		Tiles.Add(Key, MoveTemp(Entry));
	}

	EvictDeparted(ResidentScratch);
#else
	(void)View;
#endif
}

void ALogiBakedGeoTerrain::EvictDeparted(const TSet<uint64>& Resident)
{
	DepartedScratch.Reset();
	for (const TPair<uint64, FTileEntry>& Pair : Tiles)
	{
		if (!Resident.Contains(Pair.Key))
		{
			DepartedScratch.Add(Pair.Key);
		}
	}
	UProceduralMeshComponent* PMesh = ProcMesh;
	for (uint64 Key : DepartedScratch)
	{
		FTileEntry Entry;
		if (!Tiles.RemoveAndCopyValue(Key, Entry)) // dropping Entry releases its per-tile MID + texture refs
		{
			continue;
		}
		if (Entry.Section != INDEX_NONE)
		{
			if (PMesh != nullptr)
			{
				PMesh->ClearMeshSection(Entry.Section);
			}
			FreeSections.Add(Entry.Section);
		}
		if (UStaticMeshComponent* Comp = Entry.NaniteComp.Get())
		{
			Comp->DestroyComponent();
		}
	}
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
	// Tear down every built tile so a settings change re-meshes from scratch on the next tick.
	for (const TPair<uint64, FTileEntry>& Pair : Tiles)
	{
		if (UStaticMeshComponent* Comp = Pair.Value.NaniteComp.Get())
		{
			Comp->DestroyComponent();
		}
	}
	Tiles.Reset(); // releases all per-tile MID/texture strong refs
	FreeSections.Reset();
	NextSection = 0;
	if (ProcMesh != nullptr)
	{
		ProcMesh->ClearAllMeshSections();
	}
	TerrainMaterial.Reset();   // re-resolve (displacement toggle/material/scale, or the Source, may have changed)
	GlobeBaseMaterial.Reset();
	bMaterialResolveAttempted = false;
	bWarnedNoGlobeMaterial = false;
	ReleaseWorld();
	bCreateAttempted = false; // allow EnsureWorld to rebuild with the new settings
}

#if WITH_EDITOR
void ALogiBakedGeoTerrain::PostEditChangeProperty(FPropertyChangedEvent& PropertyChangedEvent)
{
	Super::PostEditChangeProperty(PropertyChangedEvent);
	// Any setting changed -> reload this actor's terrain so the edit takes effect live.
	ReloadTerrain();
}
#endif

void ALogiBakedGeoTerrain::EndPlay(const EEndPlayReason::Type EndPlayReason)
{
	for (const TPair<uint64, FTileEntry>& Pair : Tiles)
	{
		if (UStaticMeshComponent* Comp = Pair.Value.NaniteComp.Get())
		{
			Comp->DestroyComponent();
		}
	}
	Tiles.Reset();
	FreeSections.Reset();
	TerrainMaterial.Reset();
	GlobeBaseMaterial.Reset();
	ReleaseWorld();
	Super::EndPlay(EndPlayReason);
}

void ALogiBakedGeoTerrain::BeginDestroy()
{
	ReleaseWorld(); // safety net if EndPlay didn't run (e.g. editor actor deletion)
	Super::BeginDestroy();
}
