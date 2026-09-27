//! Showcase scenes: the SpaceCorps look (a close-up skirmish) and the Battle reference scene
//! (graphics spec §4.4), built from [`super::ShowcaseAssets`].

use super::content::{FACTIONS, ShowcaseAssets, hash, sun_direction, unit};
use space3d_core::glam::EulerRot;
use space3d_core::prelude::*;
use space3d_core::{Resources, World};
use space3d_particles::prelude::*;
use space3d_particles::testing::{Battle, BattleLog};
use space3d_render::{
    AmbientLight, BlendMode, Camera3d, DirectionalLight, DirectionalShadow, EnvironmentLight, GraphicsQuality,
    Material, Mesh3d, PostProcess, Projection, RenderAssets, RenderTarget, RenderTargetDesc, Renderer, ShadowFit,
    Skybox, Tonemapper, Vignette,
};

/// Clear color behind the sky (only visible before the sky is uploaded).
pub const SPACE: Vec4 = Vec4::new(0.002, 0.003, 0.007, 1.0);

/// The recommended SpaceCorps look: AgX punchy half a stop up (bright hulls, saturated
/// engines), `Bloom::glow()` (emissive and particles above 1 glow), a light vignette,
/// distortion on (shockwaves, heat haze).
pub fn battle_look() -> PostProcess {
    PostProcess::default()
        .with_tonemapper(Tonemapper::AgXPunchy)
        .with_exposure(0.5)
        .with_vignette(Vignette {
            enabled: true,
            intensity: 0.24,
            ..Vignette::default()
        })
}

/// Particle settings for `quality` on this app's device (`ParticleSettings::for_quality_on`:
/// Low's where the GPU backend is unavailable, including under
/// `DownlevelOverride::no_compute`).
pub fn particle_settings(resources: &Resources, quality: GraphicsQuality) -> ParticleSettings {
    let no_compute = resources
        .get::<space3d_render::FrameSettings>()
        .is_some_and(|f| f.downlevel.no_compute);
    match resources.get::<Renderer>() {
        Some(_) if no_compute => ParticleSettings::for_quality(GraphicsQuality::Low),
        Some(r) => ParticleSettings::for_quality_on(quality, r.graphics_caps()),
        None => ParticleSettings::for_quality(quality),
    }
}

/// The sun: `DirectionalLight` along [`sun_direction`] with a `DirectionalShadow` fitted to
/// the primary camera over `fit`.
pub fn spawn_sun(world: &mut World, fit: ShadowFit) -> Entity {
    world.spawn((
        Name("sun".into()),
        Transform::IDENTITY.looking_at(sun_direction(), Vec3::Y),
        DirectionalLight {
            color: Vec3::new(1.0, 0.94, 0.86),
            intensity: 2.2,
        },
        DirectionalShadow::new().with_fit(fit),
    ))
}

/// Specular image-based lighting × the sky: the top-down camera's reflections look up into
/// the dim zenith of the nebula (radiance about 0.05), so the lighting environment is authored
/// brighter than the backdrop, or metal would reflect next to nothing.
pub const ENV_SPECULAR: f32 = 2.5;

/// Diffuse image-based lighting × the sky: the fill of what the sun does not reach. The
/// nebula's irradiance is about 0.03 (the cosine-weighted mean radiance a side of a hull sees,
/// the sun left out: a dark sky), so at 1 a shadowed side gets about 1.4 % of what the sun's
/// 2.2 gives a lit top and reads as black. At 6 the nebula-tinted fill is about 0.18; with
/// [`spawn_fill`] and the ambient a side facing away from the sun gets about 15 % of a sunlit
/// top (20 % of a side facing the sun), a top in a cast shadow about 17 %: dark, but the hull's
/// panels and colors stay readable (docs/graphics/materials.md, "Fill light").
pub const ENV_DIFFUSE: f32 = 6.0;

/// The sky as `camera`'s `Skybox` and its image-based lighting (`EnvironmentLight`: diffuse
/// [`ENV_DIFFUSE`], specular [`ENV_SPECULAR`]), and a faint blue ambient for the shadowed sides
/// (0.03: what reaches them before the environment is baked, and with the environment off);
/// the diffuse lighting and the ambient lifted by [`fill_lift`] at `quality`.
pub fn add_environment(
    world: &mut World,
    resources: &mut Resources,
    assets: &ShowcaseAssets,
    camera: Entity,
    quality: GraphicsQuality,
) {
    let lift = fill_lift(quality);
    let _ = world.insert(
        camera,
        (
            Skybox::new(assets.sky),
            EnvironmentLight::new(assets.sky).with_intensity(ENV_DIFFUSE * lift, ENV_SPECULAR),
        ),
    );
    resources.insert(AmbientLight {
        color: Vec3::new(0.45, 0.55, 1.0),
        intensity: 0.03 * lift,
    });
}

/// How much the fill ([`add_environment`]'s diffuse lighting and ambient, [`spawn_fill`]) is
/// raised at `quality`: 1 at High and Ultra, 1.3 at Medium, 1.8 at Low. The lower tiers lose
/// what lifts High's shadows on screen — bloom's veil (Medium's is smaller, Low's smaller still,
/// none without float targets: WebGL2) and GGX shading (Low's Classic) — and, with the same
/// rig, a top in a cast shadow fell to 4–5 % of a sunlit one at Medium, 3–4 % at Low and 2–3 %
/// on WebGL2 against 6–7 % at High (M2 final review; "below 5 % they read as holes",
/// docs/graphics/materials.md).
pub fn fill_lift(quality: GraphicsQuality) -> f32 {
    match quality {
        GraphicsQuality::Low => 1.8,
        GraphicsQuality::Medium => 1.3,
        _ => 1.0,
    }
}

/// Direction the fill light travels: up from the nebula below the flight plane and away from
/// the sun, so it reaches the undersides and the sides the sun leaves in shadow.
pub fn fill_direction() -> Vec3 {
    let sun = sun_direction();
    Vec3::new(-sun.x, 0.55, -sun.z).normalize()
}

/// The fill: a shadowless lavender `DirectionalLight` (intensity 0.35, 16 % of the sun, ×
/// [`fill_lift`] at `quality`) along [`fill_direction`] — the nebula's glow bouncing up onto
/// the hulls. Together with the sky's diffuse lighting ([`ENV_DIFFUSE`]) it keeps the shadowed
/// side of a hull readable from the top-down camera. Its hue is muted (the diffuse lighting
/// carries the nebula's color): from the top-down camera the fill reaches few visible faces
/// but crease walls at grazing angles, which a saturated violet (0.62, 0.5, 1.0 at the M2 final
/// review) outlined in thin violet slivers.
pub fn spawn_fill(world: &mut World, quality: GraphicsQuality) -> Entity {
    world.spawn((
        Name("fill".into()),
        Transform::IDENTITY.looking_at(fill_direction(), Vec3::Y),
        DirectionalLight {
            color: Vec3::new(0.59, 0.53, 0.82),
            intensity: 0.35 * fill_lift(quality),
        },
    ))
}

/// A ship of hull `kind` and `faction` at `t` (the root entity carries the mesh; effects
/// follow it).
pub fn spawn_ship(world: &mut World, assets: &ShowcaseAssets, kind: usize, faction: usize, t: Transform) -> Entity {
    world.spawn((
        Name(format!(
            "{} {}",
            FACTIONS[faction % 3].name,
            super::content::HULL_NAMES[kind % 3]
        )),
        t,
        Mesh3d(assets.hulls[kind % 3]),
        assets.hull_materials[faction % 3].clone(),
    ))
}

/// Engine exhausts on `ship`'s nozzles (x ∈ `nozzles`, z = +21), with afterburners on the
/// outer ones when `boost`.
pub fn spawn_engines(app: &mut App, ship: Entity, nozzles: &[f32], tint: Vec4, boost: bool, seed: u32) {
    for (i, &x) in nozzles.iter().enumerate() {
        let at = Transform::from_xyz(x, 0.0, 21.0);
        app.spawn_effect(
            builtin::ENGINE_EXHAUST,
            EffectSpawn::from_transform(at)
                .follow(ship)
                .tint(tint)
                .seed(seed + i as u32)
                .keep(),
        );
        if boost && x != 0.0 {
            app.spawn_effect(
                builtin::ENGINE_AFTERBURNER,
                EffectSpawn::from_transform(at)
                    .follow(ship)
                    .tint(tint)
                    .seed(seed + 10 + i as u32)
                    .keep(),
            );
        }
    }
}

/// Asteroids below the flight plane: `count` rocks scattered over a disc of `radius`,
/// `depth` units below it, sizes `size.0..size.1` (shadow receivers, image-based lighting).
pub fn spawn_asteroids(
    world: &mut World,
    assets: &ShowcaseAssets,
    count: u32,
    radius: f32,
    depth: f32,
    size: (f32, f32),
    seed: u32,
) {
    for i in 0..count {
        let r = |k: i32| unit(hash(i as i32, k, seed as i32, 404));
        let a = r(1) * std::f32::consts::TAU;
        let d = radius * r(2).sqrt();
        let s = size.0 + (size.1 - size.0) * r(3).powi(2);
        world.spawn((
            Transform::from_xyz(a.cos() * d, -depth - r(4) * depth * 0.6 - s * 0.5, a.sin() * d)
                .with_rotation(Quat::from_euler(EulerRot::YXZ, r(5) * 6.3, r(6) * 6.3, r(7) * 6.3))
                .with_scale(Vec3::new(s, s * (0.7 + 0.3 * r(8)), s * (0.8 + 0.4 * r(9)))),
            Mesh3d(assets.asteroids[i as usize % 3]),
            assets.asteroid_material.clone(),
        ));
    }
}

/// Knobs of [`spawn_look`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LookScene {
    /// Quality tier applied to everything. Default High.
    pub quality: GraphicsQuality,
    /// The sun casts shadows. Default true.
    pub shadows: bool,
    /// Sky image-based lighting. Default true.
    pub environment: bool,
    /// Bloom. Default true.
    pub bloom: bool,
    /// Distortion (shockwaves, heat haze). Default true.
    pub distortion: bool,
    /// Ticks the scene is designed to be seen at (its timed effects land then). Default 96.
    pub ticks: u64,
}

impl Default for LookScene {
    fn default() -> Self {
        Self {
            quality: GraphicsQuality::High,
            shadows: true,
            environment: true,
            bloom: true,
            distortion: true,
            ticks: 96,
        }
    }
}

/// A moving ship of the look scene: where it is at `look.ticks`, its heading and speed.
struct Mover {
    entity: Entity,
    at: Vec3,
    heading: Vec3,
    speed: f32,
}

fn heading_rotation(dir: Vec3) -> Quat {
    Quat::from_rotation_arc(Vec3::NEG_Z, dir.normalize())
}

/// The SpaceCorps look: a close-up skirmish seen from the game's top-down camera — a frigate
/// and two interceptors (blue) holding a laser beam on an orange gunship (hit sparks, shield
/// ripples), orange laser bolts flying back, a missile with its smoke trail, a ship that just
/// exploded (fireball, debris, shockwave, light flash), a green ship warping in, asteroids
/// catching the ships' shadows, the nebula sky and its image-based lighting, the sun, and the
/// SpaceCorps look ([`battle_look`]). `ParticlesPlugin` must be added (with
/// [`particle_settings`]). Returns the camera.
pub fn spawn_look(app: &mut App, assets: &ShowcaseAssets, look: &LookScene) -> Entity {
    let t_end = look.ticks as f32;
    let pitch = 62f32.to_radians();
    let target = Vec3::new(0.0, 0.0, -4.0);
    let eye = target + Vec3::new(0.0, pitch.sin(), pitch.cos()) * 250.0;
    let mut post = battle_look();
    post.bloom.enabled = look.bloom;
    post.distortion.enabled = look.distortion;
    let camera = app.world.spawn((
        Name("camera".into()),
        Transform::from_xyz(eye.x, eye.y, eye.z).looking_at(target, Vec3::Y),
        Camera3d::default()
            .with_clear_color(SPACE)
            .with_projection(Projection::Perspective {
                fov_y: 40f32.to_radians(),
                near: 1.0,
                far: 3000.0,
            }),
        post,
    ));
    add_environment(&mut app.world, &mut app.resources, assets, camera, look.quality);
    if !look.environment {
        let _ = app.world.remove_one::<EnvironmentLight>(camera);
    }
    let sun = spawn_sun(&mut app.world, ShadowFit::top_down(-20.0, 60.0, 400.0));
    spawn_fill(&mut app.world, look.quality);
    if !look.shadows {
        let _ = app.world.remove_one::<DirectionalShadow>(sun);
    }
    spawn_asteroids(&mut app.world, assets, 9, 120.0, 40.0, (5.0, 14.0), 3);
    // One big rock right under the fleet: it catches the frigate's and a wingman's shadows.
    app.world.spawn((
        Transform::from_xyz(-10.0, -44.0, 8.0)
            .with_rotation(Quat::from_euler(EulerRot::YXZ, 0.7, 0.3, 0.1))
            .with_scale(Vec3::new(34.0, 20.0, 30.0)),
        Mesh3d(assets.asteroids[1]),
        assets.asteroid_material.clone(),
    ));

    let blue = FACTIONS[0];
    let orange = FACTIONS[1];
    let fleet_heading = Vec3::new(-0.3, 0.0, -1.0);
    let enemy_heading = Vec3::new(0.35, 0.0, 1.0);
    let mut movers = Vec::new();
    let mut ship = |app: &mut App, kind: usize, faction: usize, at: Vec3, heading: Vec3, speed: f32, scale: f32| {
        let start = at - heading.normalize() * speed * t_end / 60.0;
        let e = spawn_ship(
            &mut app.world,
            assets,
            kind,
            faction,
            Transform::from_xyz(start.x, start.y, start.z)
                .with_rotation(heading_rotation(heading))
                .with_scale(Vec3::splat(scale)),
        );
        movers.push(Mover {
            entity: e,
            at,
            heading: heading.normalize(),
            speed,
        });
        e
    };
    let frigate = ship(app, 1, 0, Vec3::new(8.0, 0.0, 16.0), fleet_heading, 18.0, 1.3);
    let wing_a = ship(app, 0, 0, Vec3::new(-44.0, 4.0, 30.0), fleet_heading, 18.0, 1.0);
    let wing_b = ship(app, 0, 0, Vec3::new(52.0, -3.0, 36.0), fleet_heading, 18.0, 1.0);
    let gunship = ship(app, 2, 1, Vec3::new(-34.0, 5.0, -40.0), enemy_heading, 10.0, 1.3);
    let raider = ship(
        app,
        0,
        1,
        Vec3::new(40.0, 2.0, -54.0),
        Vec3::new(-0.8, 0.0, 0.6),
        22.0,
        1.0,
    );
    spawn_engines(app, frigate, &[-8.0, 0.0, 8.0], blue.engine, true, 1);
    spawn_engines(app, wing_a, &[-8.0, 8.0], blue.engine, false, 10);
    spawn_engines(app, wing_b, &[-8.0, 8.0], blue.engine, false, 20);
    spawn_engines(app, gunship, &[-8.0, 0.0, 8.0], orange.engine, false, 30);
    spawn_engines(app, raider, &[-8.0, 8.0], orange.engine, true, 40);
    // The frigate's beam on the gunship.
    app.spawn_effect(
        builtin::LASER_BEAM,
        EffectSpawn::from_transform(Transform::from_xyz(0.0, 1.0, -26.0))
            .follow(frigate)
            .beam_to(BeamEnd::Entity(gunship))
            .tint(blue.laser)
            .seed(50)
            .keep(),
    );
    // A repair beam from a wingman to the frigate.
    app.spawn_effect(
        builtin::REPAIR_BEAM,
        EffectSpawn::from_transform(Transform::from_xyz(0.0, 1.0, -20.0))
            .follow(wing_b)
            .beam_to(BeamEnd::Entity(frigate))
            .seed(51)
            .keep(),
    );
    let movers_at_end: Vec<(Entity, Vec3, Vec3, f32)> =
        movers.iter().map(|m| (m.entity, m.at, m.heading, m.speed)).collect();
    let ticks = look.ticks;
    let mut flying: Vec<(Entity, Vec3, u64)> = Vec::new();
    app.add_system(Stage::FixedUpdate, move |ctx: &mut SystemCtx| {
        let tick = ctx.time().tick;
        let dt = ctx.time().fixed_delta_secs();
        // Ships fly straight: at `at` on the designed tick.
        for &(e, at, heading, speed) in &movers_at_end {
            let p = at - heading * speed * (ticks as f32 - tick as f32) / 60.0;
            if let Ok(mut t) = ctx.world.get::<&mut Transform>(e) {
                t.translation = p;
            }
        }
        let pos = |ctx: &SystemCtx, e: Entity| {
            ctx.world
                .get::<&Transform>(e)
                .map(|t| t.translation)
                .unwrap_or_default()
        };
        // Hits where the beam lands, and shield ripples.
        if tick % 7 == 3 {
            let (g, f) = (pos(ctx, gunship), pos(ctx, frigate));
            let from = (f - g).normalize_or_zero();
            ctx.spawn_effect(
                builtin::HIT_SPARK,
                EffectSpawn::at(g + from * 14.0 + Vec3::Y * 2.0)
                    .facing(from)
                    .tint(blue.laser)
                    .seed(100 + tick as u32),
            );
            if tick % 21 == 3 {
                ctx.spawn_effect(
                    builtin::SHIELD_RIPPLE,
                    EffectSpawn::at(Vec3::ZERO)
                        .facing(from)
                        .scale(1.4)
                        .follow(gunship)
                        .seed(200 + tick as u32),
                );
            }
        }
        // Orange bolts from the gunship's turrets at the lead wingman.
        if tick.is_multiple_of(9) {
            let (g, w) = (pos(ctx, gunship), pos(ctx, wing_a));
            let dir = (w - g).normalize_or_zero();
            let at = g + dir * 18.0 + Vec3::Y * 3.0;
            let bolt = ctx.world.spawn((Transform::from_xyz(at.x, at.y, at.z),));
            ctx.spawn_effect(
                builtin::LASER_TRAIL,
                EffectSpawn::at(Vec3::ZERO)
                    .follow(bolt)
                    .tint(orange.laser)
                    .seed(300 + tick as u32),
            );
            flying.push((bolt, dir * 260.0, tick + 16));
        }
        // A missile from the second wingman at the raider.
        if tick == ticks.saturating_sub(70) {
            let at = pos(ctx, wing_b) + Vec3::new(-6.0, 1.0, -18.0);
            let missile = ctx.world.spawn((Transform::from_xyz(at.x, at.y, at.z),));
            ctx.spawn_effect(
                builtin::MISSILE_TRAIL,
                EffectSpawn::at(Vec3::ZERO).follow(missile).seed(400),
            );
            flying.push((missile, Vec3::new(-0.25, 0.0, -1.0).normalize() * 70.0, u64::MAX));
        }
        let mut k = 0;
        while k < flying.len() {
            let (e, v, end) = flying[k];
            if tick >= end {
                let _ = ctx.world.despawn(e);
                flying.swap_remove(k);
                continue;
            }
            if let Ok(mut t) = ctx.world.get::<&mut Transform>(e) {
                t.translation += v * dt;
            }
            k += 1;
        }
        // A ship that just blew up, right of the fleet.
        if tick == ticks.saturating_sub(22) {
            let at = Vec3::new(66.0, 0.0, -18.0);
            ctx.spawn_effect(builtin::EXPLOSION, EffectSpawn::at(at).scale(0.85).seed(500));
            ctx.spawn_effect(builtin::DEBRIS_SPARKS, EffectSpawn::at(at).facing(Vec3::Y).seed(501));
        }
        // A green ship warping in on the left.
        if tick == ticks.saturating_sub(40) {
            ctx.spawn_effect(
                builtin::WARP_IN,
                EffectSpawn::at(Vec3::new(-96.0, 0.0, 6.0)).scale(0.45).seed(600),
            );
        }
    });
    look.quality.apply(&mut app.world, &mut app.resources);
    camera
}

/// Knobs of the Battle reference scene ([`spawn_battle`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BattleScene {
    /// Quality tier. Default High (the Dev gates' tier).
    pub quality: GraphicsQuality,
    /// Ships. Default 300.
    pub ships: u32,
    /// Radius of the disc they fly in. Default 1200.
    pub radius: f32,
    /// Asteroids below the flight plane. Default 60.
    pub asteroids: u32,
    /// Two 256² render-target ship previews (as in the game's UI; no post). Default true.
    pub previews: bool,
    /// A HUD camera showing the previews in the lower right corner (the game shows them in
    /// egui; not part of the reference scene's budget). Default true.
    pub hud: bool,
    /// Seed of the placements.
    pub seed: u32,
}

impl Default for BattleScene {
    fn default() -> Self {
        Self {
            quality: GraphicsQuality::High,
            ships: 300,
            radius: 1200.0,
            asteroids: 60,
            previews: true,
            hud: true,
            seed: 1,
        }
    }
}

/// What [`spawn_battle`] created.
#[derive(Clone, Debug, Default)]
pub struct BattleHandles {
    /// The battle camera (the game's view: 432 units away at a 70° pitch).
    pub camera: Option<Entity>,
    /// The sun.
    pub sun: Option<Entity>,
    /// The two ship-preview cameras (render targets) and the HUD camera showing them.
    pub previews: Vec<Entity>,
    /// Whether [`dress_battle`] ran.
    pub dressed: bool,
}

/// Where the previews' ships float (far from the battle and outside its cameras' ranges).
const HANGAR: Vec3 = Vec3::new(0.0, -20_000.0, 0.0);
/// Where the HUD camera looks.
const HUD: Vec3 = Vec3::new(0.0, -40_000.0, 0.0);

/// The Battle reference scene's simulation (graphics spec §4.4): its camera with the battle
/// look, the sun with its shadow, and the particle load of `space3d_particles::testing::battle`
/// (300 ships flying circles with three engines each, bolts, missiles, held lasers and repair
/// beams, 30 explosions a second with their light flashes, hits, shield ripples, debris,
/// warps and EMPs). The ships have no meshes yet: [`dress_battle`] adds the content (it can
/// arrive later, from a task). `ParticlesPlugin` must be added.
pub fn spawn_battle(app: &mut App, scene: &BattleScene) -> BattleHandles {
    let pitch = 70f32.to_radians();
    let camera = app.world.spawn((
        Name("battle camera".into()),
        Transform::from_xyz(0.0, 432.0 * pitch.sin(), 432.0 * pitch.cos()).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default()
            .with_clear_color(SPACE)
            .with_projection(Projection::Perspective {
                fov_y: 60f32.to_radians(),
                near: 2.0,
                far: 4000.0,
            }),
        battle_look(),
    ));
    let sun = spawn_sun(&mut app.world, ShadowFit::top_down(-30.0, 90.0, 900.0));
    spawn_fill(&mut app.world, scene.quality);
    space3d_particles::testing::battle(
        app,
        Battle {
            ships: scene.ships,
            radius: scene.radius,
            seed: scene.seed,
            ..Battle::default()
        },
    );
    scene.quality.apply(&mut app.world, &mut app.resources);
    BattleHandles {
        camera: Some(camera),
        sun: Some(sun),
        previews: Vec::new(),
        dressed: false,
    }
}

/// Add the content to a [`spawn_battle`] scene: a hull (kind by ship, faction by its engines'
/// tint) on every ship, asteroids below the flight plane, the sky and its image-based lighting
/// on the camera, and the two render-target previews with their HUD camera. Applies the
/// scene's quality again (the preview cameras and their post).
pub fn dress_battle(
    world: &mut World,
    resources: &mut Resources,
    assets: &ShowcaseAssets,
    scene: &BattleScene,
    handles: &mut BattleHandles,
) {
    if handles.dressed {
        return;
    }
    handles.dressed = true;
    let ships = resources
        .get::<BattleLog>()
        .map(|l| l.ships.clone())
        .unwrap_or_default();
    for (k, &ship) in ships.iter().enumerate() {
        // `testing::battle` tints engines by `k % 3`: the faction; the hull kind varies.
        let kind = (hash(k as i32, 0, 0, 9) % 3) as usize;
        let _ = world.insert(ship, (Mesh3d(assets.hulls[kind]), assets.hull_materials[k % 3].clone()));
    }
    spawn_asteroids(
        world,
        assets,
        scene.asteroids,
        scene.radius,
        60.0,
        (8.0, 30.0),
        scene.seed,
    );
    if let Some(camera) = handles.camera {
        add_environment(world, resources, assets, camera, scene.quality);
    }
    if scene.previews {
        handles.previews = spawn_previews(world, resources, assets, scene.hud);
    }
    scene.quality.apply(world, resources);
}

/// Rotates the preview ships.
struct PreviewShip;

/// Two 256² render-target ship previews (the game's hangar / target views: a transparent
/// clear, 4× MSAA, image-based lighting, no post — one render pass each, as in the reference
/// scene) of a frigate and a gunship turning slowly, far from the battle, and with `hud` a HUD
/// camera (order 10: a display view drawn after the battle camera's post) showing them in the
/// lower right corner as alpha-blended quads. Returns the cameras.
pub fn spawn_previews(world: &mut World, resources: &mut Resources, assets: &ShowcaseAssets, hud: bool) -> Vec<Entity> {
    let mut cameras = Vec::new();
    let mut quads = Vec::new();
    for (i, (kind, faction)) in [(1usize, 0usize), (2, 1)].into_iter().enumerate() {
        let rt = resources.init::<RenderAssets>().add_render_target_with(
            256,
            256,
            RenderTargetDesc::default().with_samples(4),
        );
        let at = HANGAR + Vec3::X * (i as f32 * 400.0);
        let ship = spawn_ship(world, assets, kind, faction, Transform::from_xyz(at.x, at.y, at.z));
        let _ = world.insert_one(ship, PreviewShip);
        let eye = at + Vec3::new(0.0, 38.0, 52.0);
        cameras.push(
            world.spawn((
                Name(format!("preview {i}")),
                Transform::from_xyz(eye.x, eye.y, eye.z).looking_at(at, Vec3::Y),
                Camera3d::default()
                    .with_clear_color(Vec4::ZERO)
                    .with_target(RenderTarget::Texture(rt))
                    .with_projection(Projection::Perspective {
                        fov_y: 40f32.to_radians(),
                        near: 1.0,
                        far: 300.0,
                    }),
                EnvironmentLight::new(assets.sky).with_intensity(ENV_DIFFUSE, ENV_SPECULAR),
            )),
        );
        quads.push(rt);
    }
    if !hud {
        return cameras;
    }
    // The HUD: an orthographic camera 2 units tall over quads in its XY plane.
    let hud = world.spawn((
        Name("hud camera".into()),
        Transform::from_xyz(HUD.x, HUD.y, HUD.z + 10.0),
        Camera3d {
            clear_color: None,
            order: 10,
            ..Camera3d::default().with_projection(Projection::Orthographic {
                height: 2.0,
                near: 0.1,
                far: 20.0,
            })
        },
    ));
    // Each preview sits on a dark translucent panel with a thin frame, so it reads as a HUD
    // inset rather than a giant ship in the fight (the render targets clear to transparent).
    let face = Quat::from_rotation_x(std::f32::consts::FRAC_PI_2);
    let quad = |world: &mut World, name: String, at: Vec3, size: Vec2, material: Material| {
        world.spawn((
            Name(name),
            Transform::from_xyz(at.x, at.y, at.z)
                .with_rotation(face)
                .with_scale(Vec3::new(size.x, 1.0, size.y)),
            Mesh3d(assets.quad),
            material,
        ));
    };
    let (panel, frame, line) = (0.53, 0.536, 0.004);
    for (i, rt) in quads.into_iter().enumerate() {
        let at = Vec3::new(HUD.x + 1.42 - 0.56 * i as f32, HUD.y - 0.72, HUD.z);
        let dark = Material::unlit(Vec4::new(0.004, 0.006, 0.012, 0.62)).with_blend(BlendMode::AlphaBlend);
        quad(
            world,
            format!("preview panel {i}"),
            at - Vec3::Z * 0.02,
            Vec2::splat(panel),
            dark,
        );
        let edge = Material::unlit(Vec4::new(0.25, 0.42, 0.6, 0.85)).with_blend(BlendMode::AlphaBlend);
        let o = (frame - line) * 0.5;
        for (k, (offset, size)) in [
            (Vec3::Y * o, Vec2::new(frame, line)),
            (-Vec3::Y * o, Vec2::new(frame, line)),
            (Vec3::X * o, Vec2::new(line, frame)),
            (-Vec3::X * o, Vec2::new(line, frame)),
        ]
        .into_iter()
        .enumerate()
        {
            quad(
                world,
                format!("preview frame {i}.{k}"),
                at + offset - Vec3::Z * 0.01,
                size,
                edge.clone(),
            );
        }
        quad(
            world,
            format!("preview quad {i}"),
            at,
            Vec2::splat(0.5),
            Material::unlit(Vec4::ONE)
                .with_texture(rt)
                .with_blend(BlendMode::AlphaBlend),
        );
    }
    cameras.push(hud);
    cameras
}

/// `Stage::FixedUpdate` system turning the preview ships (add it once with the previews).
pub fn turn_previews(ctx: &mut SystemCtx) {
    let a = ctx.time().tick as f32 * 0.01;
    for (t, _) in ctx.world.query_mut::<(&mut Transform, &PreviewShip)>() {
        t.rotation = Quat::from_rotation_y(a) * Quat::from_rotation_x(0.15);
    }
}
