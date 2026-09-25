//! Frame stories (`Engine/Frame/*`): HDR and tonemapping, render scale, multiple cameras, the LDR
//! fallback, node phases, scene depth and overlay stages.
//!
//! Owner: F (graphics spec §3.6).

mod demo;

use super::{ambient, mesh, palette, sun};
use crate::{KnobValues, Story};
use demo::{AdditiveSprites, Band, BandKind, Particle, PhaseBands, SoftParticles, Sprite};
use space3d_core::prelude::*;
use space3d_render::{
    Camera3d, CameraFrame, DownlevelOverride, FrameSettings, HdrMode, Material, Mesh3d, PhaseSlot, PointLight,
    PostProcess, Projection, RenderAssets, RenderNodes, RenderOverlays, RenderPhase, RenderTarget, RenderTargetDesc,
    SceneReads, Tonemapper,
};

/// This area's stories, in registry order.
pub(crate) fn stories() -> Vec<Story> {
    vec![
        hdr_tonemapping(),
        render_scale(),
        multi_camera(),
        ldr_fallback(),
        node_phases(),
        soft_depth(),
        overlay_stages(),
    ]
}

const TONEMAPPERS: [&str; 5] = [
    "AgX",
    "ACES (fitted)",
    "Reinhard",
    "Khronos PBR Neutral",
    "None (clamp)",
];

fn tonemapper(k: &KnobValues) -> Tonemapper {
    match k.choice("tonemapper") {
        0 => Tonemapper::AgX,
        1 => Tonemapper::AcesFitted,
        2 => Tonemapper::Reinhard,
        3 => Tonemapper::KhronosPbrNeutral,
        _ => Tonemapper::None,
    }
}

/// Dark-space backdrop of the frame stories (linear).
const SPACE: Vec4 = Vec4::new(0.004, 0.005, 0.012, 1.0);

fn at(p: Vec3) -> Transform {
    Transform::from_xyz(p.x, p.y, p.z)
}

/// Emissive ramps 0.05…64 (grey, then saturated orange and blue), lit spheres under a bright
/// light for reference, and the camera with `post`. Shared by the HDR and LDR-fallback stories.
fn emissive_ramp(app: &mut App, post: PostProcess, frame: CameraFrame) {
    const N: usize = 12;
    let cube = mesh(app, MeshData::cube(0.4));
    let sphere = mesh(app, MeshData::sphere(0.45, 32, 16));
    let ground = mesh(app, MeshData::plane(14.0));
    for (row, hue) in [Vec3::ONE, Vec3::new(1.0, 0.42, 0.1), Vec3::new(0.15, 0.45, 1.0)]
        .into_iter()
        .enumerate()
    {
        for i in 0..N {
            let radiance = 0.05 * (64.0f32 / 0.05).powf(i as f32 / (N - 1) as f32);
            app.world.spawn((
                Transform::from_xyz(-2.75 + i as f32 * 0.5, 1.05 - row as f32 * 0.48, 0.0),
                Mesh3d(cube),
                Material::unlit((hue * radiance).extend(1.0)),
            ));
        }
    }
    for (i, x) in [-1.6f32, 0.0, 1.6].into_iter().enumerate() {
        app.world.spawn((
            Transform::from_xyz(x, -0.95, 0.4),
            Mesh3d(sphere),
            Material::color(palette(i * 3).extend(1.0)).with_metallic_roughness(0.0, 0.35),
        ));
    }
    app.world.spawn((
        Transform::from_xyz(0.0, -1.4, 0.0),
        Mesh3d(ground),
        Material::srgb(0.16, 0.17, 0.2).with_metallic_roughness(0.0, 0.8),
    ));
    // A bright light: highlights far beyond 1.0 on the spheres.
    app.world.spawn((
        Transform::from_xyz(0.0, 1.0, 2.5),
        PointLight {
            color: Vec3::new(1.0, 0.9, 0.75),
            intensity: 40.0,
            range: 10.0,
        },
    ));
    sun(app, Vec3::new(-0.3, -1.0, -0.5), 1.5);
    ambient(app, 0.05);
    app.world.spawn((
        Name("camera".into()),
        Transform::from_xyz(0.0, 0.15, 4.4).looking_at(Vec3::new(0.0, 0.15, 0.0), Vec3::Y),
        Camera3d::default().with_clear_color(SPACE),
        post,
        frame,
    ));
}

fn hdr_tonemapping() -> Story {
    Story::new("Engine/Frame/HDR Tonemapping", |app, k| {
        let hdr = match k.choice("hdr") {
            1 => HdrMode::Ldr,
            2 => HdrMode::Compact,
            _ => HdrMode::Auto,
        };
        let post = PostProcess::default()
            .with_tonemapper(tonemapper(k))
            .with_exposure(k.f32("exposure"))
            .without_bloom();
        emissive_ramp(app, post, CameraFrame::default().with_hdr(hdr));
    })
    .description(
        "Unlit emissive ramps from 0.05 to 64 (grey, orange, blue) and lit spheres under a bright \
         point light, through the HDR scene texture and the composite: exposure in stops and the \
         tonemapper on knobs (AgX by default), and the scene storage (`HdrMode`: Auto = \
         Rgba16Float, Ldr = per-fragment tonemapping, Compact = Rg11b10Ufloat).",
    )
    .knob_choice("tonemapper", &TONEMAPPERS, 0)
    .knob_f32("exposure", 0.0, -4.0..=4.0)
    .knob_choice("hdr", &["Auto", "Ldr", "Compact"], 0)
    .tag("graphics")
    .size(320, 180)
}

fn ldr_fallback() -> Story {
    Story::new("Engine/Frame/LDR Fallback", |app, k| {
        app.insert_resource(FrameSettings::default().with_downlevel(DownlevelOverride {
            no_float_targets: true,
            ..Default::default()
        }));
        let post = PostProcess::default()
            .with_tonemapper(tonemapper(k))
            .with_exposure(k.f32("exposure"))
            .without_bloom();
        emissive_ramp(app, post, CameraFrame::default());
    })
    .description(
        "The HDR Tonemapping scene on a device without float render targets (forced with \
         `DownlevelOverride::no_float_targets`, as on WebGL2 without EXT_color_buffer_float): meshes \
         and clear colors are exposed and tonemapped per fragment into an LDR scene texture, so the \
         image matches the HDR path up to blending and 8-bit precision.",
    )
    .knob_choice("tonemapper", &TONEMAPPERS, 0)
    .knob_f32("exposure", 0.0, -4.0..=4.0)
    .tag("graphics")
    .size(320, 180)
}

fn render_scale() -> Story {
    Story::new("Engine/Frame/Render Scale", |app, k| {
        app.insert_resource(FrameSettings::default().with_render_scale(k.f32("scale")));
        let bar = mesh(app, MeshData::cube(1.0));
        let sphere = mesh(app, MeshData::sphere(0.7, 48, 24));
        // Thin bars and a sphere: fine detail that shows the lower scene resolution.
        for i in 0..16 {
            let width = 0.02 + 0.01 * i as f32;
            app.world.spawn((
                Transform::from_xyz(-2.4 + i as f32 * 0.32, 0.6, 0.0)
                    .with_rotation(Quat::from_rotation_z(0.15))
                    .with_scale(Vec3::new(width, 1.2, 0.1)),
                Mesh3d(bar),
                Material::unlit(palette(i).extend(1.0)),
            ));
        }
        app.world.spawn((
            Transform::from_xyz(0.0, -0.7, 0.0),
            Mesh3d(sphere),
            Material::srgb(0.85, 0.85, 0.9).with_metallic_roughness(0.0, 0.3),
        ));
        sun(app, Vec3::new(-0.4, -0.8, -0.6), 2.5);
        ambient(app, 0.1);
        app.world.spawn((
            Name("camera".into()),
            Transform::from_xyz(0.0, 0.0, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
            Camera3d::default().with_clear_color(SPACE),
        ));
    })
    .description(
        "`FrameSettings::render_scale` without post: the scene renders into a smaller texture \
         (the Ldr passthrough group) and the composite upscales it bilinearly to the target; \
         overlays stay at full resolution.",
    )
    .knob_f32("scale", 0.5, 0.25..=1.0)
    .tag("graphics")
    .size(320, 180)
}

/// A ship-like shape at `p`: hull, wings and two engine glows of radiance `glow`.
fn ship(app: &mut App, p: Vec3, tint: Vec3, glow: f32) {
    let hull = mesh(app, MeshData::cube(1.0));
    let engine = mesh(app, MeshData::sphere(0.5, 16, 8));
    let yaw = Quat::from_rotation_y(0.5);
    app.world.spawn((
        at(p).with_rotation(yaw).with_scale(Vec3::new(0.5, 0.3, 1.4)),
        Mesh3d(hull),
        Material::color(tint.extend(1.0)).with_metallic_roughness(0.6, 0.35),
    ));
    app.world.spawn((
        at(p + Vec3::new(0.0, -0.05, 0.1))
            .with_rotation(yaw)
            .with_scale(Vec3::new(1.8, 0.08, 0.5)),
        Mesh3d(hull),
        Material::color((tint * 0.6).extend(1.0)).with_metallic_roughness(0.4, 0.5),
    ));
    for side in [-0.22f32, 0.22] {
        app.world.spawn((
            at(p + yaw * Vec3::new(side, 0.0, 0.75)).with_scale(Vec3::splat(0.22)),
            Mesh3d(engine),
            Material::unlit((Vec3::new(0.4, 0.7, 1.0) * glow).extend(1.0)),
        ));
    }
}

fn multi_camera() -> Story {
    Story::new("Engine/Frame/Multi Camera", |app, k| {
        let glow = k.f32("glow");
        let exposure = k.f32("exposure");
        // Two ships far apart (each half of the split screen sees one) and a gizmo tripod in its
        // own corner of the world.
        ship(app, Vec3::new(-40.0, 0.0, 0.0), Vec3::new(0.6, 0.62, 0.7), glow);
        ship(app, Vec3::new(40.0, 0.0, 0.0), Vec3::new(0.7, 0.4, 0.3), glow);
        let axis = mesh(app, MeshData::cube(1.0));
        let tripod = [
            (Vec3::X, Vec3::new(1.0, 0.06, 0.06), Vec3::new(1.0, 0.1, 0.1)),
            (Vec3::Y, Vec3::new(0.06, 1.0, 0.06), Vec3::new(0.1, 1.0, 0.1)),
            (Vec3::Z, Vec3::new(0.06, 0.06, 1.0), Vec3::new(0.2, 0.3, 1.0)),
        ];
        for (dir, scale, color) in tripod {
            app.world.spawn((
                at(Vec3::new(0.0, 200.0, 0.0) + dir * 0.5).with_scale(scale),
                Mesh3d(axis),
                Material::unlit(color.extend(1.0)),
            ));
        }
        sun(app, Vec3::new(-0.3, -0.9, -0.5), 2.5);
        ambient(app, 0.06);
        // A render-target preview of the right ship (transparent clear, post), shown alpha-blended
        // on a floating panel next to the left ship.
        let rt = app.resources.init::<RenderAssets>().add_render_target(128, 96);
        let panel = mesh(app, MeshData::plane(1.0));
        app.world.spawn((
            Transform::from_xyz(-40.2, 1.25, 1.3)
                .with_rotation(Quat::from_rotation_y(0.45) * Quat::from_rotation_x(std::f32::consts::FRAC_PI_2))
                .with_scale(Vec3::new(1.2, 1.0, 0.9)),
            Mesh3d(panel),
            Material {
                alpha_blend: true,
                double_sided: true,
                ..Material::unlit(Vec4::ONE).with_texture(rt)
            },
        ));
        let look = |x: f32| Transform::from_xyz(x + 1.6, 1.2, 3.2).looking_at(Vec3::new(x, 0.1, 0.0), Vec3::Y);
        let post = PostProcess::default().without_bloom().with_exposure(exposure);
        app.world.spawn((
            Name("rt camera".into()),
            look(40.0),
            Camera3d {
                order: -1,
                clear_color: Some(Vec4::ZERO),
                target: RenderTarget::Texture(rt),
                ..Default::default()
            },
            post.with_tonemapper(Tonemapper::AcesFitted),
        ));
        // Split screen: two post cameras (AgX left, ACES right).
        app.world.spawn((
            Name("camera".into()),
            look(-40.0),
            Camera3d {
                viewport: Some([0.0, 0.0, 0.5, 1.0]),
                clear_color: Some(SPACE),
                ..Default::default()
            },
            post,
        ));
        app.world.spawn((
            Name("right camera".into()),
            look(40.0),
            Camera3d {
                order: 1,
                viewport: Some([0.5, 0.0, 0.5, 1.0]),
                clear_color: Some(Vec4::new(0.012, 0.005, 0.004, 1.0)),
                ..Default::default()
            },
            post.with_tonemapper(Tonemapper::AcesFitted),
        ));
        // Picture-in-picture post camera (1.5 stops brighter) in the right half's corner.
        app.world.spawn((
            Name("pip camera".into()),
            Transform::from_xyz(40.0, 4.0, 0.01).looking_at(Vec3::new(40.0, 0.0, 0.0), Vec3::Y),
            Camera3d {
                order: 2,
                viewport: Some([0.72, 0.04, 0.25, 0.3]),
                clear_color: Some(Vec4::new(0.05, 0.05, 0.08, 1.0)),
                ..Default::default()
            },
            post.with_exposure(exposure + 1.5),
        ));
        // Gizmo camera over the left half, no post, no clear: a display view drawn LDR on top
        // (the tripod lands in the half's lower left corner).
        app.world.spawn((
            Name("gizmo camera".into()),
            Transform::from_xyz(7.0, 205.0, 9.0).looking_at(Vec3::new(1.7, 201.9, 0.9), Vec3::Y),
            Camera3d {
                order: 10,
                viewport: Some([0.0, 0.0, 0.5, 1.0]),
                clear_color: None,
                projection: Projection::Perspective {
                    fov_y: 0.35,
                    near: 0.1,
                    far: 50.0,
                },
                ..Default::default()
            },
        ));
    })
    .description(
        "Post groups over several cameras: a split screen of two post cameras (AgX left, ACES right, \
         each with its own composite), a brighter picture-in-picture post camera overwriting its \
         rect, a gizmo camera without post drawn LDR over the left half after its composite (a \
         display view), and a render-target preview with a transparent clear and post, shown \
         alpha-blended on a panel.",
    )
    .knob_f32("exposure", 0.0, -3.0..=3.0)
    .knob_f32("glow", 12.0, 0.0..=40.0)
    .tag("graphics")
    .size(320, 180)
}

fn node_phases() -> Story {
    Story::new("Engine/Frame/Node Phases", |app, _| {
        let cube = mesh(app, MeshData::cube(0.9));
        let ground = mesh(app, MeshData::plane(14.0));
        let panel = mesh(app, MeshData::plane(1.0));
        for (i, x) in [-1.7f32, 0.0, 1.7].into_iter().enumerate() {
            app.world.spawn((
                at(Vec3::new(x, 0.0, 0.0)).with_rotation(Quat::from_rotation_y(0.5)),
                Mesh3d(cube),
                Material::color(palette(i * 2 + 1).extend(1.0)).with_metallic_roughness(0.0, 0.45),
            ));
        }
        app.world.spawn((
            at(Vec3::new(0.0, -0.45, 0.0)),
            Mesh3d(ground),
            Material::srgb(0.3, 0.31, 0.34).with_metallic_roughness(0.0, 0.9),
        ));
        // The built-in blended batch: a translucent panel on the right.
        app.world.spawn((
            at(Vec3::new(1.25, 0.35, 1.6))
                .with_rotation(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2))
                .with_scale(Vec3::new(1.4, 1.0, 1.3)),
            Mesh3d(panel),
            Material {
                alpha_blend: true,
                double_sided: true,
                ..Material::unlit(Vec4::new(0.15, 0.55, 1.0, 0.55))
            },
        ));
        sun(app, Vec3::new(-0.4, -1.0, -0.6), 2.2);
        ambient(app, 0.08);
        app.world.spawn((
            Name("camera".into()),
            Transform::from_xyz(0.0, 1.1, 5.2).looking_at(Vec3::new(0.0, 0.2, 0.0), Vec3::Y),
            Camera3d::default().with_clear_color(SPACE),
        ));
        let (op, sky, tr) = (RenderPhase::Opaque, RenderPhase::Skybox, RenderPhase::Transparent);
        let band = |slot, kind, rect, color| Band {
            slot,
            kind,
            rect,
            color,
        };
        let bands = vec![
            // Before the built-in opaque batches, far away: the cubes and the ground cover it,
            // the sky does not.
            band(
                PhaseSlot::new(op, -1),
                BandKind::Backdrop,
                [-1.0, 0.02, 1.0, 0.22],
                [1.0, 0.45, 0.05, 1.0],
            ),
            // After them: over the left cube.
            band(
                PhaseSlot::new(op, 10),
                BandKind::Flat,
                [-0.36, -1.0, -0.28, 1.0],
                [0.9, 0.08, 0.08, 1.0],
            ),
            // At the far plane, only where nothing was drawn.
            band(
                PhaseSlot::new(sky, 0),
                BandKind::Sky,
                [-1.0, -1.0, 1.0, 1.0],
                [0.12, 0.2, 0.5, 1.0],
            ),
            // Before the built-in blended panel (under it), after it (over it).
            band(
                PhaseSlot::new(tr, -1),
                BandKind::Flat,
                [0.08, 0.0, 0.62, 0.14],
                [0.1, 0.95, 0.3, 0.9],
            ),
            band(
                PhaseSlot::new(tr, 5),
                BandKind::Flat,
                [0.08, -0.26, 0.62, -0.12],
                [0.95, 0.2, 0.8, 0.9],
            ),
            // Reading scene depth, after everything: haze growing with distance.
            band(
                PhaseSlot::new(tr, 10).reading(SceneReads::DEPTH),
                BandKind::Haze,
                [-1.0, -1.0, 1.0, 0.02],
                [0.55, 0.6, 0.75, 0.7],
            ),
        ];
        app.resources
            .init::<RenderNodes>()
            .insert("stories/phase-bands", PhaseBands::new(bands));
    })
    .description(
        "A render node with a colored band in every phase around the built-in batches: an orange \
         Opaque band at order -1, far away (under the cubes and the ground), a red Opaque band at \
         order 10 (over them), a sky gradient in the Skybox phase (only where nothing was drawn), \
         a green Transparent band at order -1 (under the translucent built-in panel) and a magenta \
         one at order 5 (over it), and a depth-reading Transparent slot adding haze with scene \
         distance below the horizon (its SceneRead pass).",
    )
    .tag("graphics")
    .size(320, 180)
}

fn soft_depth() -> Story {
    Story::new("Engine/Frame/Soft Depth", |app, k| {
        // The scene renders into a render target with the knob's sample count (an app's MSAA
        // is set on its renderer; a story can only choose it per render target), shown
        // full-screen by the primary camera.
        let samples = if k.choice("msaa") == 1 { 4 } else { 1 };
        let (w, h) = (320, 180);
        let rt = app.resources.init::<RenderAssets>().add_render_target_with(
            w,
            h,
            RenderTargetDesc::default().with_samples(samples),
        );
        app.insert_resource(FrameSettings::default().with_downlevel(DownlevelOverride {
            no_read_only_depth: k.bool("no_read_only_depth"),
            ..Default::default()
        }));
        // The analytic plane: a flat ground at y = 0.
        let ground = mesh(app, MeshData::plane(16.0));
        app.world.spawn((
            Transform::IDENTITY,
            Mesh3d(ground),
            Material::srgb(0.32, 0.34, 0.38).with_metallic_roughness(0.0, 0.85),
        ));
        let pillar = mesh(app, MeshData::cube(1.0));
        app.world.spawn((
            at(Vec3::new(0.0, 0.6, -1.6)).with_scale(Vec3::new(0.5, 1.2, 0.5)),
            Mesh3d(pillar),
            Material::srgb(0.6, 0.55, 0.5),
        ));
        sun(app, Vec3::new(-0.3, -1.0, -0.4), 2.0);
        ambient(app, 0.1);
        // Particles centered on the ground (half below it): hard in the back row, soft in front.
        let softness = k.f32("softness");
        let mut particles = Vec::new();
        for (row, (z, soft)) in [(-0.6f32, 0.0f32), (1.0, softness)].into_iter().enumerate() {
            for i in 0..5 {
                let hue = palette(i + row * 3);
                particles.push(Particle {
                    position: [-2.4 + i as f32 * 1.2, 0.0, z],
                    radius: 0.6,
                    color: (hue * 2.0).to_array(),
                    softness: soft,
                });
            }
        }
        app.resources.init::<RenderNodes>().insert(
            "stories/soft-particles",
            SoftParticles::new(particles, RenderTarget::Texture(rt)),
        );
        app.world.spawn((
            Name("scene camera".into()),
            Transform::from_xyz(0.0, 1.6, 4.8).looking_at(Vec3::new(0.0, 0.2, 0.0), Vec3::Y),
            Camera3d {
                order: -1,
                target: RenderTarget::Texture(rt),
                ..Camera3d::default().with_clear_color(SPACE)
            },
        ));
        // The render target on a full-screen quad, far from the scene.
        let screen = mesh(app, MeshData::plane(1.0));
        let aspect = w as f32 / h as f32;
        app.world.spawn((
            at(Vec3::new(0.0, -100.0, 0.0))
                .with_rotation(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2))
                .with_scale(Vec3::new(2.0 * aspect, 1.0, 2.0)),
            Mesh3d(screen),
            Material::unlit(Vec4::ONE).with_texture(rt),
        ));
        app.world.spawn((
            Name("camera".into()),
            Transform::from_xyz(0.0, -100.0, 5.0),
            Camera3d::default()
                .with_clear_color(SPACE)
                .with_projection(Projection::Orthographic {
                    height: 2.0,
                    near: 0.1,
                    far: 10.0,
                }),
        ));
    })
    .description(
        "Additive particles centered on a flat ground, drawn by a render node whose Transparent \
         slot reads scene depth: the back row intersects the ground hard, the front row fades \
         within `softness` of it. The scene renders into a render target with `msaa` samples: \
         with 4 the depth is resolved (minimum of the samples) for the single-sample SceneRead \
         pass; with `no_read_only_depth` (the WebGL2 path) depth is sampled without an \
         attachment and tested manually — and with both, the particles draw inside the \
         multisampled pass without fade (GLES cannot sample multisampled depth).",
    )
    .knob_choice("msaa", &["1×", "4×"], 0)
    .knob_bool("no_read_only_depth", false)
    .knob_f32("softness", 0.6, 0.0..=2.0)
    .tag("graphics")
    .size(320, 180)
}

fn overlay_stages() -> Story {
    Story::new("Engine/Frame/Overlay Stages", |app, k| {
        let gain = k.f32("intensity");
        let sphere = mesh(app, MeshData::sphere(1.3, 48, 24));
        app.world.spawn((
            at(Vec3::new(0.0, -1.2, -1.0)),
            Mesh3d(sphere),
            Material::srgb(0.35, 0.45, 0.6).with_metallic_roughness(0.0, 0.7),
        ));
        sun(app, Vec3::new(-0.6, -0.4, -0.7), 2.5);
        ambient(app, 0.03);
        app.world.spawn((
            Name("camera".into()),
            Transform::from_xyz(0.0, 0.0, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
            Camera3d::default().with_clear_color(SPACE),
            PostProcess::default().with_exposure(k.f32("exposure")),
        ));
        // The same sprites twice: left half through HdrScene (tonemapped with the scene), right
        // half through AfterTonemap (soft-clipped by the overlay itself, 249c874 style).
        let sprites = |x0: f32| -> Vec<Sprite> {
            (0..6)
                .map(|i| {
                    let intensity = 0.5 * 2f32.powi(i) * gain;
                    let tint = if i % 2 == 0 {
                        Vec3::new(1.0, 0.55, 0.2)
                    } else {
                        Vec3::new(0.3, 0.6, 1.0)
                    };
                    Sprite {
                        center: [x0 + (i % 3) as f32 * 0.13, 0.32 + (i / 3) as f32 * 0.36],
                        radius: 0.11,
                        color: (tint * intensity).to_array(),
                    }
                })
                .collect()
        };
        let overlays = app.resources.init::<RenderOverlays>();
        overlays.insert(
            "stories/sprites-hdr-scene",
            -100,
            AdditiveSprites::new(true, sprites(0.1)),
        );
        overlays.insert(
            "stories/sprites-after-tonemap",
            -100,
            AdditiveSprites::new(false, sprites(0.62)),
        );
    })
    .description(
        "The same additive sprites (intensity 0.5 to 16) as overlays over a post-processed scene: \
         left through `OverlayStage::HdrScene`, into the HDR scene color before post (exposed and \
         tonemapped with the scene; bloomed once bloom exists), right through the default \
         `AfterTonemap` stage, soft-clipped by the overlay itself as 249c874-era overlays do.",
    )
    .knob_f32("intensity", 1.0, 0.0..=4.0)
    .knob_f32("exposure", 0.0, -3.0..=3.0)
    .tag("graphics")
    .size(320, 180)
}
