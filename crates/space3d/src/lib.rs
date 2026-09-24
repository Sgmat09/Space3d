//! # Space3d
//!
//! A cross-platform (desktop, web, Android, iOS) 3D game engine in Rust. This is the facade
//! crate: depend on `space3d`, `use space3d::prelude::*`, add [`DefaultPlugins`] and let
//! [`main!`] generate the entry point for every platform.
//!
//! ```no_run
//! use space3d::prelude::*;
//!
//! struct Spin;
//!
//! fn build(app: &mut App) {
//!     app.add_plugin(DefaultPlugins::new().title("Hello cube"));
//!     app.add_system(Stage::Startup, |ctx| {
//!         let cube = ctx.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
//!         ctx.world.spawn((Transform::default(), Mesh3d(cube), Material::srgb(0.9, 0.3, 0.2), Spin));
//!         ctx.world.spawn((Transform::from_xyz(2.0, 3.0, 4.0), DirectionalLight::default()));
//!         ctx.world.spawn((
//!             Transform::from_xyz(0.0, 1.5, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
//!             Camera3d::default(),
//!         ));
//!     });
//!     app.add_system(Stage::Update, |ctx| {
//!         let dt = ctx.time().delta_secs();
//!         for (t, _) in ctx.world.query_mut::<(&mut Transform, &Spin)>() {
//!             t.rotation *= Quat::from_rotation_y(dt);
//!         }
//!     });
//! }
//!
//! space3d::main!(build);
//! ```
//!
//! ## Crates and features
//!
//! | module | crate | feature (default?) |
//! |---|---|---|
//! | [`core`] | App, Plugin, ECS (hecs), resources, events, stages, fixed tick, Transform | always |
//! | [`tasks`] | non-blocking async/compute tasks | always |
//! | `render` | wgpu renderer, instancing, headless capture | `render` (yes) |
//! | `platform` | winit runner (desktop/web/Android/iOS), input, action maps | `platform` (yes) |
//! | `assets` | asset server, loaders, hot reload, texture/mesh editing | `assets` (yes) |
//! | `audio` | kira audio, spatial emitters | `audio` (yes) |
//! | `physics` | rapier physics | `physics` (yes) |
//! | `scene` | scenes, prefabs, snapshots | `scene` (yes) |
//! | `net` | HTTP/WebSocket clients + native servers | `net` (yes) |
//! | `db` | SQLite / localStorage stores | `db` (yes) |
//! | `ui` | egui integration, debug overlay | `ui` (yes) |
//! | `editor` | scene inspector, texture & model editors | `editor` (no) |
//! | `github` | releases, update checker, crash reporter | `github` (no) |
//! | `stories` | storybook | `stories` (no) |
//! | `test` | test harness, input simulation, golden screenshots | `test` (no) |
//!
//! ## Environment variables understood by the plugin groups
//!
//! * `SPACE3D_EXIT_AFTER_FRAMES=N` / `SPACE3D_EXIT_AFTER_SECS=S` — [`AutoExitPlugin`]
//! * `SPACE3D_HEADLESS=1` or `=WxH` — [`DefaultPlugins`] without a window (each axis clamped
//!   to `1..=`[`MAX_HEADLESS_SIZE`])
//! * `SPACE3D_AUDIO=off|mock` — audio backend override (`audio` crate)
//! * `RUST_LOG` — log filter ([`LogPlugin`], native)

#![warn(missing_docs)]

pub mod entry;
mod plugins;

pub use plugins::{
    AutoExitPlugin, DefaultPlugins, ENV_EXIT_AFTER_FRAMES, ENV_EXIT_AFTER_SECS, ENV_HEADLESS, LogPlugin,
    MAX_HEADLESS_SIZE, MinimalPlugins,
};

pub use space3d_core as core;
pub use space3d_core::{glam, hecs};
pub use space3d_tasks as tasks;
/// Logging macros (`space3d::tracing::info!(...)`); [`LogPlugin`] installs the subscriber.
pub use tracing;

#[cfg(feature = "assets")]
pub use space3d_assets as assets;
#[cfg(feature = "audio")]
pub use space3d_audio as audio;
#[cfg(feature = "db")]
pub use space3d_db as db;
#[cfg(feature = "editor")]
pub use space3d_editor as editor;
#[cfg(feature = "github")]
pub use space3d_github as github;
#[cfg(feature = "net")]
pub use space3d_net as net;
#[cfg(feature = "physics")]
pub use space3d_physics as physics;
#[cfg(feature = "platform")]
pub use space3d_platform as platform;
#[cfg(feature = "platform")]
pub use space3d_platform::winit;
#[cfg(feature = "render")]
pub use space3d_render as render;
#[cfg(feature = "render")]
pub use space3d_render::wgpu;
#[cfg(feature = "scene")]
pub use space3d_scene as scene;
#[cfg(feature = "stories")]
pub use space3d_stories as stories;
#[cfg(feature = "test")]
pub use space3d_test as test;
#[cfg(feature = "ui")]
pub use space3d_ui as ui;

/// Implementation details of [`main!`]. Not public API.
#[doc(hidden)]
pub mod __private {
    #[cfg(all(target_os = "android", feature = "platform"))]
    pub use space3d_platform::AndroidApp;
    #[cfg(target_arch = "wasm32")]
    pub use wasm_bindgen;
}

/// `use space3d::prelude::*;` — the types most games touch every day.
pub mod prelude {
    pub use crate::{AutoExitPlugin, DefaultPlugins, LogPlugin, MinimalPlugins};
    pub use space3d_core::glam::{EulerRot, IVec2, Mat3, Quat, UVec2, Vec2, Vec3, Vec4};
    pub use space3d_core::prelude::*;
    pub use space3d_tasks::{AsyncQueue, Task, TaskState, TasksPlugin, spawn, spawn_compute};

    #[cfg(all(feature = "render", not(target_arch = "wasm32")))]
    pub use space3d_render::init_headless;
    #[cfg(feature = "render")]
    pub use space3d_render::prelude::*;
    #[cfg(feature = "render")]
    pub use space3d_render::{
        Aabb, InstanceBufferHandle, InstancePacked, RenderCapabilities, RenderError, RenderPlugin,
    };

    #[cfg(feature = "platform")]
    pub use space3d_platform::prelude::*;
    #[cfg(feature = "platform")]
    pub use space3d_platform::{ActionValue, Touch, TouchPhase};

    #[cfg(feature = "assets")]
    pub use space3d_assets::prelude::*;
    #[cfg(feature = "assets")]
    pub use space3d_assets::{AssetError, edit};

    #[cfg(feature = "audio")]
    pub use space3d_audio::{
        Audio, AudioEmitter, AudioListener, AudioPlugin, Bus, Fade, PlayParams, SoundId, StaticSoundData,
    };

    #[cfg(feature = "physics")]
    pub use space3d_physics::{
        Collider, CollisionEvent, Friction, GravityScale, Physics, PhysicsPlugin, RayFilter, RayHit, Restitution,
        RigidBody, Sensor, Velocity,
    };

    #[cfg(feature = "scene")]
    pub use space3d_scene::{
        ComponentRegistry, Prefab, Scene, SceneCommands, SceneLoaded, ScenePlugin, WorldSnapshot, save_world,
    };

    #[cfg(feature = "net")]
    pub use space3d_net::prelude::*;

    #[cfg(feature = "db")]
    pub use space3d_db::{
        Db, DbError, DbPlugin, DbStatus, KvStore, Leaderboard, Migration, NewSave, SaveGame, SaveMeta, SaveSlots,
    };

    #[cfg(feature = "ui")]
    pub use space3d_ui::{DebugOverlay, UiAppExt, UiPlugin, egui};

    #[cfg(feature = "editor")]
    pub use space3d_editor::{EditorConfig, EditorPlugin, StorybookConfig, run_editor, run_storybook};

    #[cfg(feature = "stories")]
    pub use space3d_stories::{Knob, KnobKind, KnobValue, KnobValues, Story, Storybook};

    #[cfg(feature = "github")]
    pub use space3d_github::{GithubClient, GithubPlugin, RepoRef, UpdateChecker};
}
