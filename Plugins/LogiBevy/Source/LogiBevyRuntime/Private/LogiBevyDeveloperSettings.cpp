// Copyright LogiUE. All Rights Reserved.

#include "LogiBevyDeveloperSettings.h"

// Most members are config UPROPERTYs with inline defaults; UHT generates the boilerplate. This
// translation unit anchors the generated code and implements the editor-only live-reload hook.

#if WITH_EDITOR
ULogiBevyDeveloperSettings::FOnLogiSettingsChanged ULogiBevyDeveloperSettings::OnSettingsChanged;

void ULogiBevyDeveloperSettings::PostEditChangeProperty(FPropertyChangedEvent& PropertyChangedEvent)
{
	// Super persists the edited config value; the in-memory CDO already holds the new value, so a
	// subscriber re-reading GetDefault<ULogiBevyDeveloperSettings>() during reload sees it immediately.
	Super::PostEditChangeProperty(PropertyChangedEvent);

	// Notify the live world subsystems that a setting changed, carrying the property's name so each can
	// reload only the system it owns (and fall back to a full reload for anything it doesn't recognise).
	// The settings object never reaches into the subsystems itself — this keeps the dependency one-way.
	const FName PropertyName = PropertyChangedEvent.GetMemberPropertyName();
	OnSettingsChanged.Broadcast(PropertyName);
}
#endif
