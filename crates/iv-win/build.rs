//! Windows resources of the viewer's executable:
//! - the application manifest (`res/app.manifest`): version 6 of the common controls (themed
//!   buttons, trackbars and fields), Windows 10/11 as the supported OS and per-monitor DPI
//!   awareness v2;
//! - the icon (`res/app.ico`) and the version information (from Cargo.toml), written as a `.res`
//!   file that the linker takes directly, so no resource compiler is needed.
//!
//! It also has WinHTTP delay-loaded (see below).

// the .ico writer in it is for the icon's generator (a test), not for the build
#[allow(dead_code)]
#[path = "res/winres.rs"]
mod winres;

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=res/app.ico");
    println!("cargo:rerun-if-changed=res/winres.rs");
    println!("cargo:rerun-if-changed=res/app.manifest");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    // WinHTTP is only used when the user checks for updates: load it then, not at every start
    // (mapping it at start cost about 1 ms before the window existed)
    println!("cargo:rustc-link-arg-bins=/DELAYLOAD:winhttp.dll");
    println!("cargo:rustc-link-arg-bins=delayimp.lib");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", dir.join("res").join("app.manifest").display());

    let number = |key: &str| -> u16 {
        let v = std::env::var(key).unwrap_or_default();
        v.parse().unwrap_or_else(|_| panic!("{key} is not a number: {v:?}"))
    };
    let version =
        [number("CARGO_PKG_VERSION_MAJOR"), number("CARGO_PKG_VERSION_MINOR"), number("CARGO_PKG_VERSION_PATCH"), 0];
    let text = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let info = winres::version_info(
        version,
        &[
            ("FileDescription", "image-viewer"),
            ("ProductName", "image-viewer"),
            ("InternalName", "image-viewer"),
            ("OriginalFilename", "image-viewer.exe"),
            ("FileVersion", &text),
            ("ProductVersion", &text),
            ("Comments", "A small, fast image viewer for Windows"),
        ],
    );
    let ico = std::fs::read("res/app.ico").expect("res/app.ico");
    let res = winres::resource_file(&ico, &info).unwrap_or_else(|e| panic!("res/app.ico: {e}"));
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("image-viewer.res");
    std::fs::write(&out, res).unwrap_or_else(|e| panic!("{}: {e}", out.display()));
    println!("cargo:rustc-link-arg-bins={}", out.display());
}
