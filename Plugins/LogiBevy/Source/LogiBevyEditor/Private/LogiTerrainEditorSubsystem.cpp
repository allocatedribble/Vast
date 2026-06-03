// Copyright LogiUE. All Rights Reserved.

#include "LogiTerrainEditorSubsystem.h"
#include "LogiTerrainSubsystem.h"

#include "Editor.h"              // GEditor
#include "Editor/EditorEngine.h" // UEditorEngine::PlayWorld
#include "Engine/World.h"
#include "Framework/Docking/TabManager.h"
#include "WorkspaceMenuStructure.h"
#include "WorkspaceMenuStructureModule.h"
#include "Widgets/Docking/SDockTab.h"
#include "Widgets/Layout/SScrollBox.h"
#include "Widgets/SBoxPanel.h"
#include "Widgets/Text/STextBlock.h"
#include "Widgets/Views/STableRow.h"

#define LOCTEXT_NAMESPACE "LogiBevyEditor"

const FName ULogiTerrainEditorSubsystem::TabId(TEXT("LogiTerrainPanel"));

void ULogiTerrainEditorSubsystem::Initialize(FSubsystemCollectionBase& Collection)
{
	Super::Initialize(Collection);

	FGlobalTabmanager::Get()
		->RegisterNomadTabSpawner(TabId, FOnSpawnTab::CreateUObject(this, &ULogiTerrainEditorSubsystem::SpawnTab))
		.SetDisplayName(LOCTEXT("TerrainTitle", "Logi Terrain"))
		.SetTooltipText(LOCTEXT("TerrainTip", "Rust terrain kernel telemetry + resident tiles (ADR-0007)."))
		.SetGroup(WorkspaceMenu::GetMenuStructure().GetToolsCategory());

	TickerHandle = FTSTicker::GetCoreTicker().AddTicker(
		FTickerDelegate::CreateUObject(this, &ULogiTerrainEditorSubsystem::Tick), 0.25f);
}

void ULogiTerrainEditorSubsystem::Deinitialize()
{
	if (TickerHandle.IsValid())
	{
		FTSTicker::GetCoreTicker().RemoveTicker(TickerHandle);
		TickerHandle.Reset();
	}
	if (FGlobalTabmanager::Get()->HasTabSpawner(TabId))
	{
		FGlobalTabmanager::Get()->UnregisterNomadTabSpawner(TabId);
	}
	Super::Deinitialize();
}

TSharedRef<SDockTab> ULogiTerrainEditorSubsystem::SpawnTab(const FSpawnTabArgs& Args)
{
	using FTileListView = SListView<TSharedPtr<FLogiTerrainTileRow>>;
	ListView = SNew(FTileListView)
		.ListItemsSource(&Items)
		.SelectionMode(ESelectionMode::Single)
		.OnGenerateRow(FTileListView::FOnGenerateRow::CreateUObject(this, &ULogiTerrainEditorSubsystem::GenerateRow));

	HeaderText = SNew(STextBlock).Text(LOCTEXT("TerrainHeader", "Logi Terrain (enable Terrain + PIE to stream)"));

	return SNew(SDockTab)
		.TabRole(ETabRole::NomadTab)
		[
			SNew(SVerticalBox)
			+ SVerticalBox::Slot().AutoHeight().Padding(4.0f)
			[
				HeaderText.ToSharedRef()
			]
			+ SVerticalBox::Slot().FillHeight(1.0f)
			[
				SNew(SScrollBox)
				+ SScrollBox::Slot()
				[
					ListView.ToSharedRef()
				]
			]
		];
}

TSharedRef<ITableRow> ULogiTerrainEditorSubsystem::GenerateRow(
	TSharedPtr<FLogiTerrainTileRow> Item, const TSharedRef<STableViewBase>& OwnerTable)
{
	const FLogiTerrainTileRow R = Item.IsValid() ? *Item : FLogiTerrainTileRow();
	return SNew(STableRow<TSharedPtr<FLogiTerrainTileRow>>, OwnerTable)
		[
			SNew(STextBlock).Text(FText::FromString(FString::Printf(
				TEXT("tile %llu   lod %u   center (%.0f, %.0f, %.0f)   r=%.0f"),
				R.Key, R.Lod, R.Center.X, R.Center.Y, R.Center.Z, R.Radius)))
		];
}

bool ULogiTerrainEditorSubsystem::Tick(float /*DeltaTime*/)
{
	Refresh();
	return true;
}

void ULogiTerrainEditorSubsystem::Refresh()
{
	if (!ListView.IsValid())
	{
		return;
	}

	ULogiTerrainSubsystem* Terrain = nullptr;
	if (GEditor != nullptr && GEditor->PlayWorld != nullptr)
	{
		Terrain = GEditor->PlayWorld->GetSubsystem<ULogiTerrainSubsystem>();
	}

	FLogiTerrainStats Stats;
	TArray<FLogiTerrainTileRow> Rows;
	if (Terrain != nullptr)
	{
		Terrain->GetEditorTerrainState(Stats, Rows);
	}

	Items.Reset(Rows.Num());
	for (const FLogiTerrainTileRow& R : Rows)
	{
		Items.Add(MakeShared<FLogiTerrainTileRow>(R));
	}
	ListView->RequestListRefresh();

	if (HeaderText.IsValid())
	{
		HeaderText->SetText(FText::FromString(Terrain != nullptr
			? FString::Printf(
				TEXT("Logi Terrain  —  resident %d   in-flight %d   %.1f MiB (peak %.1f)   evict %lld   fallback %lld   frame %llu"),
				Stats.ResidentTiles, Stats.InFlight, Stats.BytesResident / 1048576.0,
				Stats.PeakBytesResident / 1048576.0, Stats.Evictions, Stats.Fallbacks, Stats.FrameId)
			: TEXT("Logi Terrain  —  enable Terrain in Project Settings, then PIE, to stream")));
	}
}

#undef LOCTEXT_NAMESPACE
