//! Space3d renderer: a wgpu 30 forward renderer built around instancing and minimal
//! CPU→GPU traffic. Works windowed (via `space3d-platform`) and **headless** (tests,
//! storybook, CI screenshots). See `ARCHITECTURE.md` for the full data-flow description.
//!
//! * **ECS path** — entities with [`Mesh3d`] + [`Material`] + `GlobalTransform` are batched
//!   automatically: one instanced draw per (pipeline, material, mesh), sorted to minimize
//!   state changes.
//! * **Explicit path** — [`InstancedMesh`] + user-owned [`InstanceBuffer<T>`] of custom
//!   [`InstanceData`] for 100k–1M+ instances with dirty-range uploads.
//! * **GPU-driven culling** — compute-shader frustum culling writing indirect args, drawn with
//!   `multi_draw_indexed_indirect[_count]` where supported; CPU culling + direct draws on
//!   WebGL2/downlevel ([`DrawPath`]).
//! * **Headless capture** — [`Renderer::capture`] / [`render_to_image`].
//!
//! ```no_run
//! use space3d_core::prelude::*;
//! use space3d_render::*;
//! let mut app = App::new();
//! app.add_plugin(RenderPlugin::headless(256, 256));
//! let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
//! app.world.spawn((Transform::default(), Mesh3d(mesh), Material::srgb(0.8, 0.2, 0.2)));
//! app.world.spawn((Transform::from_xyz(0.0, 0.0, 3.0), Camera3d::default()));
//! let image = render_to_image(&mut app).expect("GPU available");
//! assert_eq!((image.width, image.height), (256, 256));
//! ```

mod arena;
mod assets;
mod components;
mod culling;
mod dirty;
mod frame;
mod gpu;
mod handles;
mod instance;
mod pipeline;
mod renderer;
mod stats;
mod texture;

pub use assets::{AddressMode, FilterMode, RenderAssets, SamplerSettings, TextureSource};
pub use components::{
    AmbientLight, Camera3d, DirectionalLight, Material, Mesh3d, PointLight, Projection, RenderTarget, Visibility,
    linear_to_srgb, srgb_to_linear,
};
pub use culling::{Aabb, Frustum, Sphere, max_axis_scale};
pub use dirty::DirtyRanges;
pub use frame::RenderSettings;
pub use handles::{InstanceBufferHandle, MeshHandle, TextureHandle};
pub use instance::{
    BufferUsage, CullSource, InstanceAttribute, InstanceBuffer, InstanceBuffers, InstanceCompact, InstanceData,
    InstanceLayout, InstancePacked, InstanceSemantic, InstanceTransform, InstanceTransformColor, InstancedMesh, ScaleKind,
};
pub use renderer::{DisplayHandle, RenderCapabilities, RenderError, Renderer, RendererConfig};
pub use stats::{DrawPath, RenderStats};

pub use glam;
pub use wgpu;

use space3d_core::{App, ImageData, Plugin, Stage, SystemCtx};

/// Registers render resources and the `Stage::Render` system.
///
/// The [`Renderer`] itself is inserted separately: by the platform runner once a window
/// exists, by [`RenderPlugin::headless`], or by [`init_headless`]. Without a renderer the
/// render system is a no-op, so simulation-only apps and tests keep working.
#[derive(Clone, Debug, Default)]
pub struct RenderPlugin {
    /// Create a headless renderer of this size during `build` (native only).
    pub headless: Option<(u32, u32)>,
}

impl RenderPlugin {
    /// Plugin that also creates a headless `width`x`height` renderer (skipped with a warning
    /// when no GPU adapter is available).
    pub fn headless(width: u32, height: u32) -> Self {
        Self {
            headless: Some((width, height)),
        }
    }
}

impl Plugin for RenderPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RenderAssets>()
            .init_resource::<InstanceBuffers>()
            .init_resource::<AmbientLight>()
            .init_resource::<RenderStats>()
            .init_resource::<RenderSettings>();
        app.add_system_named(Stage::Render, "render::render_frame", render_system);
        if let Some((w, h)) = self.headless {
            #[cfg(not(target_arch = "wasm32"))]
            if let Err(e) = init_headless(app, w, h) {
                tracing::warn!("headless renderer unavailable: {e}");
            }
            #[cfg(target_arch = "wasm32")]
            tracing::warn!("RenderPlugin::headless({w}, {h}) is not supported on the web; insert a Renderer asynchronously");
        }
    }
}

/// The `Stage::Render` system: extract the world and draw a frame if a [`Renderer`] exists.
pub fn render_system(ctx: &mut SystemCtx) {
    let Some(mut renderer) = ctx.resources.remove::<Renderer>() else {
        return;
    };
    renderer.render_frame(ctx.world, ctx.resources);
    ctx.resources.insert(renderer);
}

/// Create a headless renderer (blocking on adapter/device creation) and insert it.
#[cfg(not(target_arch = "wasm32"))]
pub fn init_headless(app: &mut App, width: u32, height: u32) -> Result<(), RenderError> {
    let renderer = futures::executor::block_on(Renderer::new_headless(width, height))?;
    app.insert_resource(renderer);
    Ok(())
}

/// Render the app's current state (without advancing time) and read back the primary target.
///
/// Transforms are propagated first, then one frame is rendered and captured synchronously.
/// Returns `None` when there is no [`Renderer`] (e.g. no GPU) or on the web, where blocking
/// is impossible — use [`Renderer::capture`] there. Deterministic for identical inputs on the
/// same machine.
pub fn render_to_image(app: &mut App) -> Option<ImageData> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        None
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        space3d_core::transform::propagate_transforms(&mut app.world);
        let mut renderer = app.resources.remove::<Renderer>()?;
        renderer.render_frame(&app.world, &mut app.resources);
        let image = renderer.capture_blocking();
        app.resources.insert(renderer);
        match image {
            Ok(img) => Some(img),
            Err(e) => {
                tracing::error!("render_to_image: {e}");
                None
            }
        }
    }
}

/// Common imports.
pub mod prelude {
    pub use crate::{
        AmbientLight, BufferUsage, Camera3d, DirectionalLight, DrawPath, InstanceBuffer, InstanceBuffers, InstanceCompact,
        InstanceData, InstanceTransform, InstanceTransformColor, InstancedMesh, Material, Mesh3d, MeshHandle, PointLight,
        Projection, RenderAssets, RenderPlugin, RenderSettings, RenderStats, RenderTarget, Renderer, TextureHandle,
        Visibility, render_to_image,
    };
}
