use crate::resources::Resources;
use hecs::World;

/// Execution stages, run in declaration order each frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stage {
    Startup,
    PreUpdate,
    FixedUpdate,
    Update,
    PostUpdate,
    Render,
    Last,
}

impl Stage {
    pub const ALL: [Stage; 7] = [
        Stage::Startup,
        Stage::PreUpdate,
        Stage::FixedUpdate,
        Stage::Update,
        Stage::PostUpdate,
        Stage::Render,
        Stage::Last,
    ];
    pub(crate) fn index(self) -> usize {
        self as usize
    }
}

/// What a system gets: the world and resources, borrowed disjointly.
pub struct SystemCtx<'a> {
    pub world: &'a mut World,
    pub resources: &'a mut Resources,
}

impl SystemCtx<'_> {
    pub fn time(&self) -> &crate::Time {
        self.resources.expect()
    }
    pub fn send<T: Send + Sync + 'static>(&mut self, event: T) {
        self.resources.init::<crate::Events<T>>().send(event);
    }
}

pub type BoxedSystem = Box<dyn FnMut(&mut SystemCtx) + Send>;

pub(crate) struct NamedSystem {
    pub name: &'static str,
    pub run: BoxedSystem,
}

#[derive(Default)]
pub(crate) struct Schedule {
    pub stages: [Vec<NamedSystem>; 7],
}

impl Schedule {
    pub fn run_stage(&mut self, stage: Stage, world: &mut World, resources: &mut Resources) {
        let mut ctx = SystemCtx { world, resources };
        for sys in &mut self.stages[stage.index()] {
            let _span = tracing::trace_span!("system", name = sys.name).entered();
            (sys.run)(&mut ctx);
        }
    }
}
