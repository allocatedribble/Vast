// GENERATED FILE — DO NOT EDIT BY HAND.
//
// Draco normally emits src/draco/draco_features.h from CMake (draco_options.cmake ::
// draco_set_optional_features) at configure time; we vendor only the decoder subset and build with
// `cc` (no CMake), so this is the equivalent file written out by hand for a FULL decode build.
//
// Feature set = Draco's default non-GLTF, non-emscripten configuration (every DRACO_* option ON
// except DRACO_GLTF_BITSTREAM and DRACO_TRANSCODER_SUPPORTED, both default-OFF). This is the
// broadest decoder: it accepts standard + predictive edgebreaker, point clouds, normal octahedral
// encoding, attribute (index/value) deduplication, and the pre-1.0 backward-compatible bitstream —
// i.e. anything Google Photorealistic 3D Tiles or Cesium World Terrain can hand us.
//
// DRACO_TRANSCODER_SUPPORTED is deliberately LEFT UNDEFINED: it pulls in draco/material, draco/texture,
// and draco/scene (full glTF transcode), none of which we vendor — KHR_draco_mesh_compression decode
// only needs the in-memory Mesh.
#ifndef DRACO_FEATURES_H_
#define DRACO_FEATURES_H_

#define DRACO_MESH_COMPRESSION_SUPPORTED
#define DRACO_NORMAL_ENCODING_SUPPORTED
#define DRACO_STANDARD_EDGEBREAKER_SUPPORTED
#define DRACO_PREDICTIVE_EDGEBREAKER_SUPPORTED
#define DRACO_POINT_CLOUD_COMPRESSION_SUPPORTED
#define DRACO_BACKWARDS_COMPATIBILITY_SUPPORTED
#define DRACO_ATTRIBUTE_INDICES_DEDUPLICATION_SUPPORTED
#define DRACO_ATTRIBUTE_VALUES_DEDUPLICATION_SUPPORTED

#endif  // DRACO_FEATURES_H_
