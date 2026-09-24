//! Editing operations on CPU-side asset data, plus undo/redo histories.
//!
//! * [`texture`] — pure functions on [`space3d_core::ImageData`] and [`texture::ImageEditHistory`].
//! * [`mesh`] — pure functions on [`space3d_core::MeshData`], OBJ/glTF export and
//!   [`mesh::MeshEditHistory`].

pub mod history;
pub mod mesh;
pub mod texture;

pub use history::{HistorySize, SnapshotHistory};
