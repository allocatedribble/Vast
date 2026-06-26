// Build the vendored Google Draco DECODER subset (vendor/draco/, Apache-2.0, v1.5.7) plus our C ABI
// shim into one static library that the crate links. No CMake: we compile the decode source tree with
// `cc` directly, which is enough because the decode path is self-contained once DRACO_TRANSCODER_SUPPORTED
// stays off (that flag is what pulls in draco/material, draco/texture, draco/scene, draco/io — none
// vendored). See vendor/draco/src/draco/draco_features.h for the exact feature set.

use std::path::{Path, PathBuf};

// The decode-relevant Draco subdirectories we vendor and compile. Everything outside these (io, tools,
// material, texture, scene, animation, unity, maya, javascript) is encoder/transcoder/IO-only and not
// reachable from Decoder::DecodeMeshFromBuffer with the transcoder feature off.
//
// The sweep is DIRECTORY-granular, so encoder `.cc` TUs living inside these dirs (e.g. compression/'s
// *_encoder.cc) compile too — accepted: per-file trimming would make every re-vendor a curation chore,
// and the static-lib link pulls only referenced objects into the cdylib, so the ride-along TUs cost
// build time, not shipped bytes.
//
// Two files were deleted from the vendored mesh/ dir because they #include "draco/texture/..."
// UNCONDITIONALLY (not behind DRACO_TRANSCODER_SUPPORTED) yet are off the decode path:
// mesh_are_equivalent.{cc,h} (mesh equality testing) and mesh_splitter.{cc,h} (transcoder mesh split).
// A future re-vendor of Draco must delete them again, or compilation fails on the missing texture/ dir.
const DRACO_DIRS: &[&str] = &["core", "attributes", "compression", "mesh", "metadata", "point_cloud"];

// Collect every non-test `.cc` under `dir` (recursively) into `out`. We skip `*_test.cc` (already
// stripped from the vendor tree, but defended here too) so a future re-vendor that forgets to strip
// tests still produces a clean library.
fn collect_cc(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => panic!("logi_draco build: cannot read vendored dir {}: {e}", dir.display()),
    };
    for entry in entries {
        let path = entry.expect("logi_draco build: dir entry").path();
        if path.is_dir() {
            collect_cc(&path, out);
        } else if path.extension().is_some_and(|x| x == "cc") {
            let is_test = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("_test.cc"));
            if !is_test {
                out.push(path);
            }
        }
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let draco_root = manifest.join("vendor/draco");
    let draco_src = draco_root.join("src");
    let shim = manifest.join("cpp/logi_draco_shim.cpp");

    // Gather the Draco decode TUs + our shim.
    let mut sources: Vec<PathBuf> = Vec::new();
    for d in DRACO_DIRS {
        collect_cc(&draco_src.join("draco").join(d), &mut sources);
    }
    assert!(
        !sources.is_empty(),
        "logi_draco build: found 0 vendored Draco .cc files under {} — is vendor/draco/ populated?",
        draco_src.display()
    );
    sources.push(shim.clone());

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++14") // Draco is C++11; c++14 is the safe floor across MSVC/clang/gcc.
        .include(&draco_src) // resolves `#include "draco/..."`
        .include(manifest.join("cpp")) // resolves `#include "logi_draco_shim.h"`
        .warnings(false) // third-party: don't surface Draco's own warnings as noise
        .files(&sources);

    // MSVC specifics: exceptions are ON (the shim's try/catch is load-bearing) and Windows.h macro
    // hygiene in case any TU transitively includes it.
    if build.get_compiler().is_like_msvc() {
        build.flag("/EHsc");
        build.define("NOMINMAX", None);
        build.define("WIN32_LEAN_AND_MEAN", None);
    }

    build.compile("logi_draco_native"); // -> liblogi_draco_native.a / logi_draco_native.lib

    // Rebuild triggers: the shim, the header, and the vendored feature header. (The full vendored
    // source rarely changes; cargo already reruns when build.rs itself changes, and re-vendoring
    // touches these files. We avoid emitting rerun-if-changed for ~89 files to keep the line lean.)
    println!("cargo:rerun-if-changed={}", shim.display());
    println!(
        "cargo:rerun-if-changed={}",
        manifest.join("cpp/logi_draco_shim.h").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        draco_src.join("draco/draco_features.h").display()
    );
    println!("cargo:rerun-if-changed=build.rs");
}
