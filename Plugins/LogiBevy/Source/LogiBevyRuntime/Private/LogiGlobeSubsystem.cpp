// Copyright LogiUE. All Rights Reserved.

#include "LogiGlobeSubsystem.h"
#include "LogiBevyTypes.h"
#include "LogiBevyDeveloperSettings.h"
#include "LogiTileRenderShared.h" // FLogiStreamView/LogiResolveStreamView/LogiBuildTileSection (shared)
#include "Engine/World.h"
#include "Engine/GameViewportClient.h"
#include "GameFramework/Actor.h"
#include "GameFramework/PlayerController.h"
#include "Camera/PlayerCameraManager.h"
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

#if WITH_EDITOR
#include "LevelEditorViewport.h" // GCurrentLevelEditingViewportClient
#endif

#if LOGI_BEVY_HAVE_GEO
#include "logi_geo_ffi.h"
// Defined in LogiBevyFfi.cpp: false once a terrain-DLL ABI mismatch is detected. The logi_globe_* exports
// live in the same delay-loaded logi_geo.dll, so consult this before any globe call.
bool LogiBevy_IsTerrainAbiOk();
#endif

namespace
{
#if LOGI_BEVY_HAVE_GEO
	// The streaming viewpoint (FLogiStreamView + LogiResolveStreamView) and the proc-mesh section builder
	// (LogiBuildTileSection) now live in LogiTileRenderShared — shared with the baked-terrain lane instead
	// of a per-subsystem copy. The globe-specific helpers (ion token, tile-texture decode) follow.

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
#endif // LOGI_BEVY_HAVE_GEO
}

bool ULogiGlobeSubsystem::ShouldCreateSubsystem(UObject* Outer) const
{
	if (!Super::ShouldCreateSubsystem(Outer))
	{
		return false;
	}
#if LOGI_BEVY_HAVE_GEO
	if (const UWorld* World = Cast<UWorld>(Outer))
	{
		if (!GetDefault<ULogiBevyDeveloperSettings>()->bShowPhotorealGlobe)
		{
			return false;
		}
		// Game + PIE, plus the main level-editor world (NOT preview/thumbnail worlds).
		return World->IsGameWorld() || World->WorldType == EWorldType::Editor;
	}
#endif
	return false;
}

TStatId ULogiGlobeSubsystem::GetStatId() const
{
	RETURN_QUICK_DECLARE_CYCLE_STAT(ULogiGlobeSubsystem, STATGROUP_Tickables);
}

bool ULogiGlobeSubsystem::EnsureWorld()
{
#if LOGI_BEVY_HAVE_GEO
	if (GlobeWorld != nullptr)
	{
		return true;
	}
	if (bCreateAttempted)
	{
		return false; // a prior create failed — don't hammer ion every tick
	}
	bCreateAttempted = true;

	if (!LogiBevy_IsTerrainAbiOk())
	{
		UE_LOG(LogLogiBevy, Warning, TEXT("Globe: terrain DLL ABI not OK; skipping."));
		return false;
	}
	const FString Token = ReadIonToken();
	if (Token.IsEmpty())
	{
		UE_LOG(LogLogiBevy, Error,
			TEXT("Globe: no Cesium ion token. Set $env:CESIUM_ION_TOKEN or write the token to '%s'."),
			*FPaths::Combine(FPaths::ProjectIntermediateDir(), TEXT("CesiumIonToken.txt")));
		return false;
	}

	const ULogiBevyDeveloperSettings* S = GetDefault<ULogiBevyDeveloperSettings>();
	LogiGlobeCreateInfo Info{};
	Info.ion_asset_id = static_cast<uint64>(S->GlobeIonAssetId);
	Info.origin_longitude_deg = S->GlobeOriginLongitude;
	Info.origin_latitude_deg = S->GlobeOriginLatitude;
	Info.origin_height_m = S->GlobeOriginHeight;
	Info.max_tile_distance_m = S->GlobeMaxTileDistance;
	Info.workers = static_cast<uint32>(FMath::Clamp(S->GlobeFetchWorkers, 1, 32));
	Info._reserved = 0;

	// Persistent disk cache (gitignored Saved/P3DTCache). Off → 0 budget + null dir (no cache).
	const bool bCache = S->bGlobeDiskCache;
	Info.cache_budget_bytes = bCache
		? static_cast<uint64>(FMath::Max(1, S->GlobeCacheBudgetGB)) * 1024ull * 1024ull * 1024ull
		: 0ull;
	const FString CacheDir = FPaths::Combine(FPaths::ProjectSavedDir(), TEXT("P3DTCache"));

	// Keep the UTF-8 converters alive across the call.
	const FTCHARToUTF8 TokenUtf8(*Token);
	const FTCHARToUTF8 CacheDirUtf8(*CacheDir);
	GlobeWorld = logi_globe_world_create(&Info, TokenUtf8.Get(), bCache ? CacheDirUtf8.Get() : nullptr);
	if (GlobeWorld == nullptr)
	{
		UE_LOG(LogLogiBevy, Error, TEXT("logi_globe_world_create failed: %s"), *LastFfiError());
		return false;
	}
	UE_LOG(LogLogiBevy, Log,
		TEXT("Globe: streaming world created (ion asset %lld, origin %.4f, %.4f, %.0f m; disk cache %s, %d GB)."),
		S->GlobeIonAssetId, S->GlobeOriginLongitude, S->GlobeOriginLatitude, S->GlobeOriginHeight,
		bCache ? TEXT("ON") : TEXT("off"), bCache ? S->GlobeCacheBudgetGB : 0);
	return true;
#else
	return false;
#endif
}

void ULogiGlobeSubsystem::EnsureHost()
{
	if (!HostActor.IsValid() || !GlobeMesh.IsValid())
	{
		UWorld* World = GetWorld();
		if (World == nullptr)
		{
			return;
		}
		FActorSpawnParameters Params;
		Params.ObjectFlags |= RF_Transient;
		AActor* Host = World->SpawnActor<AActor>(AActor::StaticClass(), FTransform::Identity, Params);
		if (Host == nullptr)
		{
			return;
		}
#if WITH_EDITOR
		Host->SetActorLabel(TEXT("LogiPhotorealGlobe"));
#endif
		Host->Tags.Add(TEXT("LogiGlobe"));
		UProceduralMeshComponent* Mesh = NewObject<UProceduralMeshComponent>(Host, TEXT("LogiGlobeMesh"));
		Host->SetRootComponent(Mesh);
		Mesh->SetMobility(EComponentMobility::Movable);
		// P3DT atlases are pre-lit/unlit; the debug lane doesn't cast dynamic shadows (avoids VSM cost).
		Mesh->SetCastShadow(false);
		Mesh->RegisterComponent();
		HostActor = Host;
		GlobeMesh = Mesh;
	}

	// Per-tile texturing needs a material with a "BaseColorTex" texture parameter (authored asset). Until
	// it exists the tiles draw with the proc-mesh default material (untextured but visible). LOAD_Quiet so
	// a missing asset doesn't spam the log while it's being authored; warn just once.
	if (!BaseMaterial.IsValid())
	{
		UMaterialInterface* Mat = LoadObject<UMaterialInterface>(
			nullptr, TEXT("/Game/Globe/M_LogiGlobeTile.M_LogiGlobeTile"), nullptr, LOAD_NoWarn | LOAD_Quiet);
		if (Mat != nullptr)
		{
			BaseMaterial = Mat;
		}
		else if (!bWarnedNoMaterial)
		{
			UE_LOG(LogLogiBevy, Warning,
				TEXT("Globe: material /Game/Globe/M_LogiGlobeTile not found; tiles render untextured until it is authored."));
			bWarnedNoMaterial = true;
		}
	}
}

void ULogiGlobeSubsystem::EvictIfOverBudget()
{
	// Resident proc-mesh section cap (safety backstop; the Rust side already bounds the resident set via
	// the distance cull). Never evict a tile resident THIS frame — it would rebuild next frame (thrash).
	constexpr int32 MaxSections = 2048;
	UProceduralMeshComponent* Mesh = GlobeMesh.Get();
	if (Mesh == nullptr || TileSection.Num() <= MaxSections)
	{
		return;
	}
	struct FCand { uint64 LastSeen; uint64 Key; int32 Section; };
	TArray<FCand> Cands;
	Cands.Reserve(TileSection.Num());
	for (const TPair<uint64, FGlobeTileEntry>& Pair : TileSection)
	{
		if (Pair.Value.LastSeen == FrameCounter) { continue; }
		Cands.Add(FCand{ Pair.Value.LastSeen, Pair.Key, Pair.Value.Section });
	}
	Cands.Sort([](const FCand& A, const FCand& B) { return A.LastSeen < B.LastSeen; });
	const int32 NumToEvict = FMath::Min(TileSection.Num() - MaxSections, Cands.Num());
	for (int32 i = 0; i < NumToEvict; ++i)
	{
		Mesh->ClearMeshSection(Cands[i].Section);
		FreeSections.Add(Cands[i].Section);
		TileSection.Remove(Cands[i].Key); // releases the tile's Texture/MID strong refs
	}
}

void ULogiGlobeSubsystem::Tick(float DeltaTime)
{
#if LOGI_BEVY_HAVE_GEO
	UWorld* World = GetWorld();
	if (World == nullptr)
	{
		return;
	}
	// Streaming step throttle (~30 Hz) for ALL worlds (audit H5): the editor renders far faster than the
	// sim needs, and in PIE/game a synchronous per-new-tile JPEG decode + texture upload EVERY render frame
	// is a game-thread hitch. 30 Hz is ample for background globe streaming — the proc-mesh persists and
	// still renders every frame; only the step/build cadence is capped. (Decode is still synchronous; if a
	// hitch remains, move DecodeTileTexture to a worker task with a game-thread UpdateResource.)
	{
		constexpr float StepSeconds = 1.0f / 30.0f;
		StepThrottleAccum += DeltaTime;
		if (StepThrottleAccum < StepSeconds)
		{
			return;
		}
		StepThrottleAccum = 0.0f;
	}

	if (!EnsureWorld())
	{
		return;
	}
	EnsureHost();
	UProceduralMeshComponent* Mesh = GlobeMesh.Get();
	if (Mesh == nullptr)
	{
		return;
	}

	FLogiStreamView View;
	if (!LogiResolveStreamView(World, View))
	{
		return; // no viewport this tick
	}

	const ULogiBevyDeveloperSettings* S = GetDefault<ULogiBevyDeveloperSettings>();
	const double MaxSsePixels = FMath::Clamp(S->GlobeMaxScreenSpaceError, 1.0, 64.0);
	LogiGlobeFrame Frame{};
	if (logi_globe_world_step(GlobeWorld, View.Loc.X, View.Loc.Y, View.Loc.Z,
			View.FovY, View.ViewportHeight, MaxSsePixels, &Frame) != LOGI_GEO_OK)
	{
		return;
	}

	++FrameCounter;
	int32 BuildBudget = FMath::Max(1, S->GlobeTileBuildsPerStep);
	TSet<uint64>& Resident = ResidentScratch; // reused; Empty keeps the backing alloc
	Resident.Empty(Frame.tile_count);
	for (uint32 i = 0; i < Frame.tile_count; ++i)
	{
		const LogiGlobeTile& T = Frame.tiles[i];
		const uint64 Key = T.tile_key;
		Resident.Add(Key);

		if (FGlobeTileEntry* Existing = TileSection.Find(Key))
		{
			Existing->LastSeen = FrameCounter; // already built — refresh LRU stamp
			continue;
		}
		if (BuildBudget <= 0)
		{
			continue; // over budget this step — tile stays resident in Rust, builds on a later step
		}
		if (T.vertex_count == 0 || T.vertices == nullptr)
		{
			continue;
		}
		--BuildBudget;

		const int32 Section = (FreeSections.Num() > 0) ? FreeSections.Pop(EAllowShrinking::No) : NextSection++;
		LogiBuildTileSection(Mesh, Section, T.vertices, T.vertex_stride, T.vertices_len, T.vertex_count,
			T.indices, T.index_count);

		FGlobeTileEntry Entry;
		Entry.Section = Section;
		Entry.LastSeen = FrameCounter;
		// NOTE (audit C++ L9): DecodeTileTexture runs the JPEG/PNG decode synchronously on the game thread.
		// It is bounded per step by GlobeTileBuildsPerStep at the 30 Hz throttle above, so it is not a
		// per-frame hitch, but a hero-view burst still costs game-thread time. Moving it to a worker is not a
		// straightforward edit (async task + deferred UTexture2D create/UpdateResource on the game thread +
		// tile-eviction lifetime across the await); deferred until profiling shows it on the budget.
		if (UTexture2D* Tex = (T.texture_jpeg_len > 0)
				? DecodeTileTexture(T.texture_jpeg, static_cast<int64>(T.texture_jpeg_len))
				: nullptr)
		{
			Entry.Texture.Reset(Tex);
			if (UMaterialInterface* Base = BaseMaterial.Get())
			{
				if (UMaterialInstanceDynamic* MID = UMaterialInstanceDynamic::Create(Base, this))
				{
					MID->SetTextureParameterValue(TEXT("BaseColorTex"), Tex);
					Entry.Material.Reset(MID);
					Mesh->SetMaterial(Section, MID);
				}
			}
		}
		TileSection.Add(Key, MoveTemp(Entry));
	}

	// Clear tiles that left the resident set (frees their section + releases texture/MID).
	DepartedScratch.Reset();
	for (const TPair<uint64, FGlobeTileEntry>& Pair : TileSection)
	{
		if (!Resident.Contains(Pair.Key)) { DepartedScratch.Add(Pair.Key); }
	}
	for (uint64 K : DepartedScratch)
	{
		const FGlobeTileEntry E = TileSection.FindAndRemoveChecked(K);
		Mesh->ClearMeshSection(E.Section);
		FreeSections.Add(E.Section);
	}

	EvictIfOverBudget();

#if !UE_BUILD_SHIPPING
	// Milestone-F streaming diagnostic — camera fed, tiles selected/resident/in-flight, sections built, and
	// the proc-mesh world-bounds Z extent (near 0 = correct ground placement). Gated to non-shipping and
	// Verbose so it never spams shipping/PIE logs nor pays the FFI + bounds cost by default (audit): enable
	// with `Log LogLogiBevy Verbose`.
	if ((FrameCounter <= 3 || (FrameCounter % 30) == 0) && !LogLogiBevy.IsSuppressed(ELogVerbosity::Verbose))
	{
		const FBox B = Mesh->Bounds.GetBox();
		const uint64 CacheMB = logi_globe_world_cached_bytes(GlobeWorld) / (1024ull * 1024ull);
		UE_LOG(LogLogiBevy, Verbose,
			TEXT("Globe tick %llu cam_cm=(%.0f,%.0f,%.0f) fovY=%.2f vh=%.0f tiles=%u resident=%u inflight=%u sections=%d boundsZ=(%.0f..%.0f) cache=%lluMB"),
			static_cast<unsigned long long>(FrameCounter), View.Loc.X, View.Loc.Y, View.Loc.Z,
			View.FovY, View.ViewportHeight, Frame.tile_count, Frame.resident_tiles, Frame.in_flight,
			TileSection.Num(), B.Min.Z, B.Max.Z, static_cast<unsigned long long>(CacheMB));
	}
#endif
#else
	(void)DeltaTime;
#endif
}

void ULogiGlobeSubsystem::ReleaseWorld()
{
#if LOGI_BEVY_HAVE_GEO
	if (GlobeWorld != nullptr)
	{
		logi_globe_world_destroy(GlobeWorld);
		GlobeWorld = nullptr;
	}
#endif
}

void ULogiGlobeSubsystem::Deinitialize()
{
	if (AActor* Host = HostActor.Get())
	{
		Host->Destroy();
	}
	HostActor.Reset();
	GlobeMesh.Reset();
	TileSection.Reset(); // releases all per-tile Texture/MID strong refs
	FreeSections.Reset();
	NextSection = 0;
	FrameCounter = 0;
	ReleaseWorld();
	Super::Deinitialize();
}
