// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"
#include "EditorSubsystem.h"
#include "Containers/Ticker.h"
#include "Widgets/Views/SListView.h"
#include "LogiBevySubsystem.h" // FLogiBevyEntityRow
#include "LogiBevyEditorSubsystem.generated.h"

class SDockTab;
class FSpawnTabArgs;
class ITableRow;
class STableViewBase;
class IDetailsView;
class STextBlock;
class ULogiBevyEntitySelectionProxy;

/**
 * Editor subsystem hosting the "Bevy Entity Outliner" tab (Window menu). It samples
 * the running PIE sim through ULogiBevySubsystem::EditorQueryEntities (ADR-0006) at
 * a few Hz and shows VIRTUAL rows -- no per-entity UObject, and exactly ONE
 * transient selection proxy bound to a Details view.
 */
UCLASS()
class ULogiBevyEditorSubsystem : public UEditorSubsystem
{
	GENERATED_BODY()

public:
	virtual void Initialize(FSubsystemCollectionBase& Collection) override;
	virtual void Deinitialize() override;

private:
	TSharedRef<SDockTab> SpawnOutlinerTab(const FSpawnTabArgs& Args);
	TSharedRef<ITableRow> GenerateRow(TSharedPtr<FLogiBevyEntityRow> Item, const TSharedRef<STableViewBase>& OwnerTable);
	void OnSelectionChanged(TSharedPtr<FLogiBevyEntityRow> Item, ESelectInfo::Type SelectInfo);
	bool Tick(float DeltaTime);
	void RefreshEntities();
	ULogiBevySubsystem* FindSimSubsystem() const;

	static const FName TabId;

	TArray<TSharedPtr<FLogiBevyEntityRow>> Items;
	TSharedPtr<SListView<TSharedPtr<FLogiBevyEntityRow>>> ListView;
	TSharedPtr<IDetailsView> DetailsView;
	TSharedPtr<STextBlock> HeaderText;

	UPROPERTY()
	TObjectPtr<ULogiBevyEntitySelectionProxy> SelectionProxy = nullptr;

	FTSTicker::FDelegateHandle TickerHandle;
};
