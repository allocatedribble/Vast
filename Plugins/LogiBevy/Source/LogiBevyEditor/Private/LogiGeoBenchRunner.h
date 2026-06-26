// Copyright LogiUE. All Rights Reserved.
#pragma once

#include "CoreMinimal.h"
#include "Containers/Ticker.h"
#include "Dom/JsonObject.h"

class ALogiBakedGeoTerrain;
class FLevelEditorViewportClient;

/**
 * Geo bench runner — the in-engine half of the `geo-bench` harness (Docs/Testing/Geo-Bench.md).
 *
 * Watches `<Project>/Bench/_inbox/` for JSON command files dropped by the Rust CLI (or by hand):
 *
 *   {"cmd":"run","out_dir":"<abs>","label":"...","git":{...},
 *    "stations":[{"name":"orbit_na","cam":[x,y,z],"look":[x,y,z],
 *                 "snapshots_s":[10,30],"settle_timeout_s":150}, ...]}
 *   {"cmd":"snap","name":"adhoc","out_dir":"<abs>"}
 *
 * For each station it drives the level-editor viewport camera (realtime forced via an override, so runs
 * never depend on window focus), samples [FLogiGeoBenchMetrics] every tick, captures screenshots at the
 * configured offsets and at SETTLE (streaming quiescent + stable render count) or timeout, and writes a
 * JSON metrics sidecar next to every PNG. A `run_complete.json` marker ends the run for the CLI.
 *
 * Editor-only tooling (this module never ships); the file inbox — not MCP — is the contract, so runs are
 * reproducible from any driver: the CLI, a script, or a hand-written JSON.
 */
class FLogiGeoBenchRunner
{
public:
	static void Init();
	static void Shutdown();

private:
	bool Tick(float DeltaSeconds);
	void PollInbox();
	void StartRun(const TSharedRef<FJsonObject>& Request, const FString& RequestName);
	void TickStation();
	void BeginStation(int32 Index);
	void Snapshot(const FString& Tag);
	void TickFly();
	void FinishRun(const TCHAR* Reason);
	bool SampleMetrics(struct FLogiGeoBenchMetrics& Out) const;
	ALogiBakedGeoTerrain* FindGeoActor() const;
	FLevelEditorViewportClient* PerspectiveViewport() const;

	FTSTicker::FDelegateHandle TickerHandle;
	double LastInboxPollSeconds = 0.0; // the ticker runs per-frame (the fly path needs it); inbox scans stay ~4 Hz

	// ---- Active run state (Idle when RunRequest is unset) ----
	TSharedPtr<FJsonObject> RunRequest;
	FString OutDir;
	int32 StationIndex = INDEX_NONE;
	double StationStartSeconds = 0.0;
	TArray<double> PendingSnapshotOffsets; // remaining timed snapshots for the current station (sorted)
	double SettleTimeoutSeconds = 150.0;
	TArray<int32> RenderHistory; // recent Rendered samples — settle = quiescent pipeline + flat history
	double LastStationSampleSeconds = 0.0; // rate-limits the history to 4 Hz (the ticker is per-frame)
	bool bRealtimeOverridden = false;
	int32 SnapshotCounter = 0;

	// Disk-cache mode for THIS run (request "disk_cache"): the actor's Persistent Disk Cache is flipped via
	// PostEditChangeProperty (a world-defining edit → the Rust world rebuilds with/without cache_dir, which
	// also gates the persisted-geometry tier) and the editor's prior setting is restored at FinishRun.
	bool bDiskCacheOverridden = false;
	bool bPrevDiskCache = false;

	// ---- Fly (pre-baked camera-track playback — the "video" path) ----
	/** One track sample (the CLI did ALL geodesy; this is UE world cm + a unit local-up). */
	struct FFlySample
	{
		double T = 0.0;
		FVector Cam = FVector::ZeroVector;
		FVector Look = FVector::ZeroVector;
		FVector Up = FVector::UpVector;
		FString Leg;
	};
	TArray<FFlySample> FlyTrack; // non-empty ⇒ the active run is a fly
	int32 FlyIndex = 0;          // next sample to CAPTURE (the camera interpolates between samples every frame)
	double FlyStartSeconds = 0.0;
	int32 FlyFramesCaptured = 0;
	int32 FlyFramesDropped = 0; // PNGs skipped because the previous screenshot request was still pending
};
