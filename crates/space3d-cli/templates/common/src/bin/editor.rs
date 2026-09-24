//! The Space3d editor with this game's scene loaded: hierarchy, inspector, gizmos,
//! play / pause / step, scene save/load, texture and model editors.
//!
//! ```text
//! space3d editor          # = cargo run --bin editor
//! ```

use space3d::prelude::*;

fn main() {
    run_editor(EditorConfig::new("{{title}} — editor").with_setup({{crate_name}}::editor_scene));
}
