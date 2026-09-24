//! World fingerprints and two-run determinism checks.
//!
//! [`assert_deterministic`] builds the same app twice, runs both **in lockstep** and compares a
//! [`WorldFingerprint`] after every tick, so a divergence is reported at the first tick it
//! happens, with the entity and component that differ. When a GPU is available the final
//! frames are also rendered and must be bit-identical.
//!
//! ```
//! use space3d_core::prelude::*;
//! use space3d_test::{DeterminismCheck, TestApp};
//!
//! let report = DeterminismCheck::new(30).screenshots(false).assert(|app: &mut TestApp| {
//!     let e = app.world.spawn((Transform::default(),));
//!     app.add_system(Stage::FixedUpdate, move |ctx| {
//!         ctx.world.get::<&mut Transform>(e).unwrap().translation.x += 0.1;
//!     });
//! });
//! assert_eq!(report.ticks, 30);
//! ```

use crate::app::{TestApp, TestAppBuilder};
use crate::compare::{CompareOptions, compare_images};
use crate::input::InputScript;
use crate::report::{ReportEntry, ReportImages, ReportKind, ReportStatus, TestReport, scoped_check_name};
use space3d_core::hecs::Component;
use space3d_core::{App, Entity, Name, Parent, Resources, Transform, World};
use std::fmt::Debug;

type ComponentReader = fn(&World, Entity) -> Option<String>;
type ResourceReader = fn(&Resources) -> Option<String>;

fn read_component<C: Component + Debug>(world: &World, e: Entity) -> Option<String> {
    world.get::<&C>(e).ok().map(|c| format!("{:?}", *c))
}

fn read_resource<R: Debug + 'static>(res: &Resources) -> Option<String> {
    res.get::<R>().map(|r| format!("{r:?}"))
}

/// Chooses what goes into a [`WorldFingerprint`]: components (per entity) and resources,
/// captured through their `Debug` representation (exact for floats: Rust prints the shortest
/// string that round-trips).
///
/// The default fingerprints `Transform`, `Name` and `Parent`.
#[derive(Clone)]
pub struct Fingerprinter {
    components: Vec<(&'static str, ComponentReader)>,
    resources: Vec<(&'static str, ResourceReader)>,
}

impl Default for Fingerprinter {
    fn default() -> Self {
        Self::empty()
            .component::<Transform>()
            .component::<Name>()
            .component::<Parent>()
    }
}

impl std::fmt::Debug for Fingerprinter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fingerprinter")
            .field("components", &self.components.iter().map(|c| c.0).collect::<Vec<_>>())
            .field("resources", &self.resources.iter().map(|c| c.0).collect::<Vec<_>>())
            .finish()
    }
}

impl Fingerprinter {
    /// `Transform`, `Name`, `Parent`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Nothing registered.
    pub fn empty() -> Self {
        Self {
            components: Vec::new(),
            resources: Vec::new(),
        }
    }

    /// Also fingerprint component `C`.
    pub fn component<C: Component + Debug>(mut self) -> Self {
        self.components.push((std::any::type_name::<C>(), read_component::<C>));
        self
    }

    /// Also fingerprint resource `R`.
    pub fn resource<R: Debug + 'static>(mut self) -> Self {
        self.resources.push((std::any::type_name::<R>(), read_resource::<R>));
        self
    }

    /// Fingerprint an app (world + resources).
    pub fn capture(&self, app: &App) -> WorldFingerprint {
        let mut fp = self.capture_world(&app.world);
        fp.resources = self
            .resources
            .iter()
            .filter_map(|(name, read)| read(&app.resources).map(|v| (*name, v)))
            .collect();
        fp
    }

    /// Fingerprint a world (entities sorted by id).
    pub fn capture_world(&self, world: &World) -> WorldFingerprint {
        let mut ids: Vec<Entity> = world.iter().map(|e| e.entity()).collect();
        ids.sort_by_key(|e| e.to_bits());
        let entities = ids
            .into_iter()
            .filter_map(|e| {
                let components: Vec<(&'static str, String)> = self
                    .components
                    .iter()
                    .filter_map(|(name, read)| read(world, e).map(|v| (*name, v)))
                    .collect();
                (!components.is_empty()).then_some(EntityFingerprint { entity: e, components })
            })
            .collect();
        WorldFingerprint {
            entities,
            resources: Vec::new(),
        }
    }
}

/// Fingerprinted components of one entity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityFingerprint {
    /// Entity id.
    pub entity: Entity,
    /// `(type name, Debug text)` per fingerprinted component present.
    pub components: Vec<(&'static str, String)>,
}

/// Snapshot of the fingerprinted state of a world, comparable across runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorldFingerprint {
    /// Entities that have at least one fingerprinted component, sorted by id.
    pub entities: Vec<EntityFingerprint>,
    /// `(type name, Debug text)` per fingerprinted resource present.
    pub resources: Vec<(&'static str, String)>,
}

fn fnv(hash: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *hash ^= *b as u64;
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

impl WorldFingerprint {
    /// Stable 64-bit FNV-1a hash (identical across runs, processes and platforms).
    pub fn hash(&self) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for e in &self.entities {
            fnv(&mut h, &e.entity.to_bits().get().to_le_bytes());
            for (name, v) in &e.components {
                fnv(&mut h, name.as_bytes());
                fnv(&mut h, v.as_bytes());
            }
        }
        for (name, v) in &self.resources {
            fnv(&mut h, name.as_bytes());
            fnv(&mut h, v.as_bytes());
        }
        h
    }

    /// Human-readable description of the first difference, `None` if equal.
    pub fn diff(&self, other: &WorldFingerprint) -> Option<String> {
        if self == other {
            return None;
        }
        for (a, b) in self.entities.iter().zip(other.entities.iter()) {
            if a.entity != b.entity {
                return Some(format!(
                    "entity sets differ: {:?} vs {:?} (spawn order or despawns diverged)",
                    a.entity, b.entity
                ));
            }
            if a.components != b.components {
                for (x, y) in a.components.iter().zip(b.components.iter()) {
                    if x != y {
                        return Some(format!(
                            "entity {:?}: {} differs\n    run A: {} = {}\n    run B: {} = {}",
                            a.entity, x.0, x.0, x.1, y.0, y.1
                        ));
                    }
                }
                return Some(format!(
                    "entity {:?}: component sets differ\n    run A: {:?}\n    run B: {:?}",
                    a.entity,
                    a.components.iter().map(|c| c.0).collect::<Vec<_>>(),
                    b.components.iter().map(|c| c.0).collect::<Vec<_>>()
                ));
            }
        }
        if self.entities.len() != other.entities.len() {
            return Some(format!(
                "entity count differs: {} vs {}",
                self.entities.len(),
                other.entities.len()
            ));
        }
        for (x, y) in self.resources.iter().zip(other.resources.iter()) {
            if x != y {
                return Some(format!(
                    "resource {} differs\n    run A: {}\n    run B: {}",
                    x.0, x.1, y.1
                ));
            }
        }
        Some(format!(
            "resource sets differ: {:?} vs {:?}",
            self.resources.iter().map(|r| r.0).collect::<Vec<_>>(),
            other.resources.iter().map(|r| r.0).collect::<Vec<_>>()
        ))
    }
}

/// Hash of `Transform`, `Name` and `Parent` of every entity (see [`Fingerprinter`]).
pub fn world_hash(world: &World) -> u64 {
    Fingerprinter::default().capture_world(world).hash()
}

/// Result of a passing determinism check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeterminismReport {
    /// Ticks simulated per run.
    pub ticks: u64,
    /// Fingerprint hash after the last tick.
    pub final_hash: u64,
    /// Entities fingerprinted after the last tick.
    pub entities: usize,
    /// Final frames were rendered and compared.
    pub screenshots_compared: bool,
}

/// Configurable two-run determinism check (see the [module docs](self)).
#[derive(Clone, Debug)]
pub struct DeterminismCheck {
    ticks: u64,
    builder: TestAppBuilder,
    fingerprinter: Fingerprinter,
    screenshots: bool,
    script: Option<InputScript>,
    name: Option<String>,
}

impl DeterminismCheck {
    /// Check `ticks` ticks with default settings (default [`TestApp`] builder, default
    /// [`Fingerprinter`], final screenshots compared when a GPU exists).
    pub fn new(ticks: u64) -> Self {
        Self {
            ticks,
            builder: TestApp::builder(),
            fingerprinter: Fingerprinter::default(),
            screenshots: true,
            script: None,
            name: None,
        }
    }

    /// Builder used for both apps.
    pub fn builder(mut self, builder: TestAppBuilder) -> Self {
        self.builder = builder;
        self
    }

    /// What to fingerprint.
    pub fn fingerprinter(mut self, fingerprinter: Fingerprinter) -> Self {
        self.fingerprinter = fingerprinter;
        self
    }

    /// Compare final frames (default `true`; ignored without GPU).
    pub fn screenshots(mut self, enabled: bool) -> Self {
        self.screenshots = enabled;
        self
    }

    /// Play the same input script in both runs (scheduled at tick 0).
    pub fn input(mut self, script: InputScript) -> Self {
        self.script = Some(script);
        self
    }

    /// Name used in reports and panic messages (default `<current test>::determinism`, see
    /// [`scoped_check_name`], so checks of different tests do not overwrite each other).
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Run the check; `Err` describes the first divergence.
    pub fn run(&self, setup: impl Fn(&mut TestApp)) -> Result<DeterminismReport, String> {
        let make = || {
            let mut app = self.builder.clone().build();
            setup(&mut app);
            if let Some(s) = &self.script {
                app.schedule(s);
            }
            app
        };
        let (mut a, mut b) = (make(), make());
        let check = |a: &TestApp, b: &TestApp, when: &str| -> Result<WorldFingerprint, String> {
            let (fa, fb) = (self.fingerprinter.capture(a), self.fingerprinter.capture(b));
            match fa.diff(&fb) {
                None => Ok(fa),
                Some(d) => Err(format!("runs diverged {when}: {d}")),
            }
        };
        let mut last = check(&a, &b, "after setup")?;
        for t in 0..self.ticks {
            a.step();
            b.step();
            last = check(&a, &b, &format!("at tick {t}"))?;
        }
        let mut screenshots_compared = false;
        if self.screenshots
            && let (Some(ia), Some(ib)) = (a.screenshot(), b.screenshot())
        {
            let stats = compare_images(&ia, &ib, &CompareOptions::exact());
            if !stats.passed {
                return Err(format!(
                    "final frames differ although the fingerprints match: {}",
                    stats.summary()
                ));
            }
            screenshots_compared = true;
        }
        Ok(DeterminismReport {
            ticks: self.ticks,
            final_hash: last.hash(),
            entities: last.entities.len(),
            screenshots_compared,
        })
    }

    /// Run the check and panic on divergence.
    #[track_caller]
    pub fn assert(&self, setup: impl Fn(&mut TestApp)) -> DeterminismReport {
        // Not `unwrap_or_else`: a closure would hide the `#[track_caller]` location.
        let name = match &self.name {
            Some(name) => name.clone(),
            None => scoped_check_name("determinism"),
        };
        let result = self.run(setup);
        if let Some(report) = TestReport::from_env() {
            let (status, message, details) = match &result {
                Ok(r) => (
                    ReportStatus::Passed,
                    None,
                    serde_json::json!({
                        "ticks": r.ticks,
                        "final_hash": format!("{:016x}", r.final_hash),
                        "entities": r.entities,
                        "screenshots_compared": r.screenshots_compared,
                    }),
                ),
                Err(e) => (ReportStatus::Failed, Some(e.clone()), serde_json::Value::Null),
            };
            report.record_or_warn(
                ReportEntry::new(
                    &std::env::var("CARGO_PKG_NAME").unwrap_or_default(),
                    &name,
                    ReportKind::Determinism,
                    status,
                )
                .message(message)
                .details(details),
                ReportImages::default(),
            );
        }
        match result {
            Ok(r) => r,
            Err(e) => panic!("determinism check {name:?} failed: {e}"),
        }
    }
}

/// Build the app twice with `setup`, run `ticks` ticks in lockstep and assert identical world
/// fingerprints after every tick and identical final frames (when a GPU is available).
#[track_caller]
pub fn assert_deterministic(setup: impl Fn(&mut TestApp), ticks: u64) -> DeterminismReport {
    DeterminismCheck::new(ticks).assert(setup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_core::Stage;
    use space3d_core::glam::Vec3;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn identical_worlds_hash_equal() {
        let mut w = World::new();
        let p = w.spawn((Name("p".into()), Transform::from_xyz(1.0, 2.0, 3.0)));
        w.spawn((Transform::IDENTITY, Parent(p)));
        w.spawn((42u8,));
        let fp = Fingerprinter::default().capture_world(&w);
        assert_eq!(fp.entities.len(), 2, "u8-only entity is not fingerprinted");
        assert_eq!(world_hash(&w), fp.hash());
        let mut w2 = World::new();
        let p2 = w2.spawn((Name("p".into()), Transform::from_xyz(1.0, 2.0, 3.0)));
        w2.spawn((Transform::IDENTITY, Parent(p2)));
        assert_eq!(world_hash(&w), world_hash(&w2));
        w2.get::<&mut Transform>(p2).unwrap().translation.y = 2.000_001;
        let d = Fingerprinter::default()
            .capture_world(&w)
            .diff(&Fingerprinter::default().capture_world(&w2))
            .unwrap();
        assert!(d.contains("Transform differs") && d.contains("2.000001"), "{d}");
        assert_ne!(world_hash(&w), world_hash(&w2));
    }

    #[test]
    fn deterministic_simulation_passes() {
        let r = DeterminismCheck::new(20)
            .builder(TestApp::builder().no_render())
            .fingerprinter(Fingerprinter::new().resource::<u64>())
            .assert(|app| {
                app.insert_resource(0u64);
                let e = app.world.spawn((Transform::default(),));
                app.add_system(Stage::FixedUpdate, move |ctx| {
                    let t = ctx.time().tick as f32;
                    ctx.world.get::<&mut Transform>(e).unwrap().translation = Vec3::new(t.sin(), t.cos(), 0.0);
                    *ctx.resources.expect_mut::<u64>() += 3;
                });
            });
        assert_eq!((r.ticks, r.entities, r.screenshots_compared), (20, 1, false));
    }

    #[test]
    fn divergence_is_reported_at_first_tick() {
        static SHARED: AtomicU64 = AtomicU64::new(0);
        let err = DeterminismCheck::new(10)
            .builder(TestApp::builder().no_render())
            .run(|app| {
                let e = app.world.spawn((Transform::default(),));
                app.add_system(Stage::FixedUpdate, move |ctx| {
                    // Global state shared by both runs: a classic nondeterminism bug.
                    let n = SHARED.fetch_add(1, Ordering::SeqCst);
                    if ctx.time().tick == 3 {
                        ctx.world.get::<&mut Transform>(e).unwrap().translation.x = n as f32;
                    }
                });
            })
            .unwrap_err();
        assert!(err.contains("at tick 3"), "{err}");
    }

    #[test]
    fn default_name_is_scoped_to_the_test() {
        static FLIP: AtomicU64 = AtomicU64::new(0);
        // Fails on purpose: keep it out of a real SPACE3D_TEST_REPORT.
        let err = crate::report::without_report(|| {
            std::panic::catch_unwind(|| {
                DeterminismCheck::new(1)
                    .builder(TestApp::builder().no_render())
                    .assert(|app| {
                        if FLIP.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
                            app.world.spawn((Name("x".into()),));
                        }
                    })
            })
            .unwrap_err()
        });
        let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
        match std::thread::current().name() {
            Some(test) if test != "main" => {
                assert!(test.ends_with("default_name_is_scoped_to_the_test"));
                let expected = format!("determinism check \"{test}::determinism\" failed");
                assert!(msg.contains(&expected), "{msg}");
            }
            _ => assert!(msg.contains(file!()), "{msg}"),
        }
    }

    #[test]
    #[should_panic(expected = "determinism check \"spawns\" failed")]
    fn assert_panics_on_divergent_spawns() {
        static FLIP: AtomicU64 = AtomicU64::new(0);
        crate::report::without_report(|| {
            DeterminismCheck::new(1)
                .builder(TestApp::builder().no_render())
                .name("spawns")
                .assert(|app| {
                    if FLIP.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
                        app.world.spawn((Name("x".into()),));
                    }
                })
        });
    }
}
