// made by CasualArclamp
//! Builds the *decoder* half of the vendored libxaac (`third_party/libxaac`, tag v0.1.13,
//! Apache-2.0) as a static C library.
//!
//! The source list is read from libxaac's own CMake files — `common/common.cmake`,
//! `decoder/libxaacdec.cmake`, `decoder/drc_src/libxaacdec_drc.cmake` and the
//! arch-specific list (`decoder/x86/libxaacdec_x86.cmake` on x86,
//! `decoder/armv8/libxaacdec_armv8.cmake` on aarch64, `decoder/armv7/libxaacdec_armv7.cmake`
//! on arm). Both `.c` and `.s` (assembly) sources are picked up — the ARM assembly kernels
//! provide symbols the generic C fallbacks don't, so skipping them leaves undefined
//! symbols on Apple Silicon. The definitions follow libxaac's `cmake/utils.cmake`:
//! `LOUDNESS_LEVELING_SUPPORT` everywhere, the x86 ones on x86, ARM ones on ARM, and
//! `-fwrapv` where the compiler takes it. The encoder, test benches and fuzzers are not
//! compiled. (The approach is DecDRM's `decdrm-xaac-sys`, which builds the encoder half.)

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// The `"${XAAC_ROOT}/….c"` or `"${XAAC_ROOT}/….s"` entries of one of libxaac's CMake lists.
fn sources(root: &Path, list: &str) -> Vec<PathBuf> {
    let text = fs::read_to_string(root.join(list)).unwrap_or_else(|e| panic!("{list}: {e}"));
    text.split('"')
        .filter_map(|s| s.strip_prefix("${XAAC_ROOT}/"))
        .filter(|s| s.ends_with(".c") || s.ends_with(".s"))
        .map(|s| root.join(s))
        .collect()
}

/// The encoder half (`encoder/`, `encoder/drc_src/`) with the shim, after DecDRM's
/// `decdrm-xaac-sys`, unpatched. Its three `common` files come from the decoder's library.
fn build_encoder(root: &Path, manifest: &Path) {
    println!("cargo:rerun-if-changed=csrc");
    let mut files = Vec::new();
    for dir in ["encoder", "encoder/drc_src"] {
        println!("cargo:rerun-if-changed={}", root.join(dir).display());
        let mut c: Vec<PathBuf> = fs::read_dir(root.join(dir))
            .unwrap_or_else(|e| panic!("{dir}: {e}"))
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "c"))
            .collect();
        c.sort();
        files.extend(c);
    }
    assert!(files.len() > 100, "libxaac's encoder sources look wrong");
    let mut b = cc::Build::new();
    b.files(&files)
        .file(manifest.join("csrc/decsat_xaac_shim.c"))
        .include(root.join("encoder"))
        .include(root.join("encoder/drc_src"))
        .include(root.join("common"))
        .define("LOUDNESS_LEVELING_SUPPORT", None)
        .opt_level(3)
        .warnings(false);
    if !b.get_compiler().is_like_msvc() {
        b.flag("-fwrapv");
    }
    b.compile("xaacenc");
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
    for dir in [
        "common",
        "decoder",
        "decoder/armv8",
        "decoder/armv7",
        "decoder/x86",
    ] {
        println!("cargo:rerun-if-changed={}", root.join(dir).display());
    }

    let arch_list = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "decoder/armv8/libxaacdec_armv8.cmake",
        Ok("arm") => "decoder/armv7/libxaacdec_armv7.cmake",
        _ => "decoder/x86/libxaacdec_x86.cmake",
    };

    // Before the decoder, whose `common` files it also needs: a Unix linker reads static
    // libraries in order.
    if env::var_os("CARGO_FEATURE_ENCODER").is_some() {
        build_encoder(&root, &manifest);
    }

    let mut files = Vec::new();
    for list in [
        "common/common.cmake",
        "decoder/libxaacdec.cmake",
        "decoder/drc_src/libxaacdec_drc.cmake",
        arch_list,
    ] {
        files.extend(sources(&root, list));
    }
    // libxaac's ARMv8 list names `ixheaacd_postradixcompute4.s` twice, and `cc` does not
    // remove duplicates: both would compile to the same object path. `dedup` only drops
    // adjacent repeats, hence the sort first.
    files.sort();
    files.dedup();
    assert!(
        files.len() > 100,
        "libxaac's decoder source lists look wrong"
    );
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let n_c = files
        .iter()
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("c"))
        .count();
    let n_s = files
        .iter()
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("s"))
        .count();
    eprintln!("[decsat-xaac-sys] compiling {n_c} C files + {n_s} asm files for arch {target_arch}");

    let mut b = cc::Build::new();
    b.include(root.join("common"))
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
        Ok("aarch64") => {
            b.define("AARCH64", None).define("_AARCH64_", None);
            b.include(root.join("decoder/armv8"));
        }
        Ok("arm") => {
            b.define("ARM", None).define("_ARM_", None);
            b.include(root.join("decoder/armv7"));
        }
        _ => {}
    }
    if b.get_compiler().is_like_msvc() {
        b.define("_CRT_SECURE_NO_WARNINGS", None);
    } else {
        // libxaac's fixed-point code relies on wrapping signed arithmetic.
        b.flag("-fwrapv");
    }
    // Apple's assembler deliberately does not apply the leading underscore
    // that Mach-O uses for C symbols to symbols declared by hand-written
    // assembly.  The libxaac ARM sources use ELF spelling, so provide Mach-O
    // aliases for their exported entry points.  Keeping the original labels
    // means the same vendored sources continue to work on ELF targets.
    if target_os == "macos" && target_arch == "aarch64" {
        let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
        let aliases = out_dir.join("xaac_armv8_darwin_aliases.s");
        let mut text = String::from(".text\n");
        for source in &files {
            if source.extension().and_then(|e| e.to_str()) != Some("s") {
                continue;
            }
            let source_text =
                fs::read_to_string(source).unwrap_or_else(|e| panic!("{}: {e}", source.display()));
            for line in source_text.lines() {
                let Some(symbol) = line
                    .trim_start()
                    .strip_prefix(".global ")
                    .or_else(|| line.trim_start().strip_prefix(".globl "))
                    .map(str::trim)
                else {
                    continue;
                };
                if !symbol.is_empty()
                    && symbol
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    text.push_str(&format!(".globl _{symbol}\n_{symbol} = {symbol}\n"));
                }
            }
        }
        fs::write(&aliases, text).unwrap_or_else(|e| panic!("{}: {e}", aliases.display()));
        files.push(aliases);
    }

    // The one place the sources are added: `files` is only complete here, once the macOS
    // aliases are in it, and `cc` compiles a file once per time it is added.
    b.files(&files).compile("xaacdec");
}
