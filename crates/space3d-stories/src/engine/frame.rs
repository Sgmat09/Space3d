//! Frame stories (`Engine/Frame/*`): HDR and tonemapping, render scale, multiple cameras, the LDR
//! fallback, node phases, scene depth and overlay stages.
//!
//! Owner: F (graphics spec §3.6).

use super::{ambient, mesh, palette, sun};
use crate::{KnobValues, Story};
use space3d_core::prelude::*;
use space3d_render::{
    Camera3d, CameraFrame, DownlevelOverride, FrameSettings, HdrMode, Material, Mesh3d, PointLight, PostProcess,
    Projection, RenderAssets, RenderTarget, Tonemapper,
};

/// This area's stories, in registry order.
pub(crate) fn stories() -> Vec<Story> {
    vec![hdr_tonemapping(), render_scale(), multi_camera(), ldr_fallback()]
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
