//! Space3d game engine facade. Depend on this crate and `use space3d::prelude::*`.
pub use space3d_core as core;
pub use space3d_tasks as tasks;

pub mod prelude {
    pub use space3d_core::prelude::*;
    pub use space3d_tasks::{AsyncQueue, Task, TasksPlugin, spawn, spawn_compute};
}
