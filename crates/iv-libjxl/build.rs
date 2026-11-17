//! Builds the reference libjxl 0.12 from the source vendored in the `jpegxl-src` package and links
//! it statically.
//!
//! `gamut-jxl` declares the libjxl encoder API through `gamut-jxl-sys`, whose own build calls
//! `jpegxl_src::build()`. That asks CMake for the Visual Studio *ClangCL* toolset, an optional
//! component that a plain Build Tools install does not have: the configure step fails with
//! MSB8020. The workspace therefore sets `GAMUT_JXL_SYS_SKIP_NATIVE=1` (`.cargo/config.toml`), so
//! `gamut-jxl-sys` builds nothing, and this script compiles the same source with the toolset that
//! is always there (MSVC `cl` on Windows, the system compiler elsewhere).
//!
//! Only the encoder library and what it needs are linked: `jxl`, `jxl_cms` (colour management
//! through skcms), Highway and Brotli. Decoding stays with the pure-Rust `jxl-oxide`.

use std::path::{Path, PathBuf};

/// The `jpegxl-src` release whose `libjxl/` tree is built: the `=0.12.0` that `gamut-jxl-sys`
/// pins as its build dependency (so Cargo always fetches it) and writes its FFI declarations for.
const JPEGXL_SRC: &str = "jpegxl-src-0.12.0";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=IV_LIBJXL_SOURCE");
    let source = source_dir();
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");

    let mut config = cmake::Config::new(&source);
    for (key, value) in [
        ("BUILD_SHARED_LIBS", "OFF"),
        ("BUILD_TESTING", "OFF"),
        ("JPEGXL_ENABLE_TOOLS", "OFF"),
        ("JPEGXL_ENABLE_DEVTOOLS", "OFF"),
        ("JPEGXL_ENABLE_DOXYGEN", "OFF"),
        ("JPEGXL_ENABLE_MANPAGES", "OFF"),
        ("JPEGXL_ENABLE_BENCHMARK", "OFF"),
        ("JPEGXL_ENABLE_EXAMPLES", "OFF"),
        ("JPEGXL_ENABLE_FUZZERS", "OFF"),
        ("JPEGXL_ENABLE_VIEWERS", "OFF"),
        ("JPEGXL_ENABLE_PLUGINS", "OFF"),
        ("JPEGXL_ENABLE_JNI", "OFF"),
        ("JPEGXL_ENABLE_SJPEG", "OFF"),
        ("JPEGXL_ENABLE_OPENEXR", "OFF"),
        ("JPEGXL_ENABLE_TCMALLOC", "OFF"),
        ("JPEGXL_BUNDLE_LIBPNG", "OFF"),
        // needed: lossless JPEG recompression and the Exif/XMP boxes
        ("JPEGXL_ENABLE_TRANSCODE_JPEG", "ON"),
        ("JPEGXL_ENABLE_BOXES", "ON"),
        ("JPEGXL_ENABLE_SKCMS", "ON"),
    ] {
        config.define(key, value);
    }
    // the library is always optimised, also in debug builds of the workspace (like every other
    // dependency, see [profile.dev.package."*"])
    config.profile("Release");
    if msvc {
        // The C runtime linked statically (/MT), as in every other part of the executables (see
        // .cargo/config.toml): no VCRUNTIME140.dll. Left alone, the `cmake` crate replaces
        // CMake's MSVC flags with its own, which lack the optimisation level and C++ unwinding
        // (/EHsc), so CMake's defaults are given here in full; the runtime comes from
        // CMAKE_MSVC_RUNTIME_LIBRARY.
        //
        // libjxl's error messages carry __FILE__, which would put the build machine's paths (the
        // user name in the Cargo home) into the executables; /d1trimfile cuts the source folder
        // off, leaving `lib/jxl/…` (scripts/package.ps1 checks that no build path is left).
        let folder = source.display().to_string();
        // a path with spaces is quoted; a backslash before the closing quote must be doubled
        let trim = if folder.contains(' ') {
            format!("\"/d1trimfile:{folder}\\\\\"")
        } else {
            format!("/d1trimfile:{folder}\\")
        };
        let common = format!("/DWIN32 /D_WINDOWS /nologo /Brepro {trim}");
        config
            .define("CMAKE_MSVC_RUNTIME_LIBRARY", "MultiThreaded")
            .define("CMAKE_C_FLAGS", &common)
            .define("CMAKE_CXX_FLAGS", format!("{common} /GR /EHsc"))
            .define("CMAKE_C_FLAGS_RELEASE", "/O2 /Ob2 /DNDEBUG")
            .define("CMAKE_CXX_FLAGS_RELEASE", "/O2 /Ob2 /DNDEBUG")
            // compile the files of one project in parallel too, within Cargo's job count
            .define("CMAKE_VS_GLOBALS", "UseMultiToolTask=true;EnforceProcessCountAcrossBuilds=true");
    }
    let prefix = config.build();

    let lib = ["lib", "lib64"]
        .iter()
        .map(|d| prefix.join(d))
        .find(|d| d.is_dir())
        .unwrap_or_else(|| panic!("libjxl installed no lib directory under {}", prefix.display()));
    println!("cargo:rustc-link-search=native={}", lib.display());
    for name in ["jxl", "jxl_cms", "hwy", "brotlienc", "brotlidec", "brotlicommon"] {
        println!("cargo:rustc-link-lib=static={name}");
    }
    // MSVC objects name their C++ runtime themselves (libcpmt for /MT)
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos" | "ios" | "freebsd") => println!("cargo:rustc-link-lib=c++"),
        Ok("windows") if msvc => {}
        _ => println!("cargo:rustc-link-lib=stdc++"),
    }
}

/// The `libjxl/` tree inside the `jpegxl-src` package: `IV_LIBJXL_SOURCE` when set (a vendored or
/// mirrored copy), otherwise the package as Cargo unpacked it under `$CARGO_HOME/registry/src`.
fn source_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("IV_LIBJXL_SOURCE") {
        let dir = PathBuf::from(dir);
        assert!(is_libjxl(&dir), "IV_LIBJXL_SOURCE is not a libjxl source tree: {}", dir.display());
        return dir;
    }
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".cargo")))
        .expect("neither CARGO_HOME nor a home directory is set");
    let registry = cargo_home.join("registry").join("src");
    std::fs::read_dir(&registry)
        .into_iter()
        .flatten()
        .flatten()
        .map(|index| index.path().join(JPEGXL_SRC).join("libjxl"))
        .find(|dir| is_libjxl(dir))
        .unwrap_or_else(|| {
            panic!(
                "the libjxl source of {JPEGXL_SRC} is not under {}; set IV_LIBJXL_SOURCE to its \
                 libjxl directory",
                registry.display()
            )
        })
}

fn is_libjxl(dir: &Path) -> bool {
    dir.join("CMakeLists.txt").is_file() && dir.join("lib").join("include").join("jxl").join("encode.h").is_file()
}
