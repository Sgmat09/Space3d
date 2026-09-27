//! Desktop, web and iOS entry point.
//!
//! `space3d::main!` turns the game's `build` function into `fn main` on desktop and iOS
//! (`space3d build --target ios` bundles this binary) and a `#[wasm_bindgen(start)]` function on
//! the web. Android, and iOS builds with `--xcode`, load the game from the `mobile/` crate
//! instead (same `build` function).

space3d::main!({{crate_name}}::build);
