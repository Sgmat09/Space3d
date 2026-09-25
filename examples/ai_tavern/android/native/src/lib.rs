//! `android_main` for the AI tavern: NativeActivity loads `libai_tavern_android.so`, and
//! `space3d::main!` generates the entry that runs `ai_tavern::build` on android-activity.
//!
//! `space3d-ai`'s `AndroidBackend` (picked by `BackendChoice::Auto`) finds the Android context
//! through `ndk-context`, loads `com.spacecorps.oam.jni.OamJni` from the app's class loader and
//! registers `nativeDeliver`, so Mira runs on Gemini Nano on AICore devices.

space3d::main!(ai_tavern::build);
