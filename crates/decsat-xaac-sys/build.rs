// made by CasualArclamp
//! Builds the *decoder* half of the vendored libxaac (`third_party/libxaac`, tag v0.1.13,
//! Apache-2.0) as a static C library.
//!
//! The source list is read from libxaac's own CMake files — `common/common.cmake`,
//! `decoder/libxaacdec.cmake`, `decoder/drc_src/libxaacdec_drc.cmake` and
//! `decoder/x86/libxaacdec_x86.cmake` (the generic C kernels; the ARM assembly is not
//! used) — so an update that adds a file still builds. The definitions follow libxaac's
//! `cmake/utils.cmake`: `LOUDNESS_LEVELING_SUPPORT` everywhere, the x86 ones on x86, and
//! `-fwrapv` where the compiler takes it. The encoder, test benches and fuzzers are not
//! compiled. (The approach is DecDRM's `decdrm-xaac-sys`, which builds the encoder half.)

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// The `"${XAAC_ROOT}/….c"` entries of one of libxaac's CMake lists.
fn sources(root: &Path, list: &str) -> Vec<PathBuf> {
    let text = fs::read_to_string(root.join(list)).unwrap_or_else(|e| panic!("{list}: {e}"));
    text.split('"')
        .filter_map(|s| s.strip_prefix("${XAAC_ROOT}/"))
        .filter(|s| s.ends_with(".c"))
        .map(|s| root.join(s))
        .collect()
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = manifest.join("../../third_party/libxaac");
    if !root.join("decoder/ixheaacd_api.c").is_file() {
        panic!(
            "libxaac submodule at {} is empty; run `git submodule update --init third_party/libxaac`",
            root.display()
        );
    }
    println!("cargo:rerun-if-changed=build.rs");
    for dir in ["common", "decoder"] {
        println!("cargo:rerun-if-changed={}", root.join(dir).display());
    }

    let mut files = Vec::new();
    for list in [
        "common/common.cmake",
        "decoder/libxaacdec.cmake",
        "decoder/drc_src/libxaacdec_drc.cmake",
        "decoder/x86/libxaacdec_x86.cmake",
    ] {
        files.extend(sources(&root, list));
    }
    assert!(
        files.len() > 100,
        "libxaac's decoder source lists look wrong"
    );

    let mut b = cc::Build::new();
    b.files(&files)
        .include(root.join("common"))
        .include(root.join("decoder"))
        .include(root.join("decoder/drc_src"))
        .define("LOUDNESS_LEVELING_SUPPORT", None)
        // Always optimised: the codec is far too slow unoptimised.
        .opt_level(3)
        .warnings(false);
    match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => {
            b.define("X86_64", None)
                .define("_X86_64_", None)
                .define("_X86_", None);
        }
        Ok("x86") => {
            b.define("X86", None).define("_X86_", None);
        }
        _ => {}
    }
    if b.get_compiler().is_like_msvc() {
        b.define("_CRT_SECURE_NO_WARNINGS", None);
    } else {
        // libxaac's fixed-point code relies on wrapping signed arithmetic.
        b.flag("-fwrapv");
    }
    b.compile("xaacdec");
}
