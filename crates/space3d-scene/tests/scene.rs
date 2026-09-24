use serde::{Deserialize, Serialize};
use space3d_core::prelude::*;
use space3d_scene::*;
use space3d_tasks::TasksPlugin;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Health {
    hp: i32,
    max: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum Team {
    Red,
    Blue { squad: String },
}

/// Component holding an entity reference, saved as a stable id.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Target(Entity);

fn registry() -> ComponentRegistry {
    let mut r = ComponentRegistry::default();
    r.register::<Health>("Health")
        .register::<Team>("Team")
        .register_mapped::<Target, StableId>(
            "Target",
            |t, ctx| ctx.stable_id(t.0),
            |id, ctx| ctx.entity(id).map(Target),
        );
    r
}

fn name(world: &World, e: Entity) -> String {
    world.get::<&Name>(e).unwrap().0.clone()
}

fn find(world: &World, n: &str) -> Entity {
    world
        .query::<(Entity, &Name)>()
        .iter()
        .find(|(_, name)| name.0 == n)
        .map(|(e, _)| e)
        .unwrap_or_else(|| panic!("no entity named {n}"))
}

fn sample_world() -> World {
    let mut w = World::new();
    let ship = w.spawn((
        Name("ship".into()),
        Transform::from_xyz(1.5, -2.25, 3.1).with_rotation(Quat::from_rotation_y(0.7)),
        Health { hp: -3, max: 100 },
        Team::Blue { squad: "alpha".into() },
    ));
    let turret = w.spawn((
        Name("turret".into()),
        Transform::from_xyz(0.0, 1.1, 0.0),
        Parent(ship),
        Team::Red,
    ));
    w.spawn((
        Name("barrel".into()),
        Transform::from_xyz(0.0, 0.0, -0.9),
        Parent(turret),
        Target(ship),
    ));
    w
}

#[test]
fn roundtrip_ron_and_json() {
    let reg = registry();
    let src = sample_world();
    let scene = save_world(&src, &reg).unwrap();
    assert_eq!(scene.len(), 3);

    for fmt in [SceneFormat::Ron, SceneFormat::Json] {
        let bytes = scene.to_bytes(fmt).unwrap();
        let parsed = Scene::from_bytes(&bytes, fmt).unwrap();
        assert_eq!(parsed, scene, "{fmt:?} text must parse back to an identical scene");

        let mut dst = World::new();
        let spawned = parsed.spawn_into(&mut dst, &reg).unwrap();
        assert_eq!(spawned.len(), 3);
        for n in ["ship", "turret", "barrel"] {
            let (a, b) = (find(&src, n), find(&dst, n));
            assert_eq!(
                *src.get::<&Transform>(a).unwrap(),
                *dst.get::<&Transform>(b).unwrap(),
                "{n}"
            );
            assert_eq!(
                src.get::<&Health>(a).ok().map(|h| *h),
                dst.get::<&Health>(b).ok().map(|h| *h)
            );
            assert_eq!(
                src.get::<&Team>(a).ok().map(|t| (*t).clone()),
                dst.get::<&Team>(b).ok().map(|t| (*t).clone())
            );
        }
        // Saving the loaded world again yields the same scene (ids preserved).
        assert_eq!(save_world(&dst, &reg).unwrap(), scene);
    }

    let ron = scene.to_ron().unwrap();
    assert!(ron.contains("\"Transform\"") && ron.contains("\"Blue\""), "{ron}");
}

#[test]
fn parent_and_entity_refs_are_remapped() {
    let reg = registry();
    let scene = save_world(&sample_world(), &reg).unwrap();
    let mut dst = World::new();
    // Occupy low entity ids so remapping is actually needed.
    for _ in 0..5 {
        dst.spawn((Name("filler".into()),));
    }
    scene.spawn_into(&mut dst, &reg).unwrap();
    let (ship, turret, barrel) = (find(&dst, "ship"), find(&dst, "turret"), find(&dst, "barrel"));
    assert_eq!(dst.get::<&Parent>(turret).unwrap().0, ship);
    assert_eq!(dst.get::<&Parent>(barrel).unwrap().0, turret);
    assert_eq!(dst.get::<&Target>(barrel).unwrap().0, ship);
    assert!(dst.get::<&Parent>(ship).is_err());
    // Stable ids are restored as components.
    let ship_id = *dst.get::<&StableId>(ship).unwrap();
    assert_eq!(
        scene.get(ship_id).unwrap().components["Name"],
        serde_json::json!("ship")
    );
}

#[test]
fn prefab_instances_are_distinct_and_placed() {
    let reg = registry();
    let src = sample_world();
    let prefab = Prefab::from_world(&src, &reg, find(&src, "ship")).unwrap();
    assert_eq!(prefab.scene.len(), 3);

    let mut app = App::new();
    let a = prefab
        .instantiate(&mut app.world, &reg, Transform::from_xyz(10.0, 0.0, 0.0))
        .unwrap();
    let b = prefab
        .instantiate(&mut app.world, &reg, Transform::from_xyz(-10.0, 0.0, 0.0))
        .unwrap();
    assert_eq!((a.len(), b.len()), (3, 3));
    assert!(a.iter().all(|e| !b.contains(e)), "instances must not share entities");
    let ids = |v: &[Entity]| {
        v.iter()
            .map(|&e| *app.world.get::<&StableId>(e).unwrap())
            .collect::<Vec<_>>()
    };
    let (ia, ib) = (ids(&a), ids(&b));
    assert!(ia.iter().all(|id| !ib.contains(id)), "instances get fresh stable ids");

    // Hierarchy is internal to each instance.
    assert_eq!(app.world.get::<&Parent>(a[1]).unwrap().0, a[0]);
    assert_eq!(app.world.get::<&Parent>(b[1]).unwrap().0, b[0]);
    assert_eq!(app.world.get::<&Target>(b[2]).unwrap().0, b[0]);

    // Root placed at the override (template root is composed with it); children stay local.
    let root_src = *src.get::<&Transform>(find(&src, "ship")).unwrap();
    let ra = *app.world.get::<&Transform>(a[0]).unwrap();
    assert!((ra.translation - (Vec3::new(10.0, 0.0, 0.0) + root_src.translation)).length() < 1e-5);
    assert_eq!(
        *app.world.get::<&Transform>(a[1]).unwrap(),
        Transform::from_xyz(0.0, 1.1, 0.0)
    );

    // World transforms propagate through the remapped hierarchy.
    app.step_ticks(1);
    let ga = app.world.get::<&GlobalTransform>(a[1]).unwrap().0.w_axis.truncate();
    let gb = app.world.get::<&GlobalTransform>(b[1]).unwrap().0.w_axis.truncate();
    assert!(((ga - gb) - Vec3::new(20.0, 0.0, 0.0)).length() < 1e-4);
}

#[test]
fn snapshot_restore_after_mutation() {
    let reg = registry();
    let mut w = sample_world();
    let ship = find(&w, "ship");
    let turret = find(&w, "turret");
    let barrel = find(&w, "barrel");
    for e in [ship, turret, barrel] {
        w.insert_one(e, StableId::new()).unwrap();
    }
    let snap = WorldSnapshot::capture(&w, &reg);
    assert_eq!(snap.len(), 3);
    let before = save_world(&w, &reg).unwrap();

    // Mutate: edit, remove, add components; despawn; spawn new.
    w.get::<&mut Transform>(ship).unwrap().translation.x += 100.0;
    w.get::<&mut Health>(ship).unwrap().hp = 999;
    w.remove_one::<Team>(turret).unwrap();
    w.insert_one(turret, Health { hp: 1, max: 1 }).unwrap();
    w.despawn(barrel).unwrap();
    let extra = w.spawn((Name("extra".into()), Transform::IDENTITY));

    snap.restore(&mut w, &reg);

    assert!(!w.contains(extra));
    assert!(w.contains(barrel), "despawned entity comes back with the same handle");
    assert_eq!(w.get::<&Parent>(barrel).unwrap().0, turret);
    assert_eq!(w.get::<&Target>(barrel).unwrap().0, ship);
    assert_eq!(w.get::<&Health>(ship).unwrap().hp, -3);
    assert!(w.get::<&Health>(turret).is_err());
    assert_eq!(*w.get::<&Team>(turret).unwrap(), Team::Red);
    assert_eq!(name(&w, barrel), "barrel");
    // Bit-exact: re-saving gives the identical scene (including stable ids).
    let after = save_world(&w, &reg).unwrap();
    assert_eq!(before.to_ron().unwrap(), after.to_ron().unwrap());
}

#[test]
fn snapshot_enables_deterministic_replay() {
    #[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
    struct Vel(Vec3);

    let mut reg = ComponentRegistry::default();
    reg.register::<Vel>("Vel");
    let mut app = App::new();
    app.add_system(Stage::FixedUpdate, |ctx| {
        let dt = ctx.time().fixed_delta_secs();
        for (t, v) in ctx.world.query_mut::<(&mut Transform, &mut Vel)>() {
            v.0.y -= 9.81 * dt;
            t.translation += v.0 * dt;
        }
    });
    let e = app
        .world
        .spawn((Transform::from_xyz(0.0, 10.0, 0.0), Vel(Vec3::new(1.0, 5.0, 0.0))));
    app.step_ticks(10);
    let snap = WorldSnapshot::capture(&app.world, &reg);
    app.step_ticks(50);
    let first = *app.world.get::<&Transform>(e).unwrap();
    snap.restore(&mut app.world, &reg);
    app.step_ticks(50);
    assert_eq!(*app.world.get::<&Transform>(e).unwrap(), first);
}

#[test]
fn unknown_components_fail_without_side_effects() {
    let reg = registry();
    let scene = save_world(&sample_world(), &reg).unwrap();
    let mut w = World::new();
    let err = scene.spawn_into(&mut w, &ComponentRegistry::default()).unwrap_err();
    assert!(matches!(err, SceneError::UnknownComponent(_)), "{err}");
    assert_eq!(w.len(), 0);

    let mut bad = scene.clone();
    bad.entities[0]
        .components
        .insert("Health".into(), serde_json::json!("not a health"));
    let err = bad.spawn_into(&mut w, &reg).unwrap_err();
    assert!(matches!(err, SceneError::Component { .. }), "{err}");
    assert_eq!(w.len(), 0, "partially spawned entities are rolled back");
}

fn wait_for(app: &mut App, id: SceneLoadId) -> SceneLoadStatus {
    for _ in 0..5000 {
        app.step_ticks(1);
        match app.resources.expect::<SceneCommands>().status(id) {
            SceneLoadStatus::Pending => std::thread::sleep(Duration::from_millis(1)),
            other => return other,
        }
    }
    panic!("scene load timed out");
}

#[test]
fn async_load_from_bytes_and_path() {
    let reg = registry();
    let ron = save_world(&sample_world(), &reg).unwrap().to_ron().unwrap();

    let mut app = App::new();
    app.add_plugin(TasksPlugin);
    app.insert_resource(registry());
    app.add_plugin(ScenePlugin);
    assert!(
        app.resources.expect::<ComponentRegistry>().contains("Health"),
        "plugin keeps a custom registry"
    );

    let id = app
        .resources
        .expect_mut::<SceneCommands>()
        .load_bytes(ron.clone().into_bytes(), SceneFormat::Ron);
    let SceneLoadStatus::Spawned(entities) = wait_for(&mut app, id) else {
        panic!("load failed")
    };
    assert_eq!(entities.len(), 3);
    assert_eq!(name(&app.world, entities[0]), "ship");
    let mut reader = EventReader::default();
    let events: Vec<_> = reader
        .read(app.resources.expect::<Events<SceneLoaded>>())
        .cloned()
        .collect();
    assert!(
        events
            .iter()
            .any(|e| e.id == id && e.result.as_ref().is_ok_and(|v| v == &entities))
    );

    // From a file, instantiated as a prefab.
    let dir = std::env::temp_dir().join(format!("space3d-scene-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ship.scn.json");
    std::fs::write(&path, Scene::from_ron(&ron).unwrap().to_json().unwrap()).unwrap();
    let id2 = app
        .resources
        .expect_mut::<SceneCommands>()
        .instantiate_path(&path, Transform::from_xyz(0.0, 50.0, 0.0));
    let SceneLoadStatus::Spawned(inst) = wait_for(&mut app, id2) else {
        panic!("path load failed")
    };
    assert!(app.world.get::<&Transform>(inst[0]).unwrap().translation.y > 40.0);
    assert_eq!(app.world.get::<&Parent>(inst[1]).unwrap().0, inst[0]);

    // Missing files fail cleanly.
    let id3 = app
        .resources
        .expect_mut::<SceneCommands>()
        .load_path(dir.join("missing.ron"));
    assert!(matches!(
        wait_for(&mut app, id3),
        SceneLoadStatus::Failed(SceneError::Io(_))
    ));
    std::fs::remove_dir_all(&dir).ok();
}
