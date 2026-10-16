// made by CasualArclamp
//! Builds the vendored libopus (`third_party/opus`, tag v1.6.1) as a static library with
//! Opus' own CMake project: float API, no programs, no tests, no DNN extensions (DRED,
//! OSCE and deep PLC need downloaded model weights and are not needed to play radio).
//!
//! Taken from DecDRM's `decdrm-opus-sys` build script (same author, GPL-3.0-or-later).

use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let opus_root = manifest_dir.join("../../third_party/opus");
    if !opus_root.join("include/opus.h").is_file() {
        panic!(
            "libopus submodule at {} is empty; run `git submodule update --init third_party/opus`",
            opus_root.display()
        );
    }

    println!("cargo:rerun-if-changed=build.rs");
    for dir in ["CMakeLists.txt", "cmake", "include", "src", "celt", "silk"] {
        println!("cargo:rerun-if-changed={}", opus_root.join(dir).display());
    }

    // On MSVC the C runtime must be the one Rust links: the static one (/MT) in builds
    // with `-C target-feature=+crt-static` (portable executables), else the DLL (/MD).
    // Opus' CMakeLists sets CMAKE_MSVC_RUNTIME_LIBRARY from its OPUS_STATIC_RUNTIME
    // option, overriding the compiler flags.
    let static_crt =
        env::var("CARGO_CFG_TARGET_FEATURE").is_ok_and(|f| f.split(',').any(|x| x == "crt-static"));
    let dst = cmake::Config::new(&opus_root)
        // Always optimised (the codec is far too slow unoptimised) and, on MSVC, always a
        // release C runtime.
        .profile("Release")
        .static_crt(static_crt)
        .define("OPUS_STATIC_RUNTIME", if static_crt { "ON" } else { "OFF" })
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("OPUS_BUILD_SHARED_LIBRARY", "OFF")
        .define("OPUS_BUILD_PROGRAMS", "OFF")
        .define("OPUS_BUILD_TESTING", "OFF")
        .define("BUILD_TESTING", "OFF")
        .define("OPUS_INSTALL_PKG_CONFIG_MODULE", "OFF")
        .define("OPUS_INSTALL_CMAKE_CONFIG_MODULE", "OFF")
        .define("OPUS_FIXED_POINT", "OFF")
        .define("OPUS_ENABLE_FLOAT_API", "ON")
        .define("OPUS_DRED", "OFF")
        .define("OPUS_OSCE", "OFF")
        .define("OPUS_CUSTOM_MODES", "OFF")
        .build();

    // GNUInstallDirs picks `lib` or `lib64` depending on the distribution.
    for sub in ["lib", "lib64"] {
        let p = dst.join(sub);
        if p.is_dir() {
            println!("cargo:rustc-link-search=native={}", p.display());
        }
    }
    println!("cargo:rustc-link-lib=static=opus");
    println!("cargo:include={}", dst.join("include").display());
}
