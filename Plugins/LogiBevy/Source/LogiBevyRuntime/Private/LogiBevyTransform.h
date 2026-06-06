// Copyright LogiUE. All Rights Reserved.

#pragma once

#include "CoreMinimal.h"

// Shared snapshot-transform decode for the representation lanes (audit M1): the instanced renderer and the
// pooled-actor lane both turn a Rust row-major 3x4 affine + the frame origin into a world FTransform. One
// home so the actor lane applies the SAME rotation/scale the ISM lane does (it previously dropped both).

namespace LogiBevy
{
	/// World-space FTransform from a row-major 3x4 (column-vector convention: world = R*S*local + T),
	/// rebased by adding the frame origin (P0 #6). UE's FMatrix is row-vector, so the 3x3 is transposed
	/// (cols 0-2 of the source rotation*scale go into UE rows) and translation goes in row 3.
	FORCEINLINE FTransform LogiWorldTransformFrom3x4(const float M[12], const FVector& Origin)
	{
		FMatrix Mat(EForceInit::ForceInitToZero);
		Mat.M[0][0] = M[0]; Mat.M[0][1] = M[4]; Mat.M[0][2] = M[8];
		Mat.M[1][0] = M[1]; Mat.M[1][1] = M[5]; Mat.M[1][2] = M[9];
		Mat.M[2][0] = M[2]; Mat.M[2][1] = M[6]; Mat.M[2][2] = M[10];
		Mat.M[3][0] = static_cast<double>(M[3]) + Origin.X;
		Mat.M[3][1] = static_cast<double>(M[7]) + Origin.Y;
		Mat.M[3][2] = static_cast<double>(M[11]) + Origin.Z;
		// Projective column is explicitly affine (audit C++ low): no perspective, homogeneous w = 1.
		Mat.M[0][3] = 0.0; Mat.M[1][3] = 0.0; Mat.M[2][3] = 0.0;
		Mat.M[3][3] = 1.0;
		return FTransform(Mat);
	}
}
