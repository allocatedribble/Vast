// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyEditorSubsystem.h"
#include "LogiBevyEntitySelectionProxy.h"

#include "Editor.h"               // GEditor
#include "Editor/EditorEngine.h"  // UEditorEngine::PlayWorld
#include "Engine/World.h"
#include "Framework/Docking/TabManager.h"
#include "Modules/ModuleManager.h"
#include "PropertyEditorModule.h"
#include "IDetailsView.h"
#include "WorkspaceMenuStructure.h"
#include "WorkspaceMenuStructureModule.h"
#include "Widgets/Docking/SDockTab.h"
#include "Widgets/Layout/SScrollBox.h"
#include "Widgets/SBoxPanel.h"
#include "Widgets/Text/STextBlock.h"
#include "Widgets/Input/SCheckBox.h"
#include "Widgets/Views/STableRow.h"

#define LOCTEXT_NAMESPACE "LogiBevyEditor"

const FName ULogiBevyEditorSubsystem::TabId(TEXT("LogiBevyEntityOutliner"));

void ULogiBevyEditorSubsystem::Initialize(FSubsystemCollectionBase& Collection)
{
	Super::Initialize(Collection);

	SelectionProxy = NewObject<ULogiBevyEntitySelectionProxy>(this);

	FGlobalTabmanager::Get()
		->RegisterNomadTabSpawner(TabId, FOnSpawnTab::CreateUObject(this, &ULogiBevyEditorSubsystem::SpawnOutlinerTab))
		.SetDisplayName(LOCTEXT("OutlinerTitle", "Bevy Entity Outliner"))
		.SetTooltipText(LOCTEXT("OutlinerTooltip", "Inspect Rust-owned Bevy entities (ADR-0006)."))
		.SetGroup(WorkspaceMenu::GetMenuStructure().GetToolsCategory());

	TickerHandle = FTSTicker::GetCoreTicker().AddTicker(
		FTickerDelegate::CreateUObject(this, &ULogiBevyEditorSubsystem::Tick), 0.2f);
}

void ULogiBevyEditorSubsystem::Deinitialize()
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

ULogiBevySubsystem* ULogiBevyEditorSubsystem::FindSimSubsystem() const
{
	if (GEditor != nullptr && GEditor->PlayWorld != nullptr)
	{
		return GEditor->PlayWorld->GetSubsystem<ULogiBevySubsystem>();
	}
	return nullptr;
}

TSharedRef<SDockTab> ULogiBevyEditorSubsystem::SpawnOutlinerTab(const FSpawnTabArgs& Args)
{
	FPropertyEditorModule& PropertyEditor =
		FModuleManager::LoadModuleChecked<FPropertyEditorModule>("PropertyEditor");
	FDetailsViewArgs DetailsArgs;
	DetailsArgs.bAllowSearch = false;
	DetailsArgs.bHideSelectionTip = true;
	DetailsArgs.NameAreaSettings = FDetailsViewArgs::HideNameArea;
	DetailsView = PropertyEditor.CreateDetailView(DetailsArgs);
	DetailsView->SetObject(SelectionProxy);

	ListView = SNew(SListView<TSharedPtr<FLogiBevyEntityRow>>)
		.ListItemsSource(&Items)
		.SelectionMode(ESelectionMode::Single)
		.OnGenerateRow(this, &ULogiBevyEditorSubsystem::GenerateRow)
		.OnSelectionChanged(this, &ULogiBevyEditorSubsystem::OnSelectionChanged);

	HeaderText = SNew(STextBlock).Text(LOCTEXT("Header", "Bevy Entity Outliner (start PIE to inspect)"));

	return SNew(SDockTab)
		.TabRole(ETabRole::NomadTab)
		[
			SNew(SVerticalBox)
			+ SVerticalBox::Slot().AutoHeight().Padding(4.0f)
			[
				HeaderText.ToSharedRef()
			]
			+ SVerticalBox::Slot().FillHeight(0.6f)
			[
				SNew(SScrollBox)
				+ SScrollBox::Slot()
				[
					ListView.ToSharedRef()
				]
			]
			+ SVerticalBox::Slot().FillHeight(0.4f)
			[
				DetailsView.ToSharedRef()
			]
		];
}

TSharedRef<ITableRow> ULogiBevyEditorSubsystem::GenerateRow(
	TSharedPtr<FLogiBevyEntityRow> Item, const TSharedRef<STableViewBase>& OwnerTable)
{
	const FLogiBevyEntityRow Row = Item.IsValid() ? *Item : FLogiBevyEntityRow();
	return SNew(STableRow<TSharedPtr<FLogiBevyEntityRow>>, OwnerTable)
		[
			SNew(SHorizontalBox)
			+ SHorizontalBox::Slot().AutoWidth().Padding(2.0f)
			[
				SNew(SCheckBox)
				.IsChecked(Row.bPinned ? ECheckBoxState::Checked : ECheckBoxState::Unchecked)
				.OnCheckStateChanged_Lambda([this, Item](ECheckBoxState State)
				{
					if (ULogiBevySubsystem* Sim = FindSimSubsystem())
					{
						if (Item.IsValid())
						{
							Sim->EditorSetPin(*Item, State == ECheckBoxState::Checked);
						}
					}
				})
			]
			+ SHorizontalBox::Slot().FillWidth(0.32f).Padding(2.0f)
			[
				SNew(STextBlock).Text(FText::FromString(
					FString::Printf(TEXT("Entity %llu (kind %u)"), Row.Index, Row.Kind)))
			]
			+ SHorizontalBox::Slot().FillWidth(0.16f).Padding(2.0f)
			[
				SNew(STextBlock).Text(FText::FromString(FString::Printf(TEXT("tier %u"), Row.Tier)))
			]
			+ SHorizontalBox::Slot().FillWidth(0.2f).Padding(2.0f)
			[
				SNew(STextBlock).Text(FText::FromString(FString::Printf(TEXT("sig %.3f"), Row.Significance)))
			]
			+ SHorizontalBox::Slot().FillWidth(0.32f).Padding(2.0f)
			[
				SNew(STextBlock).Text(FText::FromString(FString::Printf(
					TEXT("(%.0f, %.0f, %.0f)"), Row.Position.X, Row.Position.Y, Row.Position.Z)))
			]
		];
}

void ULogiBevyEditorSubsystem::OnSelectionChanged(TSharedPtr<FLogiBevyEntityRow> Item, ESelectInfo::Type)
{
	if (SelectionProxy == nullptr || !Item.IsValid())
	{
		return;
	}
	const FLogiBevyEntityRow& R = *Item;
	SelectionProxy->EntityId =
		FString::Printf(TEXT("idx=%llu gen=%u shard=%u kind=%u"), R.Index, R.Generation, R.Shard, R.Kind);
	SelectionProxy->Index = static_cast<int64>(R.Index);
	SelectionProxy->Kind = static_cast<int32>(R.Kind);
	SelectionProxy->Significance = R.Significance;
	SelectionProxy->RepresentationTier = FString::Printf(TEXT("%u"), R.Tier);
	SelectionProxy->Position = R.Position;
	SelectionProxy->bPinned = R.bPinned;
	if (DetailsView.IsValid())
	{
		DetailsView->SetObject(SelectionProxy, true);
	}
}

bool ULogiBevyEditorSubsystem::Tick(float /*DeltaTime*/)
{
	RefreshEntities();
	return true; // keep ticking
}

void ULogiBevyEditorSubsystem::RefreshEntities()
{
	if (!ListView.IsValid())
	{
		return; // tab not open
	}

	ULogiBevySubsystem* Sim = FindSimSubsystem();
	int32 Total = 0;
	TArray<FLogiBevyEntityRow> Rows;
	if (Sim != nullptr)
	{
		Sim->EditorQueryEntities(0, 500, Rows, Total);
	}

	Items.Reset(Rows.Num());
	for (const FLogiBevyEntityRow& R : Rows)
	{
		Items.Add(MakeShared<FLogiBevyEntityRow>(R));
	}
	ListView->RequestListRefresh();

	if (HeaderText.IsValid())
	{
		const int64 SimTick = (Sim != nullptr) ? Sim->GetSimTick() : 0;
		HeaderText->SetText(FText::FromString(FString::Printf(
			TEXT("Bevy Entity Outliner  —  %d shown / %d total  —  sim tick %lld%s"),
			Items.Num(), Total, SimTick, (Sim != nullptr) ? TEXT("") : TEXT("   (start PIE to inspect)"))));
	}
}

#undef LOCTEXT_NAMESPACE
