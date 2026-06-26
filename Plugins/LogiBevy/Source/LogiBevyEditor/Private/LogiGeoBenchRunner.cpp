// Copyright LogiUE. All Rights Reserved.

#include "LogiGeoBenchRunner.h"
#include "LogiBevyEditor.h"
#include "LogiBakedGeoTerrainActor.h"

#include "Editor.h"                      // GEditor + GCurrentLevelEditingViewportClient
#include "Editor/EditorPerformanceSettings.h"
#include "LevelEditorViewport.h"         // FLevelEditorViewportClient
#include "Engine/World.h"
#include "EngineUtils.h"                 // TActorIterator
#include "Engine/DirectionalLight.h"     // ADirectionalLight — sun pose in every sidecar (planet-true sky attribution)
#include "HAL/FileManager.h"
#include "Misc/FileHelper.h"
#include "Misc/Paths.h"
#include "Misc/DateTime.h"
#include "Serialization/JsonReader.h"
#include "Serialization/JsonSerializer.h"
#include "UnrealClient.h"                // FScreenshotRequest

namespace
{
	FLogiGeoBenchRunner* GBenchRunner = nullptr;

	FString InboxDir()
	{
		return FPaths::ConvertRelativePathToFull(FPaths::Combine(FPaths::ProjectDir(), TEXT("Bench"), TEXT("_inbox")));
	}

	// The realtime override needs a stable display name for Add/Remove pairing.
	FText RealtimeOverrideName()
	{
		return NSLOCTEXT("LogiGeoBench", "RealtimeOverride", "Logi Geo Bench");
	}

	// Serialize one metrics sample into a JSON object — the sidecar schema. Field names are the contract the
	// CLI's summary/compare tables read; extend additively.
	TSharedRef<FJsonObject> MetricsToJson(const FLogiGeoBenchMetrics& M)
	{
		TSharedRef<FJsonObject> J = MakeShared<FJsonObject>();
		J->SetNumberField(TEXT("rendered"), M.Rendered);
		J->SetNumberField(TEXT("resident"), M.Resident);
		J->SetNumberField(TEXT("built"), M.Built);
		J->SetNumberField(TEXT("retained"), M.Retained);
		J->SetNumberField(TEXT("visible_comps"), M.VisibleComps);
		J->SetNumberField(TEXT("in_flight"), M.InFlight);
		J->SetNumberField(TEXT("decoding"), M.Decoding);
		J->SetNumberField(TEXT("pending_texture"), M.PendingTexture);
		J->SetNumberField(TEXT("pending_binds"), M.PendingBinds);
		J->SetNumberField(TEXT("awaiting_confirm"), M.AwaitingConfirm);
		J->SetNumberField(TEXT("fading"), M.Fading);
		J->SetNumberField(TEXT("textured"), M.Textured);
		J->SetNumberField(TEXT("frame_id"), static_cast<double>(M.FrameId));
		J->SetNumberField(TEXT("fetch_failed"), M.FetchFailed);
		J->SetNumberField(TEXT("decode_failed"), M.DecodeFailed);
		J->SetNumberField(TEXT("graft_failed"), M.GraftFailed);
		J->SetNumberField(TEXT("url_resolve_failed"), M.UrlResolveFailed);
		// THE TWO CACHES (v17): flash = the in-memory cell; disk = the one disk cell's aggregate over its
		// byte + decoded-geometry tiers (hit = any tier served, miss = network needed, mb = both tiers).
		J->SetNumberField(TEXT("flash_hits"), static_cast<double>(M.FlashHits));
		J->SetNumberField(TEXT("flash_misses"), static_cast<double>(M.FlashMisses));
		J->SetNumberField(TEXT("flash_mb"), M.FlashMB);
		J->SetNumberField(TEXT("disk_hits"), static_cast<double>(M.DiskHits));
		J->SetNumberField(TEXT("disk_misses"), static_cast<double>(M.DiskMisses));
		J->SetNumberField(TEXT("disk_cached_mb"), static_cast<double>(M.DiskCachedMB));
		J->SetNumberField(TEXT("built_verts"), static_cast<double>(M.BuiltVerts));
		J->SetNumberField(TEXT("built_tris"), static_cast<double>(M.BuiltTris));
		J->SetNumberField(TEXT("visible_verts"), static_cast<double>(M.VisibleVerts));
		J->SetNumberField(TEXT("visible_tris"), static_cast<double>(M.VisibleTris));
		J->SetNumberField(TEXT("geo_step_avg_ms"), M.GeoStepAvgMs);
		J->SetNumberField(TEXT("geo_step_peak_ms"), M.GeoStepPeakMs);
		J->SetNumberField(TEXT("draw_calls"), M.DrawCalls);
		J->SetNumberField(TEXT("primitives_drawn"), static_cast<double>(M.PrimitivesDrawn));
		J->SetNumberField(TEXT("game_thread_ms"), M.GameThreadMs);
		J->SetNumberField(TEXT("render_thread_ms"), M.RenderThreadMs);
		J->SetNumberField(TEXT("gpu_ms"), M.GpuMs);
		J->SetNumberField(TEXT("decode_dispatched"), M.DecodeDispatched);
		J->SetNumberField(TEXT("decode_job_ok"), M.DecodeJobOk);
		J->SetNumberField(TEXT("decode_job_empty"), M.DecodeJobEmpty);
		J->SetNumberField(TEXT("drain_drop_evicted"), M.DrainDropEvicted);
		J->SetNumberField(TEXT("drain_drop_dup"), M.DrainDropDup);
		J->SetNumberField(TEXT("upload_confirmed"), M.UploadConfirmed);
		J->SetNumberField(TEXT("upload_no_rhi"), M.UploadNoRhi);
		J->SetNumberField(TEXT("upload_mip_mismatch"), M.UploadMipMismatch);
		J->SetNumberField(TEXT("upload_short_mip"), M.UploadShortMip);
		J->SetNumberField(TEXT("upload_lock_failed"), M.UploadLockFailed);
		J->SetNumberField(TEXT("upload_retried"), M.UploadRetried);
		J->SetNumberField(TEXT("upload_surrendered"), M.UploadSurrendered);
		J->SetNumberField(TEXT("free_slots"), M.FreeSlots);
		J->SetNumberField(TEXT("resident_cap"), M.ResidentCap);
		J->SetNumberField(TEXT("evicted_total"), M.EvictedTotal);
		J->SetNumberField(TEXT("pool_starved"), M.PoolStarved);
		J->SetNumberField(TEXT("hold_kick"), M.HoldKick);
		J->SetNumberField(TEXT("hold_dead"), M.HoldDead);
		J->SetNumberField(TEXT("hold_loading"), M.HoldLoading);
		J->SetNumberField(TEXT("hold_leaf_under_sse"), M.HoldLeafUnderSse);
		J->SetNumberField(TEXT("hold_pending_subtree"), M.HoldPendingSubtree);
		J->SetNumberField(TEXT("fetch_skip_grafted"), M.FetchSkipGrafted);
		return J;
	}

	void WriteJson(const FString& Path, const TSharedRef<FJsonObject>& Obj)
	{
		FString Out;
		const TSharedRef<TJsonWriter<>> Writer = TJsonWriterFactory<>::Create(&Out);
		FJsonSerializer::Serialize(Obj, Writer);
		FFileHelper::SaveStringToFile(Out, *Path);
	}
}

void FLogiGeoBenchRunner::Init()
{
	check(GBenchRunner == nullptr);
	GBenchRunner = new FLogiGeoBenchRunner();
	IFileManager::Get().MakeDirectory(*InboxDir(), /*Tree=*/true);
	// Per-frame ticker: the fly path captures at up to 60 Hz, so the runner ticks every frame; the idle
	// cost is bounded by the inbox scan rate-limiting itself to ~4 Hz inside PollInbox.
	GBenchRunner->TickerHandle = FTSTicker::GetCoreTicker().AddTicker(
		FTickerDelegate::CreateRaw(GBenchRunner, &FLogiGeoBenchRunner::Tick), 0.0f);
	UE_LOG(LogLogiBevyEditor, Log, TEXT("Geo bench runner: watching %s"), *InboxDir());
}

void FLogiGeoBenchRunner::Shutdown()
{
	if (GBenchRunner != nullptr)
	{
		FTSTicker::GetCoreTicker().RemoveTicker(GBenchRunner->TickerHandle);
		delete GBenchRunner;
		GBenchRunner = nullptr;
	}
}

bool FLogiGeoBenchRunner::Tick(float /*DeltaSeconds*/)
{
	if (RunRequest.IsValid())
	{
		if (FlyTrack.Num() > 0)
		{
			TickFly();
		}
		else
		{
			TickStation();
		}
	}
	else
	{
		PollInbox();
	}
	return true; // keep ticking
}

void FLogiGeoBenchRunner::PollInbox()
{
	// The ticker is per-frame for the fly path; a directory scan per frame is waste — keep it ~4 Hz.
	const double Now = FPlatformTime::Seconds();
	if (Now - LastInboxPollSeconds < 0.25)
	{
		return;
	}
	LastInboxPollSeconds = Now;
	TArray<FString> Files;
	IFileManager::Get().FindFiles(Files, *(InboxDir() / TEXT("*.json")), /*Files=*/true, /*Dirs=*/false);
	if (Files.Num() == 0)
	{
		return;
	}
	Files.Sort(); // oldest-named first; one request per tick
	const FString Path = InboxDir() / Files[0];

	// A request is consumable only once the geo actor exists (the editor may still be loading the map).
	if (FindGeoActor() == nullptr)
	{
		return; // leave the file queued; retry next tick
	}

	FString Body;
	if (!FFileHelper::LoadFileToString(Body, *Path))
	{
		return; // transient share violation (writer mid-copy) — retry next tick
	}
	TSharedPtr<FJsonObject> Request;
	const TSharedRef<TJsonReader<>> Reader = TJsonReaderFactory<>::Create(Body);
	if (!FJsonSerializer::Deserialize(Reader, Request) || !Request.IsValid())
	{
		UE_LOG(LogLogiBevyEditor, Warning, TEXT("Geo bench: unparseable request %s — deleting."), *Path);
		IFileManager::Get().Delete(*Path);
		return;
	}
	IFileManager::Get().Delete(*Path);
	StartRun(Request.ToSharedRef(), FPaths::GetBaseFilename(Path));
}

void FLogiGeoBenchRunner::StartRun(const TSharedRef<FJsonObject>& Request, const FString& RequestName)
{
	const FString Cmd = Request->GetStringField(TEXT("cmd"));
	OutDir = Request->GetStringField(TEXT("out_dir"));
	IFileManager::Get().MakeDirectory(*OutDir, /*Tree=*/true);

	// Never depend on window focus: kill the background CPU throttle for this editor session and force the
	// perspective viewport realtime for the run's duration (removed in FinishRun).
	GetMutableDefault<UEditorPerformanceSettings>()->bThrottleCPUWhenNotForeground = false;
	if (FLevelEditorViewportClient* Client = PerspectiveViewport())
	{
		Client->AddRealtimeOverride(true, RealtimeOverrideName());
		bRealtimeOverridden = true;
	}

	// Disk-cache mode: flip the actor's Persistent Disk Cache to the request's setting through the proper
	// editor path (PostEditChangeProperty — a world-defining edit, so the Rust world rebuilds with/without
	// its cache_dir, which carries BOTH the byte cache and the persisted-geometry tier). `--no-cache` runs
	// therefore measure the true cold network path. The editor's prior setting is restored at FinishRun.
	bool bWantDiskCache = true;
	if (Request->TryGetBoolField(TEXT("disk_cache"), bWantDiskCache))
	{
		if (ALogiBakedGeoTerrain* Actor = FindGeoActor())
		{
			if (Actor->bGlobeDiskCache != bWantDiskCache)
			{
				bPrevDiskCache = Actor->bGlobeDiskCache;
				bDiskCacheOverridden = true;
				Actor->bGlobeDiskCache = bWantDiskCache;
				FProperty* Prop = ALogiBakedGeoTerrain::StaticClass()->FindPropertyByName(
					GET_MEMBER_NAME_CHECKED(ALogiBakedGeoTerrain, bGlobeDiskCache));
				FPropertyChangedEvent Evt(Prop);
				Actor->PostEditChangeProperty(Evt); // fires ReloadTerrain — the run starts on the rebuilt world
				UE_LOG(LogLogiBevyEditor, Log, TEXT("Geo bench: disk cache %s for this run (was %s)."),
					bWantDiskCache ? TEXT("ON") : TEXT("OFF"), bPrevDiskCache ? TEXT("on") : TEXT("off"));
			}
		}
	}

	if (Cmd == TEXT("snap"))
	{
		// One-shot: a single screenshot + sidecar of the CURRENT view, named by the request. Implemented as a
		// zero-station run so the capture flows through the same Snapshot path.
		RunRequest = Request;
		StationIndex = INDEX_NONE;
		FString Name = TEXT("snap");
		Request->TryGetStringField(TEXT("name"), Name);
		Snapshot(Name);
		FinishRun(TEXT("snap"));
		return;
	}
	if (Cmd == TEXT("fly"))
	{
		// Pre-baked camera-track playback (the "video" path): the CLI sampled the whole flight at the
		// capture rate, all geodesy done in Rust. We interpolate the camera between samples every frame
		// and capture a PNG + full-telemetry sidecar at each sample crossing.
		FlyTrack.Reset();
		const TArray<TSharedPtr<FJsonValue>>* Samples = nullptr;
		if (Request->TryGetArrayField(TEXT("samples"), Samples))
		{
			FlyTrack.Reserve(Samples->Num());
			for (const TSharedPtr<FJsonValue>& V : *Samples)
			{
				const TSharedPtr<FJsonObject> S = V->AsObject();
				if (!S.IsValid()) { continue; }
				const TArray<TSharedPtr<FJsonValue>>* Cam = nullptr;
				const TArray<TSharedPtr<FJsonValue>>* Look = nullptr;
				const TArray<TSharedPtr<FJsonValue>>* Up = nullptr;
				if (!S->TryGetArrayField(TEXT("cam"), Cam) || !S->TryGetArrayField(TEXT("look"), Look)
					|| !S->TryGetArrayField(TEXT("up"), Up) || Cam->Num() != 3 || Look->Num() != 3 || Up->Num() != 3)
				{
					continue;
				}
				FFlySample Sample;
				Sample.T = S->GetNumberField(TEXT("t"));
				Sample.Cam = FVector((*Cam)[0]->AsNumber(), (*Cam)[1]->AsNumber(), (*Cam)[2]->AsNumber());
				Sample.Look = FVector((*Look)[0]->AsNumber(), (*Look)[1]->AsNumber(), (*Look)[2]->AsNumber());
				Sample.Up = FVector((*Up)[0]->AsNumber(), (*Up)[1]->AsNumber(), (*Up)[2]->AsNumber());
				S->TryGetStringField(TEXT("leg"), Sample.Leg);
				FlyTrack.Add(MoveTemp(Sample));
			}
		}
		if (FlyTrack.Num() == 0)
		{
			UE_LOG(LogLogiBevyEditor, Warning, TEXT("Geo bench: fly request with no usable samples — ignored."));
			return;
		}
		IFileManager::Get().MakeDirectory(*(OutDir / TEXT("frames")), /*Tree=*/true);
		RunRequest = Request;
		StationIndex = INDEX_NONE;
		FlyIndex = 0;
		FlyFramesCaptured = 0;
		FlyFramesDropped = 0;
		FlyStartSeconds = FPlatformTime::Seconds();
		UE_LOG(LogLogiBevyEditor, Log, TEXT("Geo bench: fly started — %d samples over %.0fs → %s"),
			FlyTrack.Num(), FlyTrack.Last().T, *OutDir);
		return;
	}
	if (Cmd != TEXT("run"))
	{
		UE_LOG(LogLogiBevyEditor, Warning, TEXT("Geo bench: unknown cmd '%s' in %s — ignored."), *Cmd, *RequestName);
		return;
	}
	RunRequest = Request;
	SnapshotCounter = 0;
	UE_LOG(LogLogiBevyEditor, Log, TEXT("Geo bench: run started → %s"), *OutDir);
	BeginStation(0);
}

void FLogiGeoBenchRunner::BeginStation(int32 Index)
{
	const TArray<TSharedPtr<FJsonValue>>* Stations = nullptr;
	if (!RunRequest->TryGetArrayField(TEXT("stations"), Stations) || Index >= Stations->Num())
	{
		FinishRun(TEXT("all stations complete"));
		return;
	}
	StationIndex = Index;
	const TSharedPtr<FJsonObject> Station = (*Stations)[Index]->AsObject();

	// Camera: position + look-at (+ local geodetic up), all UE world cm/dirs (the CLI did the geodesy in
	// Rust — single-homed there). The up vector matters: the render frame is one ENU plane pinned at the
	// actor's origin, so far from it local up is NOT +Z — a plain (Look-Cam).Rotation() (world roll = 0)
	// rolls the horizon and frames the limb sideways (the tilted shots of the first 8-station run).
	const TArray<TSharedPtr<FJsonValue>>* Cam = nullptr;
	const TArray<TSharedPtr<FJsonValue>>* Look = nullptr;
	const TArray<TSharedPtr<FJsonValue>>* Up = nullptr;
	Station->TryGetArrayField(TEXT("cam"), Cam);
	Station->TryGetArrayField(TEXT("look"), Look);
	Station->TryGetArrayField(TEXT("up"), Up);
	if (Cam != nullptr && Look != nullptr && Cam->Num() == 3 && Look->Num() == 3)
	{
		const FVector CamPos((*Cam)[0]->AsNumber(), (*Cam)[1]->AsNumber(), (*Cam)[2]->AsNumber());
		const FVector LookAt((*Look)[0]->AsNumber(), (*Look)[1]->AsNumber(), (*Look)[2]->AsNumber());
		FRotator ViewRot;
		const FVector Forward = (LookAt - CamPos).GetSafeNormal();
		const FVector UpDir = (Up != nullptr && Up->Num() == 3)
			? FVector((*Up)[0]->AsNumber(), (*Up)[1]->AsNumber(), (*Up)[2]->AsNumber())
			: FVector::ZeroVector;
		if (!UpDir.IsNearlyZero() && FMath::Abs(FVector::DotProduct(Forward, UpDir)) < 0.99)
		{
			ViewRot = FRotationMatrix::MakeFromXZ(Forward, UpDir).Rotator();
		}
		else
		{
			// No up vector, or looking ~straight down/up (Forward ∥ Up — the orbit stations): the
			// orthogonalization is degenerate and any roll is fine. Plain world-axes rotation.
			ViewRot = Forward.Rotation();
		}
		if (FLevelEditorViewportClient* Client = PerspectiveViewport())
		{
			Client->SetViewLocation(CamPos);
			Client->SetViewRotation(ViewRot);
			Client->Invalidate();
		}
	}

	PendingSnapshotOffsets.Reset();
	const TArray<TSharedPtr<FJsonValue>>* Offsets = nullptr;
	if (Station->TryGetArrayField(TEXT("snapshots_s"), Offsets))
	{
		for (const TSharedPtr<FJsonValue>& V : *Offsets)
		{
			PendingSnapshotOffsets.Add(V->AsNumber());
		}
		PendingSnapshotOffsets.Sort();
	}
	SettleTimeoutSeconds = 150.0;
	Station->TryGetNumberField(TEXT("settle_timeout_s"), SettleTimeoutSeconds);
	StationStartSeconds = FPlatformTime::Seconds();
	RenderHistory.Reset();
	UE_LOG(LogLogiBevyEditor, Log, TEXT("Geo bench: station %d '%s'"), Index,
		*Station->GetStringField(TEXT("name")));
}

void FLogiGeoBenchRunner::TickStation()
{
	if (StationIndex == INDEX_NONE)
	{
		return;
	}
	const double Elapsed = FPlatformTime::Seconds() - StationStartSeconds;

	// Timed snapshots first (the convergence-history shots the diagnosis needs).
	while (PendingSnapshotOffsets.Num() > 0 && Elapsed >= PendingSnapshotOffsets[0])
	{
		Snapshot(FString::Printf(TEXT("t%ds"), FMath::RoundToInt(PendingSnapshotOffsets[0])));
		PendingSnapshotOffsets.RemoveAt(0);
	}

	// Settle: the streaming pipeline quiescent AND the published set flat for ~4 s of samples, AND a minimum
	// dwell with something actually RENDERED. The dwell + rendered guards close the fake-settle window the
	// 2026-06-11 runs hit twice: a station beginning during a world rebuild (the cache-flip reload) or editor
	// boot sees an EMPTY quiescent pipeline (nothing demanded yet, nothing rendered) and "settles" in 1-3 s
	// on a blank world; a fetch-stalled world (the pool-starvation era) looked identical. Genuine convergence
	// can't complete before the dwell anyway (the fastest warm orbit settle measured ~5.8 s).
	constexpr double MinSettleDwellSeconds = 12.0;
	FLogiGeoBenchMetrics M;
	const bool bHaveGlobe = SampleMetrics(M);
	// Stability sampling at 4 Hz regardless of tick rate: the ticker went per-frame for the fly path, which
	// silently shrank "16 samples" from the designed 4 s window to ~0.14 s — rate-limit the history instead.
	const double Now = FPlatformTime::Seconds();
	if (Now - LastStationSampleSeconds < 0.25)
	{
		return;
	}
	LastStationSampleSeconds = Now;
	RenderHistory.Add(M.Rendered);
	if (RenderHistory.Num() > 16)
	{
		RenderHistory.RemoveAt(0);
	}
	bool bStable = bHaveGlobe && RenderHistory.Num() == 16;
	for (int32 i = 1; bStable && i < RenderHistory.Num(); ++i)
	{
		bStable &= FMath::Abs(RenderHistory[i] - RenderHistory[0]) <= FMath::Max(2, RenderHistory[0] / 100);
	}
	const bool bQuiescent = bHaveGlobe && M.InFlight == 0 && M.Decoding == 0 && M.PendingBinds == 0
		&& M.PendingTexture == 0 && M.Fading == 0;
	const bool bMeaningful = Elapsed >= MinSettleDwellSeconds && M.Rendered > 0;

	if ((bQuiescent && bStable && bMeaningful) || Elapsed >= SettleTimeoutSeconds)
	{
		Snapshot(bQuiescent && bStable && bMeaningful ? TEXT("settled") : TEXT("timeout"));
		BeginStation(StationIndex + 1);
	}
}

void FLogiGeoBenchRunner::Snapshot(const FString& Tag)
{
	FString StationName = TEXT("adhoc");
	const TArray<TSharedPtr<FJsonValue>>* Stations = nullptr;
	if (RunRequest.IsValid() && RunRequest->TryGetArrayField(TEXT("stations"), Stations)
		&& Stations->IsValidIndex(StationIndex))
	{
		StationName = (*Stations)[StationIndex]->AsObject()->GetStringField(TEXT("name"));
	}
	const FString Base = FString::Printf(TEXT("%02d_%s_%s"), SnapshotCounter++, *StationName, *Tag);

	// PNG lands on the next rendered viewport frame; the sidecar samples NOW (same tick the request queues —
	// the one-frame skew is far below the 0.25 s sample cadence).
	FScreenshotRequest::RequestScreenshot(OutDir / Base + TEXT(".png"), /*bInShowUI=*/false, /*bAddFilenameSuffix=*/false);

	FLogiGeoBenchMetrics M;
	SampleMetrics(M);
	TSharedRef<FJsonObject> Sidecar = MetricsToJson(M);
	Sidecar->SetStringField(TEXT("station"), StationName);
	Sidecar->SetStringField(TEXT("tag"), Tag);
	Sidecar->SetStringField(TEXT("captured_utc"), FDateTime::UtcNow().ToIso8601());
	Sidecar->SetNumberField(TEXT("station_elapsed_s"),
		StationIndex == INDEX_NONE ? 0.0 : FPlatformTime::Seconds() - StationStartSeconds);
	// The ACTUAL camera at capture (UE cm + view angles): a shot whose pose drifted from the request's
	// station is attributable on sight — user navigation, a stray track playback, anything that fought
	// BeginStation's placement (the 2026-06-11 wrong-altitude "settled" shots took hours to attribute
	// without this).
	if (const FLevelEditorViewportClient* Client = PerspectiveViewport())
	{
		const FVector Cam = Client->GetViewLocation();
		const FRotator Rot = Client->GetViewRotation();
		TArray<TSharedPtr<FJsonValue>> CamArr;
		for (const double V : { Cam.X, Cam.Y, Cam.Z })
		{
			CamArr.Add(MakeShared<FJsonValueNumber>(V));
		}
		Sidecar->SetArrayField(TEXT("cam_ue"), CamArr);
		Sidecar->SetNumberField(TEXT("cam_yaw"), Rot.Yaw);
		Sidecar->SetNumberField(TEXT("cam_pitch"), Rot.Pitch);
	}
	// The SUN at capture: with the planet-true sky the atmosphere is a pure function of camera + sun, so a
	// shot that flips day↔twilight at a fixed camera is attributable the moment both poses are recorded
	// (the 2026-06-11 t10s-daylight/settled-twilight flip — suspect: the stream view following viewport
	// FOCUS while the station camera stays put, dragging the zenith sun with it).
	if (UWorld* W = GEditor ? GEditor->GetEditorWorldContext().World() : nullptr)
	{
		for (TActorIterator<ADirectionalLight> It(W); It; ++It)
		{
			const FRotator SunRot = (*It)->GetActorRotation();
			Sidecar->SetNumberField(TEXT("sun_yaw"), SunRot.Yaw);
			Sidecar->SetNumberField(TEXT("sun_pitch"), SunRot.Pitch);
			break;
		}
	}
	if (RunRequest.IsValid())
	{
		const TSharedPtr<FJsonObject>* Git = nullptr;
		if (RunRequest->TryGetObjectField(TEXT("git"), Git))
		{
			Sidecar->SetObjectField(TEXT("git"), *Git);
		}
		FString Label;
		if (RunRequest->TryGetStringField(TEXT("label"), Label))
		{
			Sidecar->SetStringField(TEXT("label"), Label);
		}
	}
	WriteJson(OutDir / Base + TEXT(".json"), Sidecar);
	UE_LOG(LogLogiBevyEditor, Log, TEXT("Geo bench: snapshot %s"), *Base);
}

void FLogiGeoBenchRunner::TickFly()
{
	const double Elapsed = FPlatformTime::Seconds() - FlyStartSeconds;

	// Smooth camera every frame: lerp between the samples bracketing `Elapsed` (the track is dense — one
	// sample per capture interval — so a plain lerp of position/look/up is visually smooth).
	{
		int32 Hi = FlyIndex;
		while (Hi < FlyTrack.Num() && FlyTrack[Hi].T < Elapsed)
		{
			++Hi;
		}
		const FFlySample& B = FlyTrack[FMath::Min(Hi, FlyTrack.Num() - 1)];
		const FFlySample& A = FlyTrack[FMath::Max(FMath::Min(Hi, FlyTrack.Num() - 1) - 1, 0)];
		const double Span = FMath::Max(B.T - A.T, 1e-6);
		const float S = static_cast<float>(FMath::Clamp((Elapsed - A.T) / Span, 0.0, 1.0));
		const FVector Cam = FMath::Lerp(A.Cam, B.Cam, S);
		const FVector Look = FMath::Lerp(A.Look, B.Look, S);
		const FVector Up = FMath::Lerp(A.Up, B.Up, S).GetSafeNormal();
		const FVector Forward = (Look - Cam).GetSafeNormal();
		if (FLevelEditorViewportClient* Client = PerspectiveViewport())
		{
			Client->SetViewLocation(Cam);
			Client->SetViewRotation((!Up.IsNearlyZero() && FMath::Abs(FVector::DotProduct(Forward, Up)) < 0.99)
				? FRotationMatrix::MakeFromXZ(Forward, Up).Rotator()
				: Forward.Rotation());
		}
	}

	// Capture every sample whose timestamp we crossed this frame: a PNG when the screenshot pipe is free
	// (one pending request at a time — a slow frame drops the PNG, never the telemetry), and a FULL metrics
	// sidecar ALWAYS, timestamped with the sample time + the actual capture time.
	while (FlyIndex < FlyTrack.Num() && FlyTrack[FlyIndex].T <= Elapsed)
	{
		const FFlySample& Sample = FlyTrack[FlyIndex];
		const FString Base = FString::Printf(TEXT("frames/%05d_%s"), FlyIndex, *Sample.Leg);
		bool bPng = !FScreenshotRequest::IsScreenshotRequested();
		if (bPng)
		{
			FScreenshotRequest::RequestScreenshot(OutDir / Base + TEXT(".png"), /*bInShowUI=*/false,
				/*bAddFilenameSuffix=*/false);
			++FlyFramesCaptured;
		}
		else
		{
			++FlyFramesDropped;
		}
		FLogiGeoBenchMetrics M;
		SampleMetrics(M);
		TSharedRef<FJsonObject> Sidecar = MetricsToJson(M);
		Sidecar->SetStringField(TEXT("leg"), Sample.Leg);
		Sidecar->SetNumberField(TEXT("t_s"), Sample.T);
		Sidecar->SetNumberField(TEXT("captured_at_s"), Elapsed);
		Sidecar->SetBoolField(TEXT("png"), bPng);
		Sidecar->SetStringField(TEXT("captured_utc"), FDateTime::UtcNow().ToIso8601());
		WriteJson(OutDir / Base + TEXT(".json"), Sidecar);
		++FlyIndex;
	}

	if (FlyIndex >= FlyTrack.Num())
	{
		FinishRun(TEXT("fly complete"));
	}
}

void FLogiGeoBenchRunner::FinishRun(const TCHAR* Reason)
{
	if (bRealtimeOverridden)
	{
		if (FLevelEditorViewportClient* Client = PerspectiveViewport())
		{
			Client->RemoveRealtimeOverride(RealtimeOverrideName());
		}
		bRealtimeOverridden = false;
	}
	if (bDiskCacheOverridden)
	{
		// Restore the editor's pre-run cache setting (another world-defining edit → rebuild; post-run, cheap).
		if (ALogiBakedGeoTerrain* Actor = FindGeoActor())
		{
			Actor->bGlobeDiskCache = bPrevDiskCache;
			FProperty* Prop = ALogiBakedGeoTerrain::StaticClass()->FindPropertyByName(
				GET_MEMBER_NAME_CHECKED(ALogiBakedGeoTerrain, bGlobeDiskCache));
			FPropertyChangedEvent Evt(Prop);
			Actor->PostEditChangeProperty(Evt);
			UE_LOG(LogLogiBevyEditor, Log, TEXT("Geo bench: disk cache restored to %s."),
				bPrevDiskCache ? TEXT("on") : TEXT("off"));
		}
		bDiskCacheOverridden = false;
	}
	const TSharedRef<FJsonObject> Done = MakeShared<FJsonObject>();
	Done->SetStringField(TEXT("reason"), Reason);
	Done->SetStringField(TEXT("completed_utc"), FDateTime::UtcNow().ToIso8601());
	Done->SetNumberField(TEXT("snapshots"), SnapshotCounter);
	if (FlyTrack.Num() > 0)
	{
		Done->SetNumberField(TEXT("frames"), FlyFramesCaptured);
		Done->SetNumberField(TEXT("frames_dropped"), FlyFramesDropped);
		UE_LOG(LogLogiBevyEditor, Log, TEXT("Geo bench: fly captured %d frames (%d dropped)"),
			FlyFramesCaptured, FlyFramesDropped);
	}
	WriteJson(OutDir / TEXT("run_complete.json"), Done);
	UE_LOG(LogLogiBevyEditor, Log, TEXT("Geo bench: run complete (%s) → %s"), Reason, *OutDir);
	RunRequest.Reset();
	StationIndex = INDEX_NONE;
	FlyTrack.Reset();
	FlyIndex = 0;
}

bool FLogiGeoBenchRunner::SampleMetrics(FLogiGeoBenchMetrics& Out) const
{
	if (ALogiBakedGeoTerrain* Actor = FindGeoActor())
	{
		return Actor->GetBenchMetrics(Out);
	}
	Out = FLogiGeoBenchMetrics{};
	return false;
}

ALogiBakedGeoTerrain* FLogiGeoBenchRunner::FindGeoActor() const
{
	if (GEditor == nullptr)
	{
		return nullptr;
	}
	UWorld* World = GEditor->GetEditorWorldContext().World();
	if (World == nullptr)
	{
		return nullptr;
	}
	for (TActorIterator<ALogiBakedGeoTerrain> It(World); It; ++It)
	{
		return *It;
	}
	return nullptr;
}

FLevelEditorViewportClient* FLogiGeoBenchRunner::PerspectiveViewport() const
{
	if (GEditor == nullptr)
	{
		return nullptr;
	}
	if (GCurrentLevelEditingViewportClient != nullptr && GCurrentLevelEditingViewportClient->IsPerspective())
	{
		return GCurrentLevelEditingViewportClient;
	}
	for (FLevelEditorViewportClient* Client : GEditor->GetLevelViewportClients())
	{
		if (Client != nullptr && Client->IsPerspective())
		{
			return Client;
		}
	}
	return nullptr;
}
