#![warn(missing_docs)]
//! Space3d editor tools, built on `space3d-ui` (egui).
//!
//! * [`EditorPlugin`] — add to any app for an in-game editor: scene hierarchy tree
//!   (`Name` / `Parent`, drag-and-drop reparenting), inspector (typed editors for `Transform`,
//!   `Name`, `Material`, lights, `Camera3d`, `Visibility`, [`MeshSource`], plus a JSON editor
//!   for any component in the scene `ComponentRegistry`), translate gizmo and click-picking
//!   in the viewport, play / pause / step-N-ticks ([`SimControl`]), scene save/load (RON or
//!   JSON), and undo/redo ([`EditorHistory`]) for every editor operation.
//! * [`TextureEditor`] — brush / eraser / fill / line / rect / picker tools, color picker,
//!   brightness-contrast, hue-saturation, blur, resize, flip, rotate, noise and normal-map
//!   generation, zoom/pan canvas, undo/redo via `ImageEditHistory`, PNG save.
//! * [`ModelEditor`] — 3D preview rendered by an orbit camera into an offscreen render target
//!   shown as an egui texture, vertex/triangle stats and wireframe overlay, `edit::mesh`
//!   operations (transform, normals, weld, subdivide, extrude, UV projection, decimate,
//!   vertex-color paint), undo/redo and OBJ/glTF export.
//! * [`run_editor`] — standalone editor app; [`run_storybook`] — storybook viewer (sidebar
//!   tree + search, live story viewport, knobs, tick controls, screenshots), native and web.
//!
//! Pausing works by controlling time: the runner keeps calling `App::update(dt)`, and while
//! paused the fixed-step accumulator is emptied so `FixedUpdate` does not run; stepping sets
//! it to exactly `n` steps (see [`SimControl`]).
//!
//! ```no_run
//! use space3d_core::prelude::*;
//! use space3d_editor::{EditorConfig, run_editor};
//!
//! run_editor(EditorConfig::new("My level").with_setup(|app: &mut App| {
//!     app.world.spawn((Name("Sun".into()), Transform::IDENTITY));
//! }));
//! ```

pub mod components;
mod editor;
mod files;
mod hierarchy;
pub mod history;
mod inspector;
mod model_editor;
pub mod scene_ops;
pub mod sim;
mod storybook;
mod sub_app;
mod texture_editor;
pub mod viewport;

pub use components::{EditorInternal, MeshSource, register_editor_components};
pub use editor::{EditorPlugin, EditorState, camera_transform};
pub use files::{read_file, read_file_with, sanitize_file_name, write_file, write_file_with};
pub use hierarchy::{HierarchyAction, can_reparent, hierarchy_ui};
pub use history::{EditCommand, EditorHistory, EntitySnapshot};
pub use inspector::{InspectorState, inspector_ui};
pub use model_editor::{
    Axis, MeshOpParams, ModelEditor, PREVIEW_ORIGIN, faces_facing, raycast_mesh, raycast_mesh_face,
};
pub use scene_ops::AddKind;
pub use sim::{SimControl, SimControlPlugin};
pub use storybook::{StorybookConfig, StorybookPlugin, StorybookViewer, build_story_app};
pub use sub_app::{LiveView, SubApp, panics_are_isolated};
pub use texture_editor::{Adjustments, TextureEditor, TextureTool, display_image, erase_dab};

use space3d_core::{App, AppExit, Stage};
use space3d_platform::{InputPlugin, PlatformPlugin, WindowPlugin};
use space3d_render::RenderPlugin;
use space3d_stories::Storybook;
use space3d_ui::UiPlugin;
use std::sync::Arc;

/// Environment variable: run without a window (`1`/`true`, or `WIDTHxHEIGHT`).
pub const ENV_HEADLESS: &str = "SPACE3D_HEADLESS";
/// Environment variable: quit after this many frames.
pub const ENV_EXIT_AFTER_FRAMES: &str = "SPACE3D_EXIT_AFTER_FRAMES";

/// Read [`ENV_HEADLESS`] / [`ENV_EXIT_AFTER_FRAMES`] (never set on the web).
pub(crate) fn env_overrides(default_size: (u32, u32)) -> (Option<(u32, u32)>, Option<u64>) {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = default_size;
        (None, None)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let var = |n: &str| {
            std::env::var(n)
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let headless = var(ENV_HEADLESS).and_then(|v| match v.to_ascii_lowercase().as_str() {
            "0" | "false" | "no" => None,
            "1" | "true" | "yes" => Some(default_size),
            other => other
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                .or(Some(default_size)),
        });
        let frames = var(ENV_EXIT_AFTER_FRAMES).and_then(|v| v.parse().ok());
        (headless, frames)
    }
}

type Setup = Arc<dyn Fn(&mut App) + Send + Sync>;

/// Settings of the standalone editor ([`run_editor`]).
#[derive(Clone)]
pub struct EditorConfig {
    /// Window title.
    pub title: String,
    /// Window width (logical px).
    pub width: u32,
    /// Window height (logical px).
    pub height: u32,
    /// Start with the simulation paused.
    pub start_paused: bool,
    /// Scene file for save/load.
    pub scene_path: String,
    /// Run without a window at this size (also `SPACE3D_HEADLESS`).
    pub headless: Option<(u32, u32)>,
    /// Quit after this many frames (also `SPACE3D_EXIT_AFTER_FRAMES`).
    pub exit_after_frames: Option<u64>,
    /// Open the texture editor at startup.
    pub show_texture_editor: bool,
    /// Open the model editor at startup.
    pub show_model_editor: bool,
    /// Game content: called with the app after the editor plugins were added.
    pub setup: Option<Setup>,
}

impl std::fmt::Debug for EditorConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorConfig")
            .field("title", &self.title)
            .field("size", &(self.width, self.height))
            .field("headless", &self.headless)
            .finish()
    }
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            title: "Space3d Editor".into(),
            width: 1600,
            height: 900,
            start_paused: true,
            scene_path: "scene.ron".into(),
            headless: None,
            exit_after_frames: None,
            show_texture_editor: false,
            show_model_editor: false,
            setup: None,
        }
    }
}

impl EditorConfig {
    /// Config with a window title.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Default::default()
        }
    }

    /// Set the game-content setup function.
    pub fn with_setup(mut self, setup: impl Fn(&mut App) + Send + Sync + 'static) -> Self {
        self.setup = Some(Arc::new(setup));
        self
    }

    /// Apply `SPACE3D_HEADLESS` and `SPACE3D_EXIT_AFTER_FRAMES`.
    pub fn with_env(mut self) -> Self {
        let (headless, frames) = env_overrides((self.width, self.height));
        self.headless = self.headless.or(headless);
        self.exit_after_frames = self.exit_after_frames.or(frames);
        self
    }

    /// The [`EditorPlugin`] for these settings.
    pub fn plugin(&self) -> EditorPlugin {
        EditorPlugin {
            start_paused: self.start_paused,
            scene_path: self.scene_path.clone(),
            show_texture_editor: self.show_texture_editor,
            show_model_editor: self.show_model_editor,
        }
    }
}

fn host_app(title: &str, size: (u32, u32), headless: Option<(u32, u32)>, exit_after: Option<u64>) -> App {
    let mut app = App::new();
    match headless {
        Some((w, h)) => {
            app.add_plugin(InputPlugin);
            app.add_plugin(RenderPlugin::headless(w, h));
        }
        None => {
            app.add_plugin(PlatformPlugin {
                window: WindowPlugin {
                    title: title.to_owned(),
                    width: size.0,
                    height: size.1,
                    ..Default::default()
                },
            });
        }
    }
    app.add_plugin(UiPlugin::default());
    if let Some(n) = exit_after {
        app.add_system_named(Stage::Last, "editor::auto_exit", move |sys| {
            if sys.time().frame >= n {
                sys.send(AppExit);
            }
        });
    }
    app
}

/// Build the standalone editor app without running it (tests, custom runners).
pub fn editor_app(config: &EditorConfig) -> App {
    let mut app = host_app(
        &config.title,
        (config.width, config.height),
        config.headless,
        config.exit_after_frames,
    );
    app.add_plugin(config.plugin());
    if let Some(setup) = &config.setup {
        setup(&mut app);
    }
    app
}

/// Run the standalone editor (window, UI, editor; `config.setup` adds the game content).
/// `SPACE3D_HEADLESS` / `SPACE3D_EXIT_AFTER_FRAMES` are honored.
pub fn run_editor(config: EditorConfig) {
    editor_app(&config.with_env()).run();
}

/// Build the storybook viewer app without running it.
pub fn storybook_app(book: Storybook, config: StorybookConfig) -> App {
    let mut app = host_app(
        &config.title,
        (config.width, config.height),
        config.headless,
        config.exit_after_frames,
    );
    app.add_plugin(StorybookPlugin { book, config });
    app
}

/// Run the storybook viewer for `book` with default settings (native window or web canvas).
///
/// Each selected story runs in an app of its own. A panicking story is shown as an error
/// only in builds where panics unwind (dev builds); with `panic = "abort"` (the workspace's
/// release profile, and every `wasm32` build) it takes the viewer down — see
/// [`panics_are_isolated`]. On the web, story viewports need WebGPU or WebGL2.
pub fn run_storybook(book: Storybook) {
    run_storybook_with(book, StorybookConfig::default());
}

/// [`run_storybook`] with explicit settings (`SPACE3D_HEADLESS` / `SPACE3D_EXIT_AFTER_FRAMES`
/// are honored).
pub fn run_storybook_with(book: Storybook, config: StorybookConfig) {
    storybook_app(book, config.with_env()).run();
}

/// Common imports.
pub mod prelude {
    pub use crate::{
        EditorConfig, EditorHistory, EditorPlugin, EditorState, MeshSource, ModelEditor, SimControl, StorybookViewer,
        TextureEditor, run_editor, run_storybook,
    };
}
