//! Physics playground: rapier rigid bodies stepped on the deterministic fixed tick.
//!
//! ```text
//! cargo run -p physics_playground --release
//! cargo run -p physics_playground --release -- --rain          # bodies keep falling
//! SPACE3D_EXIT_AFTER_FRAMES=300 cargo run -p physics_playground -- --rain
//! ```
//! Controls: **left click** drops a body where the cursor ray hits the scene (a physics
//! raycast), **Space** drops one at a random spot, **R** toggles rain, **A/D** or ←/→ orbit
//! the camera, mouse wheel zooms, **Backspace** clears, **Esc** quits.

use space3d::prelude::*;

/// Maximum simultaneously simulated bodies (older ones are removed first).
const MAX_BODIES: usize = 1500;

/// Marks bodies spawned by the player (so they can be cleared / culled).
struct Dropped {
    order: u64,
}

/// Shared meshes.
struct Meshes {
    cube: MeshHandle,
    ball: MeshHandle,
}

/// Game state.
struct Playground {
    rng: u64,
    rain: bool,
    spawned: u64,
    contacts: u64,
    orbit_angle: f32,
    orbit_distance: f32,
}

impl Playground {
    /// xorshift64*: deterministic, dependency-free.
    fn rand(&mut self) -> f32 {
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 24) as f32
    }
}

fn build(app: &mut App) {
    let rain = std::env::args().any(|a| a == "--rain");
    app.add_plugin(
        DefaultPlugins::new()
            .title("Space3d — physics playground")
            .without_net(),
    );
    add_game(app, rain);
}

/// The playground and its systems, without the engine plugins (the tests add them to a
/// headless app).
fn add_game(app: &mut App, rain: bool) {
    app.insert_resource(Playground {
        rng: 0x9E37_79B9_7F4A_7C15,
        rain,
        spawned: 0,
        contacts: 0,
        orbit_angle: 0.6,
        orbit_distance: 26.0,
    });
    app.add_system(Stage::Startup, setup);
    app.add_system(Stage::Update, controls);
    app.add_system(Stage::Update, count_contacts);
    app.add_system(Stage::FixedUpdate, rain_system);
    app.add_system(Stage::Update, cull_bodies);
    app.add_system(Stage::Update, camera_system);
    app.add_system(Stage::Last, title);
}

fn setup(ctx: &mut SystemCtx) {
    let (cube, ball) = {
        let assets = ctx.resources.expect_mut::<RenderAssets>();
        (
            assets.add_mesh(MeshData::cube(1.0)),
            assets.add_mesh(MeshData::sphere(0.5, 24, 16)),
        )
    };
    ctx.resources.insert(Meshes { cube, ball });

    // Table + ramps: fixed bodies. The transform scale is baked into the collider.
    let slab = Material::srgb(0.3, 0.33, 0.37).with_metallic_roughness(0.0, 0.9);
    let statics = [
        (Vec3::new(0.0, -0.5, 0.0), Vec3::new(24.0, 1.0, 24.0), Quat::IDENTITY),
        (
            Vec3::new(-6.0, 2.0, 0.0),
            Vec3::new(8.0, 0.4, 5.0),
            Quat::from_rotation_z(-0.35),
        ),
        (
            Vec3::new(6.0, 1.2, -5.0),
            Vec3::new(6.0, 0.4, 4.0),
            Quat::from_rotation_x(0.3),
        ),
    ];
    for (pos, size, rot) in statics {
        ctx.world.spawn((
            Transform::from_xyz(pos.x, pos.y, pos.z)
                .with_rotation(rot)
                .with_scale(size),
            Mesh3d(cube),
            slab.clone(),
            RigidBody::Fixed,
            Collider::cuboid(Vec3::ONE),
        ));
    }

    // A small pyramid of boxes and a ball that knocks it over.
    for layer in 0..5 {
        for i in 0..(5 - layer) {
            for j in 0..(5 - layer) {
                let x = i as f32 - (4 - layer) as f32 * 0.5 + 4.0;
                let z = j as f32 - (4 - layer) as f32 * 0.5 + 4.0;
                ctx.world.spawn((
                    Transform::from_xyz(x * 1.02, 0.5 + layer as f32, z * 1.02),
                    Mesh3d(cube),
                    Material::srgb(0.95, 0.75 - layer as f32 * 0.1, 0.3),
                    RigidBody::Dynamic,
                    Collider::cuboid(Vec3::ONE),
                ));
            }
        }
    }
    ctx.world.spawn((
        Transform::from_xyz(-6.0, 8.0, 0.0),
        Mesh3d(ball),
        Material::srgb(0.2, 0.6, 0.95).with_metallic_roughness(0.8, 0.2),
        RigidBody::Dynamic,
        Collider::ball(0.5),
        Restitution(0.4),
    ));

    ctx.world.spawn((
        Transform::default().looking_at(Vec3::new(-0.4, -1.0, -0.2), Vec3::Y),
        DirectionalLight {
            color: Vec3::new(1.0, 0.97, 0.9),
            intensity: 2.4,
        },
    ));
    ctx.resources.insert(AmbientLight {
        color: Vec3::new(0.65, 0.75, 1.0),
        intensity: 0.18,
    });
    ctx.world.spawn((
        Transform::from_xyz(0.0, 14.0, 26.0).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.05, 0.06, 0.09, 1.0)),
    ));
}

/// Spawn one dynamic body at `pos`.
fn drop_body(ctx: &mut SystemCtx, pos: Vec3) {
    let (cube, ball) = {
        let m = ctx.resources.expect::<Meshes>();
        (m.cube, m.ball)
    };
    let pg = ctx.resources.expect_mut::<Playground>();
    let (kind, r, g, b, s) = (pg.rand(), pg.rand(), pg.rand(), pg.rand(), pg.rand());
    pg.spawned += 1;
    let order = pg.spawned;
    let color = Material::srgb(0.3 + 0.7 * r, 0.3 + 0.7 * g, 0.3 + 0.7 * b).with_metallic_roughness(0.1, 0.4);
    let scale = 0.6 + s * 0.8;
    let transform = Transform::from_xyz(pos.x, pos.y, pos.z)
        .with_rotation(Quat::from_euler(EulerRot::XYZ, r * 3.0, g * 3.0, b * 3.0))
        .with_scale(Vec3::splat(scale));
    if kind < 0.5 {
        ctx.world.spawn((
            transform,
            Mesh3d(cube),
            color,
            RigidBody::Dynamic,
            Collider::cuboid(Vec3::ONE),
            Dropped { order },
        ));
    } else {
        ctx.world.spawn((
            transform,
            Mesh3d(ball),
            color,
            RigidBody::Dynamic,
            Collider::ball(0.5),
            Restitution(0.5),
            Dropped { order },
        ));
    }
}

/// World-space ray through a cursor position (physical pixels) for the first camera.
fn cursor_ray(ctx: &SystemCtx, cursor: Vec2, window: Vec2) -> Option<(Vec3, Vec3)> {
    let mut query = ctx.world.query::<(&Transform, &Camera3d)>();
    let (transform, camera) = query.iter().next()?;
    let window = window.max(Vec2::ONE);
    let ndc = Vec2::new(2.0 * cursor.x / window.x - 1.0, 1.0 - 2.0 * cursor.y / window.y);
    // (`unproject_ray` inverts the projection and the view in f64: the f32 inverse of
    // `proj * view` is off by units far from the origin.)
    Some(space3d::render::unproject_ray(
        transform.matrix().inverse(),
        camera.projection.matrix(window.x / window.y),
        ndc,
    ))
}

fn controls(ctx: &mut SystemCtx) {
    let input = ctx.resources.expect::<Input>();
    let quit = input.just_pressed(KeyCode::Escape);
    let random_drop = input.just_pressed(KeyCode::Space);
    let toggle_rain = input.just_pressed(KeyCode::KeyR);
    let clear = input.just_pressed(KeyCode::Backspace);
    let click = input
        .mouse_just_pressed(MouseButton::Left)
        .then(|| input.mouse_position())
        .flatten();
    let window = input.window_size();
    let orbit = input.any_pressed([KeyCode::KeyD, KeyCode::ArrowRight]) as i32 as f32
        - input.any_pressed([KeyCode::KeyA, KeyCode::ArrowLeft]) as i32 as f32;
    let zoom = input.wheel_delta().y;

    if quit {
        ctx.send(AppExit);
    }
    let dt = ctx.time().delta_secs();
    {
        let pg = ctx.resources.expect_mut::<Playground>();
        pg.orbit_angle += orbit * dt * 1.5;
        pg.orbit_distance = (pg.orbit_distance - zoom * 1.5).clamp(8.0, 60.0);
        if toggle_rain {
            pg.rain = !pg.rain;
        }
    }
    if random_drop {
        let pg = ctx.resources.expect_mut::<Playground>();
        let pos = Vec3::new(pg.rand() * 16.0 - 8.0, 12.0, pg.rand() * 16.0 - 8.0);
        drop_body(ctx, pos);
    }
    if let Some(cursor) = click
        && let Some((origin, dir)) = cursor_ray(ctx, cursor, window)
    {
        let target = ctx
            .resources
            .expect::<Physics>()
            .raycast(origin, dir, 500.0)
            .map_or(origin + dir * 20.0, |hit| hit.point);
        drop_body(ctx, target + Vec3::Y * 8.0);
    }
    if clear {
        let dropped: Vec<Entity> = ctx.world.query::<(Entity, &Dropped)>().iter().map(|(e, _)| e).collect();
        for e in dropped {
            let _ = ctx.world.despawn(e);
        }
    }
}

/// While raining, drop a body every 6th fixed tick (10 per second at 60 Hz).
fn rain_system(ctx: &mut SystemCtx) {
    let tick = ctx.time().tick;
    let pg = ctx.resources.expect_mut::<Playground>();
    if pg.rain && tick.is_multiple_of(6) {
        let pos = Vec3::new(pg.rand() * 18.0 - 9.0, 14.0, pg.rand() * 18.0 - 9.0);
        drop_body(ctx, pos);
    }
}

fn count_contacts(ctx: &mut SystemCtx) {
    let started = ctx
        .resources
        .get::<Events<CollisionEvent>>()
        .map_or(0, |ev| ev.iter().filter(|e| e.is_started()).count());
    ctx.resources.expect_mut::<Playground>().contacts += started as u64;
}

/// Remove bodies that fell off the table, and the oldest ones above [`MAX_BODIES`].
fn cull_bodies(ctx: &mut SystemCtx) {
    let mut dropped: Vec<(Entity, u64, f32)> = ctx
        .world
        .query::<(Entity, &Dropped, &Transform)>()
        .iter()
        .map(|(e, d, t)| (e, d.order, t.translation.y))
        .collect();
    let mut remove: Vec<Entity> = dropped.iter().filter(|(_, _, y)| *y < -30.0).map(|d| d.0).collect();
    if dropped.len() > MAX_BODIES {
        dropped.sort_by_key(|d| d.1);
        remove.extend(dropped[..dropped.len() - MAX_BODIES].iter().map(|d| d.0));
    }
    for e in remove {
        let _ = ctx.world.despawn(e);
    }
}

fn camera_system(ctx: &mut SystemCtx) {
    let (angle, dist) = {
        let pg = ctx.resources.expect::<Playground>();
        (pg.orbit_angle, pg.orbit_distance)
    };
    for (t, _) in ctx.world.query_mut::<(&mut Transform, &Camera3d)>() {
        let eye = Vec3::new(angle.sin() * dist, dist * 0.55, angle.cos() * dist);
        *t = Transform::from_xyz(eye.x, eye.y, eye.z).looking_at(Vec3::new(0.0, 1.0, 0.0), Vec3::Y);
    }
}

fn title(ctx: &mut SystemCtx) {
    let frame = ctx.time().frame;
    if !frame.is_multiple_of(20) {
        return;
    }
    let bodies = ctx.resources.expect::<Physics>().body_count();
    let (contacts, rain) = {
        let pg = ctx.resources.expect::<Playground>();
        (pg.contacts, pg.rain)
    };
    let text = format!(
        "Space3d — physics playground | {bodies} bodies | {contacts} contacts | rain {}",
        if rain { "on" } else { "off" }
    );
    if frame.is_multiple_of(120) {
        println!("frame {frame}: {bodies} bodies, {contacts} contacts");
    }
    if let Some(window) = ctx.resources.get_mut::<WindowState>() {
        window.title = text;
    }
}

space3d::main!(build);

#[cfg(test)]
mod tests {
    use super::*;

    fn headless(rain: bool) -> App {
        let mut app = App::new();
        app.add_plugin(DefaultPlugins::headless(64, 64).without_net());
        add_game(&mut app, rain);
        app
    }

    fn dropped(app: &App) -> Vec<(u64, Vec3)> {
        let mut query = app.world.query::<(&Dropped, &Transform)>();
        query.iter().map(|(d, t)| (d.order, t.translation)).collect()
    }

    fn tap(app: &mut App, key: KeyCode) {
        app.resources.expect_mut::<Input>().inject_key_tap(key);
        app.step_ticks(1);
    }

    #[test]
    fn space_drops_a_body_that_falls_and_backspace_clears() {
        let mut app = headless(false);
        app.step_ticks(2);
        assert!(dropped(&app).is_empty());

        tap(&mut app, KeyCode::Space);
        let spawned = dropped(&app);
        assert_eq!(spawned.len(), 1);
        let start_y = spawned[0].1.y;
        app.step_ticks(30);
        let now = dropped(&app);
        assert!(
            now[0].1.y < start_y - 1.0,
            "the body falls: {start_y} -> {}",
            now[0].1.y
        );
        assert!(app.resources.expect::<Physics>().body_count() > 50);

        tap(&mut app, KeyCode::Backspace);
        assert!(dropped(&app).is_empty());
    }

    /// Left click at `cursor` (pixels of a 64×64 window); returns the body it dropped.
    fn click(app: &mut App, cursor: Vec2) -> Vec3 {
        let before: Vec<u64> = dropped(app).iter().map(|d| d.0).collect();
        let input = app.resources.expect_mut::<Input>();
        input.set_window_size(Vec2::new(64.0, 64.0));
        input.inject_mouse_move(cursor);
        input.inject_mouse_button_down(MouseButton::Left);
        app.step_ticks(1);
        app.resources
            .expect_mut::<Input>()
            .inject_mouse_button_up(MouseButton::Left);
        let new: Vec<(u64, Vec3)> = dropped(app).into_iter().filter(|d| !before.contains(&d.0)).collect();
        assert_eq!(new.len(), 1, "one click, one body");
        new[0].1
    }

    #[test]
    fn click_drops_a_body_above_the_raycast_hit() {
        let mut app = headless(false);
        // Look at the table from the -x/+z side: the centre ray misses the pyramid and ramps.
        app.resources.expect_mut::<Playground>().orbit_angle = -0.9;
        app.step_ticks(2);
        let (eye, forward, right) = {
            let mut query = app.world.query::<(&Transform, &Camera3d)>();
            let (t, _) = query.iter().next().unwrap();
            (t.translation, t.rotation * Vec3::NEG_Z, t.rotation * Vec3::X)
        };

        // The screen centre looks straight down the camera axis; the body is spawned 8 units
        // above where that ray meets the table top (y = 0).
        let center = click(&mut app, Vec2::new(32.0, 32.0));
        let hit = eye + forward * (-eye.y / forward.y);
        assert!(
            (center - (hit + Vec3::Y * 8.0)).length() < 0.05,
            "dropped at {center}, expected above {hit}"
        );
        assert!(
            hit.x.abs() < 12.0 && hit.z.abs() < 12.0,
            "the ray hits the table: {hit}"
        );

        // A little up and right on screen: farther away and to the camera's right (NDC signs),
        // still on the open table (a ray that misses everything drops the body mid-air).
        let upper_right = click(&mut app, Vec2::new(36.0, 30.0));
        let flat = |v: Vec3| Vec3::new(v.x, 0.0, v.z);
        assert!(
            flat(upper_right - eye).length() > flat(center - eye).length(),
            "upper click lands farther: {upper_right} vs {center}"
        );
        assert!(
            (upper_right - center).dot(right) > 0.5,
            "right click lands right: {upper_right}"
        );
        assert!((upper_right.y - 8.0).abs() < 0.05, "on the table top: {upper_right}");
    }

    #[test]
    fn rain_drops_ten_bodies_per_second_deterministically() {
        let run = || {
            let mut app = headless(true);
            app.step_ticks(60);
            let mut bodies = dropped(&app);
            bodies.sort_by_key(|b| b.0);
            bodies
        };
        let first = run();
        assert_eq!(first.len(), 10);
        assert_eq!(first, run(), "same inputs, same simulation");
    }
}
