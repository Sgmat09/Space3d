//! Links open-apple-models' C ABI when the `apple-ffi` feature is enabled on an Apple target.
//!
//! * `OAM_APPLE_LIB_DIR` — folder containing `libOpenAppleModelsFFI.dylib` (macOS) or
//!   `libOpenAppleModelsFFI.a` (iOS, when linking from Cargo instead of Xcode).
//! * `OAM_APPLE_LIB_KIND` — `dylib` (default on macOS) or `static`.
//!
//! On macOS an rpath to the folder is added for this package's own tests and examples; without
//! the variable nothing is linked (a build warning explains how to fix it) and `AppleBackend`
//! reports `Unsupported`, so `--all-features` builds still work. On iOS the library is normally
//! linked by Xcode from the XCFramework, so the variable is optional there. Without the
//! feature, or on other targets, nothing is linked and `AppleBackend` reports `Unsupported`.

use std::env;

fn main() {
    println!("cargo::rerun-if-env-changed=OAM_APPLE_LIB_DIR");
    println!("cargo::rerun-if-env-changed=OAM_APPLE_LIB_KIND");
    println!("cargo::rustc-check-cfg=cfg(oam_apple_linked)");

    let feature = env::var_os("CARGO_FEATURE_APPLE_FFI").is_some();
    let vendor = env::var("CARGO_CFG_TARGET_VENDOR").unwrap_or_default();
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if !feature || vendor != "apple" {
        return;
    }
    let dir = env::var("OAM_APPLE_LIB_DIR").ok().filter(|d| !d.trim().is_empty());
    let default_kind = if os == "macos" { "dylib" } else { "static" };
    let kind = env::var("OAM_APPLE_LIB_KIND").unwrap_or_else(|_| default_kind.to_owned());
    match dir {
        Some(dir) => {
            println!("cargo::rustc-cfg=oam_apple_linked");
            println!("cargo::rustc-link-search=native={dir}");
            println!("cargo::rustc-link-lib={kind}=OpenAppleModelsFFI");
            if kind == "dylib" {
                // `@rpath/libOpenAppleModelsFFI.dylib` resolves for this package's tests and
                // examples; other binaries add their own rpath (see docs/ai.md).
                println!("cargo::rustc-link-arg=-Wl,-rpath,{dir}");
            }
        }
        None if os == "macos" => println!(
            "cargo::warning=space3d-ai: `apple-ffi` is enabled but OAM_APPLE_LIB_DIR is not set, so Apple's bridge \
             is not linked and AppleBackend reports Unsupported. Build it with `swift build -c release --product \
             OpenAppleModelsFFI` in open-apple-models and set OAM_APPLE_LIB_DIR to its .build/release folder."
        ),
        // iOS / visionOS: Xcode links OpenAppleModelsFFI (static XCFramework) into the app.
        None => println!("cargo::rustc-cfg=oam_apple_linked"),
    }
}
