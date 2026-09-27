//! Android entry point of the example: `space3d build --target android` builds this library
//! as a `cdylib` (`cargo rustc --crate-type cdylib`). It compiles the example (`src/main.rs`)
//! as a module, whose `space3d::main!` exports `android_main`; on other targets it is empty.
#![cfg(target_os = "android")]

#[allow(dead_code)]
#[path = "main.rs"]
mod example;
