use space3d_core::prelude::*;
use space3d_physics::*;
use std::time::Duration;

fn app() -> App {
    let mut app = App::new();
    app.add_plugin(PhysicsPlugin::default());
    app
}

fn ground(app: &mut App) -> Entity {
    app.world.spawn((
        Name("ground".into()),
        Transform::from_xyz(0.0, -0.5, 0.0),
        RigidBody::Fixed,
        Collider::cuboid(Vec3::new(40.0, 1.0, 40.0)),
    ))
}

fn ball(app: &mut App, pos: Vec3) -> Entity {
    app.world.spawn((
        Transform::from_xyz(pos.x, pos.y, pos.z),
        RigidBody::Dynamic,
        Collider::ball(0.5),
        Velocity::default(),
    ))
}

fn pos(app: &App, e: Entity) -> Vec3 {
    app.world.get::<&Transform>(e).unwrap().translation
}

/// Records the ball's translation after every fixed tick.
#[derive(Default)]
struct Trace(Vec<Vec3>);

fn run_ball_sim(ticks: u64) -> (Vec3, Quat) {
    let mut app = app();
    ground(&mut app);
    let b = ball(&mut app, Vec3::new(0.0, 5.0, 0.0));
    app.step_ticks(ticks);
    let t = *app.world.get::<&Transform>(b).unwrap();
    (t.translation, t.rotation)
}

#[test]
fn ball_falls_and_rests_on_ground() {
    let mut app = app();
    ground(&mut app);
    let b = ball(&mut app, Vec3::new(0.0, 5.0, 0.0));
    app.step_ticks(10);
    let mid = pos(&app, b);
    assert!(mid.y < 5.0, "ball should fall under gravity, y = {}", mid.y);
    app.step_ticks(290);
    let p = pos(&app, b);
    assert!((p.y - 0.5).abs() < 0.02, "ball should rest on the ground, y = {}", p.y);
    let v = *app.world.get::<&Velocity>(b).unwrap();
    assert!(v.linear.length() < 0.05, "ball should be at rest, v = {:?}", v.linear);
    assert_eq!(app.resources.expect::<Physics>().steps(), 300);
}

#[test]
fn simulation_is_deterministic() {
    let a = run_ball_sim(120);
    let b = run_ball_sim(120);
    assert_eq!(a, b, "two identical runs must produce bit-identical poses");
}

#[test]
fn frame_dt_pattern_does_not_change_results() {
    const TICKS: usize = 120;
    // Both apps are built identically; the tracer records the ball after every physics tick.
    let build = || {
        let mut app = app();
        app.init_resource::<Trace>();
        ground(&mut app);
        let b = ball(&mut app, Vec3::new(0.3, 4.0, -0.2));
        app.add_system(Stage::FixedUpdate, move |ctx| {
            let p = ctx.world.get::<&Transform>(b).unwrap().translation;
            ctx.resources.expect_mut::<Trace>().0.push(p);
        });
        app
    };

    let mut fixed = build();
    fixed.step_ticks(TICKS as u64);

    let mut jittery = build();
    let pattern = [3u64, 41, 17, 9, 33, 1, 25, 70, 12];
    let mut i = 0;
    while jittery.resources.expect::<Trace>().0.len() < TICKS {
        jittery.update(Duration::from_millis(pattern[i % pattern.len()]));
        i += 1;
    }

    let a = &fixed.resources.expect::<Trace>().0[..TICKS];
    let b = &jittery.resources.expect::<Trace>().0[..TICKS];
    assert_eq!(a, b, "per-tick trajectory must not depend on frame timing");
}

#[test]
fn raycast_hits_ground_and_ball() {
    let mut app = app();
    let g = ground(&mut app);
    let b = ball(&mut app, Vec3::new(5.0, 3.0, 0.0));
    app.resources.expect_mut::<Physics>().set_gravity(Vec3::ZERO);
    app.step_ticks(1);

    let physics = app.resources.expect::<Physics>();
    let hit = physics
        .raycast(Vec3::new(0.0, 10.0, 0.0), Vec3::NEG_Y * 3.0, 100.0)
        .expect("hits ground");
    assert_eq!(hit.entity, g);
    assert!((hit.toi - 10.0).abs() < 1e-4, "toi = {}", hit.toi);
    assert!((hit.point - Vec3::ZERO).length() < 1e-4);
    assert!((hit.normal - Vec3::Y).length() < 1e-4);

    let hit = physics
        .raycast(Vec3::new(5.0, 10.0, 0.0), Vec3::NEG_Y, 100.0)
        .expect("hits ball");
    assert_eq!(hit.entity, b);
    assert!((hit.point.y - 3.5).abs() < 1e-3);

    assert!(physics.raycast(Vec3::new(0.0, 10.0, 0.0), Vec3::Y, 100.0).is_none());
    assert!(
        physics.raycast(Vec3::new(0.0, 10.0, 0.0), Vec3::NEG_Y, 5.0).is_none(),
        "max distance respected"
    );

    let filtered = physics.raycast_with(
        Vec3::new(5.0, 10.0, 0.0),
        Vec3::NEG_Y,
        100.0,
        RayFilter {
            exclude: Some(b),
            ..Default::default()
        },
    );
    assert_eq!(filtered.map(|h| h.entity), Some(g));
}

#[test]
fn collision_and_sensor_events_fire() {
    let mut app = app();
    let g = ground(&mut app);
    let trigger = app.world.spawn((
        Transform::from_xyz(0.0, 2.0, 0.0),
        Collider::cuboid(Vec3::splat(1.0)),
        Sensor,
    ));
    let b = ball(&mut app, Vec3::new(0.0, 4.0, 0.0));

    let mut reader = EventReader::default();
    let mut seen = Vec::new();
    for _ in 0..120 {
        app.step_ticks(1);
        seen.extend(reader.read(app.resources.expect::<Events<CollisionEvent>>()).copied());
    }
    let started_with = |other: Entity, sensor: bool| {
        seen.iter().any(|e| {
            matches!(e, CollisionEvent::Started { sensor: s, .. } if *s == sensor) && e.involves(b) && e.involves(other)
        })
    };
    assert!(started_with(trigger, true), "sensor overlap event missing: {seen:?}");
    assert!(started_with(g, false), "ground contact event missing: {seen:?}");
    assert!(
        seen.iter()
            .any(|e| matches!(e, CollisionEvent::Stopped { sensor: true, .. }) && e.involves(trigger)),
        "ball should leave the sensor: {seen:?}"
    );
    // Sensor generates no contact forces: ball passes through and still rests on the ground.
    assert!((pos(&app, b).y - 0.5).abs() < 0.05);

    // Despawning emits a `removed` stop event and cleans up the rapier body.
    let before = app.resources.expect::<Physics>().body_count();
    app.world.despawn(b).unwrap();
    app.step_ticks(1);
    seen.extend(reader.read(app.resources.expect::<Events<CollisionEvent>>()).copied());
    assert!(
        seen.iter()
            .any(|e| matches!(e, CollisionEvent::Stopped { removed: true, .. }) && e.involves(g)),
        "removal stop event missing: {seen:?}"
    );
    let physics = app.resources.expect::<Physics>();
    assert_eq!(physics.body_count(), before - 1);
    assert!(!physics.has_body(b));
    assert_eq!(physics.raw().bodies.len(), before - 1);
}

#[test]
fn despawn_and_component_removal_cleanup() {
    let mut app = app();
    let a = ball(&mut app, Vec3::ZERO);
    let b = ball(&mut app, Vec3::new(3.0, 0.0, 0.0));
    app.step_ticks(1);
    assert_eq!(app.resources.expect::<Physics>().body_count(), 2);
    app.world.despawn(a).unwrap();
    app.world.remove::<(RigidBody, Collider)>(b).unwrap();
    app.step_ticks(1);
    let physics = app.resources.expect::<Physics>();
    assert_eq!(physics.body_count(), 0);
    assert_eq!(physics.raw().bodies.len(), 0);
    assert_eq!(physics.raw().colliders.len(), 0);
}

#[test]
fn velocity_teleport_and_kinematic_sync() {
    let mut app = app();
    app.resources.expect_mut::<Physics>().set_gravity(Vec3::ZERO);
    let b = ball(&mut app, Vec3::ZERO);
    app.step_ticks(1);
    // Setting velocity from game code moves the body.
    app.world.get::<&mut Velocity>(b).unwrap().linear = Vec3::X * 6.0;
    app.step_ticks(60);
    assert!((pos(&app, b).x - 6.0).abs() < 0.2, "x = {}", pos(&app, b).x);
    // Teleport by editing Transform.
    app.world.get::<&mut Transform>(b).unwrap().translation = Vec3::new(-10.0, 0.0, 0.0);
    app.step_ticks(1);
    assert!(pos(&app, b).x < -9.0);

    // Kinematic bodies follow their Transform.
    let k = app.world.spawn((
        Transform::from_xyz(0.0, 20.0, 0.0),
        RigidBody::KinematicPosition,
        Collider::cuboid(Vec3::ONE),
    ));
    app.step_ticks(1);
    app.world.get::<&mut Transform>(k).unwrap().translation.x = 4.0;
    app.step_ticks(2);
    let h = app.resources.expect::<Physics>().body_handle(k).unwrap();
    let x = app.resources.expect::<Physics>().raw().bodies[h].translation().x;
    assert!((x - 4.0).abs() < 1e-5);
}

#[test]
fn trimesh_and_capsule_colliders() {
    let mut app = app();
    app.world.spawn((
        Transform::IDENTITY.with_scale(Vec3::splat(40.0)),
        RigidBody::Fixed,
        Collider::trimesh(MeshData::plane(1.0)),
    ));
    let c = app.world.spawn((
        Transform::from_xyz(0.0, 3.0, 0.0),
        RigidBody::Dynamic,
        Collider::capsule(0.5, 0.25),
    ));
    app.step_ticks(240);
    let p = pos(&app, c);
    // Capsule may tip over; it must stay above the mesh and within its total half-length.
    assert!(
        p.y > 0.2 && p.y < 0.8,
        "capsule should rest on the trimesh plane, y = {}",
        p.y
    );
}

#[test]
fn restitution_makes_things_bounce() {
    let bounce_height = |r: f32| {
        let mut app = app();
        let g = ground(&mut app);
        app.world.insert_one(g, Restitution(r)).unwrap();
        let b = ball(&mut app, Vec3::new(0.0, 3.0, 0.0));
        app.world.insert_one(b, Restitution(r)).unwrap();
        // Fall (~0.7 s) then track the max height reached after the bounce.
        app.step_ticks(50);
        let mut max_y = f32::MIN;
        for _ in 0..60 {
            app.step_ticks(1);
            max_y = max_y.max(pos(&app, b).y);
        }
        max_y
    };
    assert!(bounce_height(0.9) > bounce_height(0.0) + 0.5);
}
