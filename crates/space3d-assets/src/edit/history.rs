//! Generic snapshot-based undo/redo.

use space3d_core::{ImageData, MeshData};
use std::collections::VecDeque;

/// Approximate heap size of a value, for history memory caps.
pub trait HistorySize {
    /// Bytes this value occupies (roughly).
    fn history_bytes(&self) -> usize;
}

impl HistorySize for MeshData {
    fn history_bytes(&self) -> usize {
        self.positions.len() * 12
            + self.normals.len() * 12
            + self.uvs.len() * 8
            + self.colors.len() * 16
            + self.indices.len() * 4
    }
}

impl HistorySize for ImageData {
    fn history_bytes(&self) -> usize {
        self.pixels.len()
    }
}

/// Undo/redo by storing a full copy of the value before each edit.
///
/// The oldest steps are evicted once the stored snapshots exceed the memory cap or the step
/// limit; the most recent step is always kept.
pub struct SnapshotHistory<T> {
    current: T,
    undo: VecDeque<(String, T)>,
    redo: Vec<(String, T)>,
    bytes: usize,
    memory_cap: usize,
    max_steps: usize,
}

impl<T: Clone + HistorySize> SnapshotHistory<T> {
    /// Default memory cap for stored snapshots: 256 MiB.
    pub const DEFAULT_MEMORY_CAP: usize = 256 << 20;
    /// Default maximum number of undo steps.
    pub const DEFAULT_MAX_STEPS: usize = 100;

    /// Start tracking `value`.
    pub fn new(value: T) -> Self {
        Self {
            current: value,
            undo: VecDeque::new(),
            redo: Vec::new(),
            bytes: 0,
            memory_cap: Self::DEFAULT_MEMORY_CAP,
            max_steps: Self::DEFAULT_MAX_STEPS,
        }
    }

    /// Set the snapshot memory cap in bytes.
    pub fn with_memory_cap(mut self, bytes: usize) -> Self {
        self.memory_cap = bytes;
        self.enforce_limits();
        self
    }

    /// Set the maximum number of undo steps (at least 1).
    pub fn with_max_steps(mut self, steps: usize) -> Self {
        self.max_steps = steps.max(1);
        self.enforce_limits();
        self
    }

    /// The current value.
    pub fn current(&self) -> &T {
        &self.current
    }

    /// Mutable access *without* recording a step (call [`SnapshotHistory::checkpoint`] first).
    pub fn current_mut(&mut self) -> &mut T {
        &mut self.current
    }

    /// Consume the history, returning the current value.
    pub fn into_inner(self) -> T {
        self.current
    }

    fn enforce_limits(&mut self) {
        while self.undo.len() > 1 && (self.bytes > self.memory_cap || self.undo.len() > self.max_steps) {
            if let Some((_, v)) = self.undo.pop_front() {
                self.bytes -= v.history_bytes();
            }
        }
    }

    /// Snapshot the current value as an undo step labelled `label`.
    pub fn checkpoint(&mut self, label: impl Into<String>) {
        for (_, v) in self.redo.drain(..) {
            self.bytes -= v.history_bytes();
        }
        self.bytes += self.current.history_bytes();
        self.undo.push_back((label.into(), self.current.clone()));
        self.enforce_limits();
    }

    /// Snapshot, then run `op` on the value.
    pub fn apply<R>(&mut self, label: impl Into<String>, op: impl FnOnce(&mut T) -> R) -> R {
        self.checkpoint(label);
        op(&mut self.current)
    }

    /// Undo the last step. Returns `false` if there was nothing to undo.
    pub fn undo(&mut self) -> bool {
        let Some((label, mut prev)) = self.undo.pop_back() else {
            return false;
        };
        std::mem::swap(&mut self.current, &mut prev);
        self.bytes = self.bytes - self.current.history_bytes() + prev.history_bytes();
        self.redo.push((label, prev));
        true
    }

    /// Redo the last undone step. Returns `false` if there was nothing to redo.
    pub fn redo(&mut self) -> bool {
        let Some((label, mut next)) = self.redo.pop() else {
            return false;
        };
        std::mem::swap(&mut self.current, &mut next);
        self.bytes = self.bytes - self.current.history_bytes() + next.history_bytes();
        self.undo.push_back((label, next));
        true
    }

    /// Anything to undo?
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Anything to redo?
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Label of the step `undo` would revert.
    pub fn undo_label(&self) -> Option<&str> {
        self.undo.back().map(|(l, _)| l.as_str())
    }

    /// Label of the step `redo` would re-apply.
    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|(l, _)| l.as_str())
    }

    /// Number of undo steps stored.
    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    /// Number of redo steps stored.
    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }

    /// Bytes held by snapshots.
    pub fn memory_usage(&self) -> usize {
        self.bytes
    }

    /// Drop all undo/redo steps.
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.bytes = 0;
    }
}
