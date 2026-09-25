//! Compatibility fixture (graphics spec §2.7): code written against Space3d 249c874 — the
//! revision SpaceCorps2027 pins at 4159bb8 — must keep compiling, unchanged, while the renderer
//! grows. Every item below is written the way the game writes it (same imports, literals,
//! functional-record-update forms and trait impls); a change to the public API that breaks any
//! of them fails this file's build.
//!
//! The tests also check that the legacy defaults still mean what they meant.
#![cfg(all(feature = "render", feature = "assets"))]
#![allow(dead_code)]

// The game's imports, verbatim (`client/src/game/{additive,env,models}.rs`).
use space3d::assets::{AlphaMode, MaterialData, ModelData, Primitive};
use space3d::prelude::*;
use space3d::render::{AddressMode, RendererConfig, SamplerSettings, TextureSource};
use space3d::render::{OverlayTarget, OverlayTargetKind, RenderOverlay, RenderOverlays};
use space3d::wgpu;
use std::sync::{Arc, Mutex};

// ---- RenderOverlay: `prepare` + `render` only (client/src/game/additive.rs) -----------------

/// Per-target GPU state, keyed by the target kind like the game's overlay.
struct KindState {
    kind: OverlayTargetKind,
    size: (u32, u32),
}

#[derive(Default)]
struct ParticleOverlay {
    count: u32,
    encode_srgb: bool,
    pipelines: Vec<wgpu::TextureFormat>,
    views: Vec<KindState>,
    scissor: Option<[u32; 4]>,
}

impl RenderOverlay for ParticleOverlay {
    fn prepare(
        &mut self,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _encoder: &mut wgpu::CommandEncoder,
        target: &OverlayTarget,
    ) {
        if self.count == 0 {
            return;
        }
        if !self.pipelines.contains(&target.format) {
            self.pipelines.push(target.format);
        }
        // The 249c874 heuristic: shader-side sRGB encoding for non-sRGB formats.
        self.encode_srgb = !target.format.is_srgb();
        match self.views.iter_mut().find(|v| v.kind == target.kind) {
            Some(v) => v.size = target.size,
            None => self.views.push(KindState {
                kind: target.kind,
                size: target.size,
            }),
        }
    }

    fn render(&self, pass: &mut wgpu::RenderPass<'static>, target: &OverlayTarget) {
        if self.count == 0 {
            return;
        }
        if let Some([x, y, w, h]) = self.scissor {
            let (tw, th) = target.size;
            let (x, y) = (x.min(tw), y.min(th));
            let (w, h) = (w.min(tw - x), h.min(th - y));
            if w == 0 || h == 0 {
                return;
            }
            pass.set_scissor_rect(x, y, w, h);
        }
        pass.draw(0..6, 0..self.count);
    }
}

/// Handle to the registered overlay (resource), like `AdditiveParticles`.
struct AdditiveParticles(Arc<Mutex<ParticleOverlay>>);

impl AdditiveParticles {
    fn register(resources: &mut Resources) {
        let Some(overlays) = resources.get_mut::<RenderOverlays>() else {
            return;
        };
        let handle = overlays.insert("additive-particles", 50, ParticleOverlay::default());
        resources.insert(AdditiveParticles(handle));
    }
}

/// The game keeps per-kind buffers and matches kinds exhaustively.
fn kind_slot(kind: OverlayTargetKind) -> usize {
    match kind {
        OverlayTargetKind::Surface => 0,
        OverlayTargetKind::Offscreen => 1,
        OverlayTargetKind::Capture => 2,
    }
}

// ---- Materials: FRU forms (ui/landing.rs, ui/pages/wiki/viewer.rs) --------------------------

fn see_through(color: Vec4) -> Material {
    Material {
        base_color: color,
        unlit: true,
        double_sided: true,
        alpha_blend: true,
        ..Material::default()
    }
}

fn holo_solid(c: Vec4, metallic: f32, roughness: f32) -> Material {
    Material {
        alpha_blend: c.w < 1.0,
        double_sided: true,
        emissive: c.truncate() * 0.18,
        ..Material::color(c).with_metallic_roughness(metallic, roughness)
    }
}

fn holo_basic(c: Vec4) -> Material {
    Material {
        unlit: true,
        alpha_blend: true,
        double_sided: true,
        ..Material::color(c)
    }
}

// ---- Lights, meshes, instancing, settings ---------------------------------------------------

const STUDIO_AMBIENT: AmbientLight = AmbientLight {
    color: Vec3::new(0.6, 0.66, 0.8),
    intensity: 0.22,
};

fn lights(world: &mut World) {
    world.spawn((
        Transform::IDENTITY.looking_at(Vec3::new(-0.4, -1.0, -0.5), Vec3::Y),
        DirectionalLight {
            color: Vec3::new(1.0, 0.94, 0.86),
            intensity: 2.1,
        },
    ));
    world.spawn((
        Transform::from_xyz(0.0, 10.0, 0.0),
        PointLight {
            color: Vec3::new(0.4, 0.7, 1.0),
            intensity: 3.0,
            range: 520.0,
        },
    ));
}

fn quad(corners: [Vec3; 4], uvs: [[f32; 2]; 4]) -> MeshData {
    let n = (corners[1] - corners[0])
        .cross(corners[3] - corners[0])
        .normalize_or_zero();
    MeshData {
        positions: corners.iter().map(|c| c.to_array()).collect(),
        normals: vec![n.to_array(); 4],
        uvs: uvs.to_vec(),
        colors: Vec::new(),
        indices: vec![0, 1, 2, 0, 2, 3],
    }
}

fn grid(world: &mut World, assets: &mut RenderAssets, buffers: &mut InstanceBuffers) -> Entity {
    let mesh = assets.add_mesh(quad(
        [
            Vec3::new(-1.0, 0.0, -1.0),
            Vec3::new(1.0, 0.0, -1.0),
            Vec3::new(1.0, 0.0, 1.0),
            Vec3::new(-1.0, 0.0, 1.0),
        ],
        [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
    ));
    let buffer = buffers.insert(InstanceBuffer::from_vec(
        vec![InstanceTransform::from_trs(Vec3::ZERO, Quat::IDENTITY, Vec3::ONE)],
        BufferUsage::Static,
    ));
    world.spawn((InstancedMesh {
        mesh,
        material: holo_basic(Vec4::new(0.3, 0.6, 1.0, 0.5)),
        instances: buffer,
    },))
}

fn textures(assets: &mut RenderAssets, img: ImageData, address: AddressMode) -> TextureHandle {
    assets.add_texture(TextureSource {
        sampler: SamplerSettings {
            address,
            ..SamplerSettings::default()
        },
        ..TextureSource::from_image(img)
    })
}

fn hull_material() -> (MaterialData, MaterialData) {
    let data = MaterialData {
        name: Some("SC_Hull".into()),
        emissive: [1.0, 1.0, 1.0],
        double_sided: true,
        ..Default::default()
    };
    let engine = MaterialData {
        emissive: [0.2, 0.6, 1.0],
        ..data.clone()
    };
    (data, engine)
}

/// The game's primitive split (`client/src/game/models.rs` at 2f160d1, which pins a614a02):
/// exhaustive `Primitive { mesh, material }` literals, no functional record update.
fn split_primitive(model: &mut ModelData, moved: MeshData, material: usize) {
    let mut added = Vec::new();
    if !moved.indices.is_empty() {
        added.push(Primitive {
            mesh: moved,
            material: Some(material),
        });
    }
    let mesh = &mut model.meshes[0];
    mesh.primitives.retain(|p| !p.mesh.indices.is_empty());
    mesh.primitives.extend(added);
}

fn plugins_and_settings() -> (RenderPlugin, RenderSettings, RendererConfig, RenderStats) {
    (
        RenderPlugin { headless: None },
        RenderSettings {
            draw_path: Some(DrawPath::CpuDirect),
            frustum_culling: true,
        },
        RendererConfig {
            width: 640,
            height: 360,
            ..Default::default()
        },
        RenderStats {
            draw_calls: 3,
            ..Default::default()
        },
    )
}

fn cameras(world: &mut World, target: TextureHandle) {
    world.spawn((
        Transform::from_xyz(0.0, 1.5, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0)),
    ));
    world.spawn((
        Transform::from_xyz(0.0, 0.0, 3.0),
        Camera3d::default()
            .with_projection(Projection::Orthographic {
                height: 2.0,
                near: 0.1,
                far: 10.0,
            })
            .with_target(RenderTarget::Texture(target)),
        Visibility::Visible,
    ));
}

// ---- Checks ---------------------------------------------------------------------------------

#[test]
fn legacy_forms_keep_their_meaning() {
    let m = see_through(Vec4::ONE);
    assert!(m.unlit && m.double_sided && m.alpha_blend);
    assert_eq!(
        (m.metallic, m.roughness, m.base_color_texture, m.shader),
        (0.0, 0.5, None, None)
    );
    let h = holo_solid(Vec4::new(1.0, 0.5, 0.25, 0.5), 0.2, 0.3);
    assert!(h.alpha_blend && h.double_sided && !h.unlit);
    assert_eq!((h.metallic, h.roughness), (0.2, 0.3));
    assert_eq!(Material::default(), Material::color(Vec4::ONE));

    let (hull, engine) = hull_material();
    assert_eq!(engine.name.as_deref(), Some("SC_Hull"));
    assert_eq!(hull.alpha_mode, AlphaMode::default());

    let mut model = ModelData::from_mesh(MeshData::cube(1.0));
    split_primitive(&mut model, MeshData::cube(0.5), 0);
    assert_eq!(model.meshes[0].primitives.len(), 2);
    assert_eq!(model.meshes[0].primitives[1].material, Some(0));

    let (plugin, settings, config, stats) = plugins_and_settings();
    assert_eq!(plugin.headless, None);
    assert!(settings.frustum_culling);
    assert_eq!((config.width, config.height, config.msaa_samples), (640, 360, 1));
    assert_eq!(stats.draw_calls, 3);
    assert_eq!(STUDIO_AMBIENT.intensity, 0.22);
    assert_eq!(
        [
            OverlayTargetKind::Surface,
            OverlayTargetKind::Offscreen,
            OverlayTargetKind::Capture
        ]
        .map(kind_slot),
        [0, 1, 2]
    );
}

#[test]
fn legacy_scene_setup_builds() {
    let mut app = App::new();
    app.add_plugin(RenderPlugin::default());
    AdditiveParticles::register(&mut app.resources);
    assert!(app.resources.contains::<AdditiveParticles>());
    assert!(app.resources.expect::<RenderOverlays>().contains("additive-particles"));
    let mut assets = RenderAssets::default();
    let mut buffers = InstanceBuffers::default();
    let target = assets.add_render_target(64, 64);
    let _texture = textures(&mut assets, ImageData::new(4, 4), AddressMode::Repeat);
    lights(&mut app.world);
    grid(&mut app.world, &mut assets, &mut buffers);
    cameras(&mut app.world, target);
    app.world.spawn((
        Transform::default(),
        Mesh3d(assets.add_mesh(MeshData::cube(1.0))),
        see_through(Vec4::new(1.0, 1.0, 1.0, 0.62)),
    ));
    app.insert_resource(assets);
    app.insert_resource(buffers);
    app.insert_resource(STUDIO_AMBIENT);
    // Without a renderer the render system is a no-op: simulation-only apps keep working.
    app.step_ticks(2);
}
