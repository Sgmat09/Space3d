//! With the `apple` feature on macOS, lets the binary find `libOpenAppleModelsFFI.dylib`
//! (`@rpath`) in `OAM_APPLE_LIB_DIR`, the folder space3d-ai links it from.

fn main() {
    println!("cargo::rerun-if-env-changed=OAM_APPLE_LIB_DIR");
    let apple = std::env::var_os("CARGO_FEATURE_APPLE").is_some();
    let macos = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "macos");
    if apple
        && macos
        && let Ok(dir) = std::env::var("OAM_APPLE_LIB_DIR")
    {
        println!("cargo::rustc-link-arg=-Wl,-rpath,{dir}");
    }
}
