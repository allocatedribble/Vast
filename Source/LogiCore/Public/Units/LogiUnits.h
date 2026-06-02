// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"

// Lightweight unit conventions. Unreal world units are centimeters; logistics
// design reasons in meters/kilometers and kilograms. These aliases + constants
// keep conversions explicit and in one place. Kept header-only and POD (no UHT).

namespace LogiUnits
{
	// Length
	using FCentimeters = double;
	using FMeters = double;
	using FKilometers = double;

	constexpr double CentimetersPerMeter = 100.0;
	constexpr double MetersPerKilometer = 1000.0;
	constexpr double CentimetersPerKilometer = CentimetersPerMeter * MetersPerKilometer;

	FORCEINLINE constexpr FCentimeters MetersToCm(FMeters M) { return M * CentimetersPerMeter; }
	FORCEINLINE constexpr FMeters CmToMeters(FCentimeters Cm) { return Cm / CentimetersPerMeter; }
	FORCEINLINE constexpr FCentimeters KilometersToCm(FKilometers Km) { return Km * CentimetersPerKilometer; }

	// Mass (payload)
	using FKilograms = double;
	using FTonnes = double;
	constexpr double KilogramsPerTonne = 1000.0;
	FORCEINLINE constexpr FKilograms TonnesToKg(FTonnes T) { return T * KilogramsPerTonne; }
}
