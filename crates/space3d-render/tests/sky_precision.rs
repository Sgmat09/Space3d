//! The sky is independent of the camera's position: far from the origin (85 000 and 430 000
//! units) a skybox does not jitter as the camera moves.
//!
//! The sky pass used to rebuild each pixel's direction from an `f32` inverse of `proj * view`,
//! whose translation column is the camera position: far from the origin that inverse is badly
//! conditioned and the sky landed several pixels off, differently for every camera pose ("the
//! textures flicker when the camera moves"). It now unprojects with `GpuView::inv_proj_rot`,
//! `(proj · view rotation)⁻¹` computed in `f64` (no translation anywhere).
//!
//! # Method
//!
//! A camera renders a high-frequency procedural star/nebula cubemap behind a sphere, near 2 (or
//! 10) and far 300 000, for 64 frames, capturing every frame (`render_to_image`). The sky patch
//! (a 128² window at 1x, 256² at 2x, in the top of the view, clear of the sphere) of each
//! consecutive pair of frames is registered to sub-pixel accuracy (`common::subpixel`: phase
//! correlation, then Gauss-Newton on a bicubic resampling), giving the shift of the sky between
//! the two frames. Two motions:
//!
//! * **translate**: the camera moves 0.03 units per frame, its rotation constant. The sky is
//!   at infinity: the true shift is exactly 0 px.
//! * **yaw**: the same translation while the camera turns slowly (0.06 px per frame at the
//!   patch). The true shift is computed from the two rotations (a fixed world direction
//!   projected through both), and is what the measured shift is compared with.
//!
//! Per run the *residual* (measured − true shift) is reduced to its mean and maximum magnitude
//! and to the number of *reversals* (consecutive frame pairs whose residuals have opposite x
//! sign, both larger than 0.05 px: the sky jumping back and forth). A control at the origin
//! (and the estimator's own tests below) show that what is measured is the renderer, not the
//! method. Runs cover 1x (640 × 360) and 2x (1280 × 720), Direct and HDR (passthrough post)
//! frame modes, `DownlevelOverride::webgl2()` and — with wgpu's ANGLE (see `gl.rs`) — the GL
//! backend.
//!
//! Measured (Metal), before / after the fix, mean |residual| in px (1x, near 2 at 85 000 out):
//! see `docs/graphics/frame.md`, "Sky precision".

mod common;
use common::subpixel::{Gray, Patch, contrast, measure_shift};
use common::*;
use glam::{DQuat, DVec3};
use space3d_core::prelude::*;
use space3d_render::*;
use std::sync::OnceLock;

/// Frames captured per run (63 consecutive pairs).
const FRAMES: usize = 64;
/// World units the camera moves per frame.
const STEP: f64 = 0.03;
/// Residual limits (px): the request's "< 0.1 px" for the mean, a looser bound for the worst
/// frame, and the magnitude above which two opposite residuals count as a reversal.
const MEAN_LIMIT: f64 = 0.1;
const MAX_LIMIT: f64 = 0.3;
const REVERSAL_MIN: f64 = 0.05;
/// Vertical field of view of the camera.
const FOV_Y_DEG: f64 = 60.0;
/// Gaussian low-pass (σ, px) applied to captured frames before registration.
const BLUR: f64 = 1.2;

// ---- the procedural sky ------------------------------------------------------------------

fn hash(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x7feb_352d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846c_a68b);
    h ^ (h >> 16)
}

fn hash3(x: i32, y: i32, z: i32, seed: u32) -> f32 {
    let h = hash(
        (x as u32).wrapping_mul(0x9e37_79b1)
            ^ hash((y as u32).wrapping_mul(0x85eb_ca77) ^ hash((z as u32).wrapping_mul(0xc2b2_ae3d) ^ seed)),
    );
    (h >> 8) as f32 / (1u32 << 24) as f32
}

fn value_noise(p: Vec3, seed: u32) -> f32 {
    let i = p.floor();
    let f = p - i;
    let s = f * f * (Vec3::splat(3.0) - 2.0 * f);
    let (x, y, z) = (i.x as i32, i.y as i32, i.z as i32);
    let h = |dx: i32, dy: i32, dz: i32| hash3(x + dx, y + dy, z + dz, seed);
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let x00 = lerp(h(0, 0, 0), h(1, 0, 0), s.x);
    let x10 = lerp(h(0, 1, 0), h(1, 1, 0), s.x);
    let x01 = lerp(h(0, 0, 1), h(1, 0, 1), s.x);
    let x11 = lerp(h(0, 1, 1), h(1, 1, 1), s.x);
    lerp(lerp(x00, x10, s.y), lerp(x01, x11, s.y), s.z)
}

fn fbm(mut p: Vec3, seed: u32) -> f32 {
    let (mut sum, mut amp) = (0.0, 0.5);
    for octave in 0..5 {
        sum += amp * value_noise(p, seed + octave);
        p *= 2.03;
        amp *= 0.5;
    }
    sum
}

/// Direction of texel `(x, y)` of cube face `face` (wgpu / OpenGL order +X −X +Y −Y +Z −Z).
fn face_direction(face: usize, u: f32, v: f32) -> Vec3 {
    match face {
        0 => Vec3::new(1.0, -v, -u),
        1 => Vec3::new(-1.0, -v, u),
        2 => Vec3::new(u, 1.0, v),
        3 => Vec3::new(u, -1.0, -v),
        4 => Vec3::new(u, -v, 1.0),
        _ => Vec3::new(-u, -v, -1.0),
    }
    .normalize()
}

/// Six 512² faces of a colourful nebula with grain and ~1.5% single-texel stars: detail at
/// every scale down to the texel (about one screen pixel at 1x), so that image registration
/// has something to lock on to everywhere. Built once.
fn sky_faces() -> [ImageData; 6] {
    static FACES: OnceLock<[ImageData; 6]> = OnceLock::new();
    FACES
        .get_or_init(|| {
            const N: u32 = 512;
            std::array::from_fn(|face| {
                let mut img = ImageData::new(N, N);
                for y in 0..N {
                    for x in 0..N {
                        let (u, v) = (
                            (x as f32 + 0.5) / N as f32 * 2.0 - 1.0,
                            (y as f32 + 0.5) / N as f32 * 2.0 - 1.0,
                        );
                        let d = face_direction(face, u, v);
                        let base = fbm(d * 3.0, 11);
                        let tint = fbm(d * 5.0 + Vec3::splat(7.3), 23);
                        let grain = hash3(x as i32, y as i32, face as i32, 5) - 0.5;
                        let star = hash3(x as i32, y as i32, face as i32 + 6, 9);
                        let star = if star > 0.985 {
                            0.35 + 0.65 * (star - 0.985) / 0.015
                        } else {
                            0.0
                        };
                        let c = [
                            0.10 + 0.8 * base * base + 0.25 * grain + star,
                            0.08 + 0.55 * base * tint + 0.25 * grain + star,
                            0.15 + 0.75 * tint * tint + 0.25 * grain + star,
                        ];
                        let byte = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
                        img.set(x, y, [byte(c[0]), byte(c[1]), byte(c[2]), 255]);
                    }
                }
                img
            })
        })
        .clone()
}

// ---- the runs ----------------------------------------------------------------------------

/// Where the camera is and its clip planes.
#[derive(Clone, Copy, Debug)]
struct Pose {
    name: &'static str,
    eye: [f64; 3],
    near: f32,
    far: f32,
}

/// Control: the same clip planes at the origin.
const ORIGIN: Pose = Pose {
    name: "origin",
    eye: [0.0, 0.0, 0.0],
    near: 2.0,
    far: 300_000.0,
};
/// The game's login backdrop: 85 000 units out, near 2.
const FAR_XZ: Pose = Pose {
    name: "(85000,0,85000) near 2",
    eye: [85_000.0, 0.0, 85_000.0],
    near: 2.0,
    far: 300_000.0,
};
/// The game's Skylab stage: y = −430 000, near 10.
const FAR_Y: Pose = Pose {
    name: "y=-430000 near 10",
    eye: [1_200.0, -430_000.0, 900.0],
    near: 10.0,
    far: 300_000.0,
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Motion {
    /// Constant rotation: the sky must not move at all.
    Translate,
    /// A slow turn: the sky moves by a known amount.
    Yaw,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Direct,
    /// Every camera with passthrough post: the HDR scene texture and the composite (the Ldr
    /// fallback under `DownlevelOverride::webgl2()`).
    Hdr,
}

#[derive(Clone, Copy, Debug)]
struct Run {
    width: u32,
    height: u32,
    mode: Mode,
    motion: Motion,
    pose: Pose,
}

impl Run {
    fn label(&self, backend: &str) -> String {
        format!(
            "{backend} {}x{} {:?} {:?} @ {}",
            self.width, self.height, self.mode, self.motion, self.pose.name
        )
    }

    /// Focal length in pixels.
    fn focal(&self) -> f64 {
        self.height as f64 / 2.0 / (FOV_Y_DEG.to_radians() / 2.0).tan()
    }

    /// The patch of sky registered: 128² at 1x (256² at 2x), top centre, clear of the sphere.
    fn patch(&self) -> Patch {
        let size = if self.height >= 720 { 256 } else { 128 };
        Patch {
            x: self.width as usize / 2 - size / 2,
            y: self.height as usize / 45,
            size,
        }
    }

    /// The camera's yaw at frame `i`: constant, or turning 0.06 px per frame at the patch.
    fn yaw(&self, i: usize) -> f64 {
        let rate = match self.motion {
            Motion::Translate => 0.0,
            Motion::Yaw => 0.06 / self.focal(),
        };
        0.4 + rate * i as f64
    }

    fn rotation(&self, i: usize) -> DQuat {
        DQuat::from_euler(glam::EulerRot::YXZ, self.yaw(i), 0.1, 0.0)
    }

    /// The camera position at frame `i`, moving 0.03 units per frame.
    fn eye(&self, i: usize) -> DVec3 {
        DVec3::from(self.pose.eye) + DVec3::new(0.6, 0.35, -0.72).normalize() * STEP * i as f64
    }

    fn transform(&self, i: usize) -> Transform {
        let q = self.rotation(i);
        Transform {
            translation: self.eye(i).as_vec3(),
            rotation: Quat::from_xyzw(q.x as f32, q.y as f32, q.z as f32, q.w as f32),
            scale: Vec3::ONE,
        }
    }

    /// How far the sky of the patch moves from frame `i − 1` to `i` (px, x right, y down): the
    /// mean over a 9 × 9 grid of the patch of the motion of a fixed world direction through the
    /// pixel, projected with both rotations (a turn moves the sky slightly more at the edges
    /// of the view than at its middle, so the patch's translation is the mean).
    fn true_shift(&self, i: usize) -> (f64, f64) {
        let (w, h, f) = (self.width as f64, self.height as f64, self.focal());
        let patch = self.patch();
        let (mut sx, mut sy) = (0.0, 0.0);
        for j in 0..9 {
            for k in 0..9 {
                let px = patch.x as f64 + (patch.size as f64 - 1.0) * k as f64 / 8.0;
                let py = patch.y as f64 + (patch.size as f64 - 1.0) * j as f64 / 8.0;
                let cam = DVec3::new((px + 0.5 - w / 2.0) / f, -(py + 0.5 - h / 2.0) / f, -1.0);
                let world = self.rotation(i - 1) * cam;
                let c = self.rotation(i).inverse() * world;
                sx += w / 2.0 + f * c.x / -c.z - 0.5 - px;
                sy += h / 2.0 - f * c.y / -c.z - 0.5 - py;
            }
        }
        (sx / 81.0, sy / 81.0)
    }
}

/// The app of a run: its renderer (`make`), a skybox camera at the pose and a sphere in front
/// of it.
fn build(make: &dyn Fn(u32, u32) -> Option<App>, run: &Run) -> Option<(App, Entity)> {
    let mut app = make(run.width, run.height)?;
    let (sky, sphere) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        (
            a.add_cubemap(CubemapSource::from_faces(sky_faces()).expect("six equal faces")),
            a.add_mesh(MeshData::sphere(7.0, 32, 16)),
        )
    };
    let camera = app.world.spawn((
        run.transform(0),
        Camera3d::default()
            .with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0))
            .with_projection(Projection::Perspective {
                fov_y: FOV_Y_DEG.to_radians() as f32,
                near: run.pose.near,
                far: run.pose.far,
            }),
        Skybox::new(sky),
    ));
    if run.mode == Mode::Hdr {
        app.world.insert_one(camera, PostProcess::passthrough()).unwrap();
    }
    // Ten units below and 34 in front of the first frame's camera: the bottom centre of the view.
    let q = run.rotation(0);
    let at = run.eye(0) + q * DVec3::new(0.0, -10.0, -34.0);
    app.world.spawn((
        Transform::from_xyz(at.x as f32, at.y as f32, at.z as f32),
        Mesh3d(sphere),
        Material::unlit(Vec4::new(0.9, 0.5, 0.2, 1.0)),
    ));
    Some((app, camera))
}

/// The measured and true shifts of every consecutive frame pair of a run.
struct Series {
    measured: Vec<(f64, f64)>,
    truth: Vec<(f64, f64)>,
}

/// What a series comes to (px).
#[derive(Debug)]
struct Summary {
    mean_shift: f64,
    mean_residual: f64,
    max_residual: f64,
    reversals: usize,
}

impl Series {
    fn summary(&self) -> Summary {
        let n = self.measured.len() as f64;
        let residual: Vec<(f64, f64)> = self
            .measured
            .iter()
            .zip(&self.truth)
            .map(|(m, t)| (m.0 - t.0, m.1 - t.1))
            .collect();
        let len = |v: &(f64, f64)| v.0.hypot(v.1);
        Summary {
            mean_shift: self.measured.iter().map(len).sum::<f64>() / n,
            mean_residual: residual.iter().map(len).sum::<f64>() / n,
            max_residual: residual.iter().map(len).fold(0.0, f64::max),
            reversals: residual
                .windows(2)
                .filter(|w| w[0].0 * w[1].0 < 0.0 && w[0].0.abs().min(w[1].0.abs()) > REVERSAL_MIN)
                .count(),
        }
    }
}

/// Render and register a run: `None` without a renderer.
fn measure(make: &dyn Fn(u32, u32) -> Option<App>, run: &Run) -> Option<Series> {
    let (mut app, camera) = build(make, run)?;
    let patch = run.patch();
    // Every second pixel of a 256² patch is registered.
    let stride = patch.size / 128;
    let mut previous: Option<Gray> = None;
    let mut series = Series {
        measured: Vec::new(),
        truth: Vec::new(),
    };
    for i in 0..FRAMES {
        *app.world.get::<&mut Transform>(camera).unwrap() = run.transform(i);
        let img = render_to_image(&mut app)?;
        assert_eq!((img.width, img.height), (run.width, run.height));
        let raw = Gray::from_image(&img);
        let gray = raw.blurred(BLUR);
        if i == 0 {
            assert!(contrast(&raw, patch) > 0.05, "the sky patch has no texture");
            let intermediate = app.resources.expect::<RenderStats>().intermediate_groups;
            assert_eq!(intermediate > 0, run.mode == Mode::Hdr, "frame mode of {run:?}");
        }
        if let Some(prev) = &previous {
            series.measured.push(measure_shift(prev, &gray, patch, stride));
            series.truth.push(run.true_shift(i));
        }
        previous = Some(gray);
    }
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
    Some(series)
}

/// Measure `run` and hold its sky to the limits: mean residual < 0.1 px, no frame beyond 0.3,
/// no reversals.
fn check(backend: &str, make: &dyn Fn(u32, u32) -> Option<App>, run: Run) -> bool {
    let Some(series) = measure(make, &run) else {
        return false;
    };
    let s = series.summary();
    if std::env::var_os("SKY_DEBUG").is_some() {
        for (m, t) in series.measured.iter().zip(&series.truth).take(6) {
            eprintln!("  measured ({:.4}, {:.4}) true ({:.4}, {:.4})", m.0, m.1, t.0, t.1);
        }
    }
    eprintln!(
        "sky shift {}: mean |shift| {:.4} px, mean |residual| {:.4}, max |residual| {:.4}, reversals {}",
        run.label(backend),
        s.mean_shift,
        s.mean_residual,
        s.max_residual,
        s.reversals
    );
    // `SKY_REPORT=1`: print every run's numbers without asserting (to tabulate a failing build).
    if std::env::var_os("SKY_REPORT").is_some() {
        return true;
    }
    assert!(
        s.mean_residual < MEAN_LIMIT,
        "{}: the sky moves {:.3} px per frame off the truth (limit {MEAN_LIMIT}): {s:?}",
        run.label(backend),
        s.mean_residual
    );
    assert!(
        s.max_residual < MAX_LIMIT,
        "{}: a frame's sky is {:.3} px off the truth (limit {MAX_LIMIT}): {s:?}",
        run.label(backend),
        s.max_residual
    );
    assert_eq!(
        s.reversals,
        0,
        "{}: the sky jumps back and forth: {s:?}",
        run.label(backend)
    );
    true
}

const POSES: [Pose; 3] = [ORIGIN, FAR_XZ, FAR_Y];
const MOTIONS: [Motion; 2] = [Motion::Translate, Motion::Yaw];

fn default_app(width: u32, height: u32) -> Option<App> {
    headless_app(width, height)
}

fn webgl2_app(width: u32, height: u32) -> Option<App> {
    let mut app = headless_app(width, height)?;
    app.insert_resource(FrameSettings::default().with_downlevel(DownlevelOverride::webgl2()));
    Some(app)
}

/// A renderer on wgpu's GL backend (ANGLE on macOS: see `gl.rs`), or `None`.
fn gl_app(width: u32, height: u32) -> Option<App> {
    let mut app = App::new();
    app.add_plugin(RenderPlugin::default());
    let renderer = pollster::block_on(Renderer::new_headless_with(RendererConfig {
        width,
        height,
        backends: Some(wgpu::Backends::GL),
        ..Default::default()
    }));
    match renderer {
        Ok(r) => {
            assert_eq!(r.capabilities().adapter.backend, wgpu::Backend::Gl);
            app.insert_resource(r);
            Some(app)
        }
        Err(e) => {
            eprintln!("skipping GL sky test: no GL adapter ({e})");
            None
        }
    }
}

fn all_runs(width: u32, height: u32, mode: Mode) -> Vec<Run> {
    POSES
        .into_iter()
        .flat_map(|pose| {
            MOTIONS.into_iter().map(move |motion| Run {
                width,
                height,
                mode,
                motion,
                pose,
            })
        })
        .collect()
}

#[test]
fn sky_is_stable_far_from_origin_direct_1x() {
    for run in all_runs(640, 360, Mode::Direct) {
        if !check("default", &default_app, run) {
            return;
        }
    }
}

#[test]
fn sky_is_stable_far_from_origin_direct_2x() {
    for run in all_runs(1280, 720, Mode::Direct) {
        if !check("default", &default_app, run) {
            return;
        }
    }
}

#[test]
fn sky_is_stable_far_from_origin_hdr_1x() {
    for run in all_runs(640, 360, Mode::Hdr) {
        if !check("default", &default_app, run) {
            return;
        }
    }
}

#[test]
fn sky_is_stable_far_from_origin_hdr_2x() {
    for run in all_runs(1280, 720, Mode::Hdr) {
        if !check("default", &default_app, run) {
            return;
        }
    }
}

/// `DownlevelOverride::webgl2()`: no float targets (the Ldr fallback for HDR), no compute, no
/// read-only depth.
#[test]
fn sky_is_stable_far_from_origin_webgl2_downlevel() {
    for mode in [Mode::Direct, Mode::Hdr] {
        for run in all_runs(640, 360, mode) {
            if !check("webgl2()", &webgl2_app, run) {
                return;
            }
        }
    }
}

/// The GL backend (ANGLE on macOS, Mesa on Linux): skips without an adapter. One test: ANGLE
/// on macOS crashes when several GL contexts render from different threads at once.
#[test]
fn sky_is_stable_far_from_origin_gl_backend() {
    if gl_app(8, 8).is_none() {
        return;
    }
    for mode in [Mode::Direct, Mode::Hdr] {
        for run in all_runs(640, 360, mode) {
            if !check("gl", &gl_app, run) {
                return;
            }
        }
    }
}

// ---- the method itself -------------------------------------------------------------------

/// The registration recovers known shifts of a synthetic texture (8-bit, sub-pixel) to 0.03 px,
/// from a fraction of a pixel to 17 px.
#[test]
fn estimator_recovers_known_shifts() {
    // Ten plane waves up to 0.3 cycles/px.
    let mut state = 0x1234_5678u32;
    let mut next = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 8) as f64 / (1u32 << 24) as f64
    };
    let waves: Vec<[f64; 4]> = (0..10)
        .map(|_| {
            [
                next() * 0.6 - 0.3,
                next() * 0.6 - 0.3,
                next() * 6.28,
                0.04 + next() * 0.08,
            ]
        })
        .collect();
    let texture = |x: f64, y: f64| {
        let v: f64 = waves
            .iter()
            .map(|w| w[3] * (std::f64::consts::TAU * (w[0] * x + w[1] * y) + w[2]).sin())
            .sum();
        (0.5 + v).clamp(0.0, 1.0)
    };
    let image = |dx: f64, dy: f64| {
        let mut img = ImageData::new(256, 256);
        for y in 0..256u32 {
            for x in 0..256u32 {
                let g = (texture(x as f64 - dx, y as f64 - dy) * 255.0).round() as u8;
                img.set(x, y, [g, g, g, 255]);
            }
        }
        Gray::from_image(&img).blurred(BLUR)
    };
    let base = image(0.0, 0.0);
    let patch = Patch {
        x: 64,
        y: 64,
        size: 128,
    };
    for (dx, dy) in [
        (0.0, 0.0),
        (0.05, -0.02),
        (0.3, -0.2),
        (-0.5, 0.45),
        (9.6, -4.3),
        (-17.25, 12.1),
    ] {
        let (mx, my) = measure_shift(&base, &image(dx, dy), patch, 1);
        assert!(
            (mx - dx).abs() < 0.03 && (my - dy).abs() < 0.03,
            "true ({dx}, {dy}), measured ({mx:.4}, {my:.4})"
        );
    }
}
