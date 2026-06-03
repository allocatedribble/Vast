// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "EditorSubsystem.h"
#include "Containers/Ticker.h"
#include "Widgets/Views/SListView.h"
#include "LogiTerrainSubsystem.h" // FLogiTerrainTileRow, FLogiTerrainStats
#include "LogiTerrainEditorSubsystem.generated.h"

class SDockTab;
class FSpawnTabArgs;
class ITableRow;
class STableViewBase;
class STextBlock;

/**
 * Editor subsystem hosting the "Logi Terrain" tab (Window -> Tools). Read-only:
 * samples the running PIE terrain kernel via ULogiTerrainSubsystem::GetEditorTerrainState
 * (ADR-0006/0007) at a few Hz and shows the telemetry + resident tiles. The debug tool
 * for diagnosing the streaming + coordinate mapping; tile picking comes later.
 */
UCLASS()
class ULogiTerrainEditorSubsystem : public UEditorSubsystem
{
	GENERATED_BODY()

public:
	virtual void Initialize(FSubsystemCollectionBase& Collection) override;
	virtual void Deinitialize() override;

private:
	TSharedRef<SDockTab> SpawnTab(const FSpawnTabArgs& Args);
	TSharedRef<ITableRow> GenerateRow(TSharedPtr<FLogiTerrainTileRow> Item, const TSharedRef<STableViewBase>& OwnerTable);
	bool Tick(float DeltaTime);
	void Refresh();

	static const FName TabId;

	TArray<TSharedPtr<FLogiTerrainTileRow>> Items;
	TSharedPtr<SListView<TSharedPtr<FLogiTerrainTileRow>>> ListView;
	TSharedPtr<STextBlock> HeaderText;
	FTSTicker::FDelegateHandle TickerHandle;
};
