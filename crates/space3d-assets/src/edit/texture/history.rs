//! Undo/redo for image editing with region snapshots and a memory cap.

use super::Rect;
use space3d_core::ImageData;
use std::collections::VecDeque;

enum Snapshot {
    Full(ImageData),
    Region { rect: Rect, pixels: Vec<u8> },
}

impl Snapshot {
    fn bytes(&self) -> usize {
        match self {
            Self::Full(img) => img.pixels.len(),
            Self::Region { pixels, .. } => pixels.len(),
        }
    }

    /// Restore this snapshot into `img`, returning the snapshot that undoes the restore.
    fn swap_into(self, img: &mut ImageData) -> Snapshot {
        match self {
            Self::Full(mut other) => {
                std::mem::swap(img, &mut other);
                Self::Full(other)
            }
            Self::Region { rect, pixels } => {
                let current = rect.extract(img);
                rect.write(img, &pixels);
                Self::Region { rect, pixels: current }
            }
        }
    }
}

struct Entry {
    label: String,
    snapshot: Snapshot,
}

/// Snapshot-based undo/redo stack for an image being edited.
///
/// Each edit stores the pixels it is about to overwrite: the whole image for
/// [`ImageEditHistory::apply`], or just a rectangle for [`ImageEditHistory::apply_in_region`]
/// (ideal for brush strokes). The oldest undo steps are evicted once the stored snapshots
/// exceed the memory cap or the step limit; the most recent step is always kept.
///
/// ```
/// use space3d_assets::edit::texture::{ImageEditHistory, invert};
/// use space3d_core::ImageData;
/// let mut h = ImageEditHistory::new(ImageData::filled(4, 4, [10, 20, 30, 255]));
/// h.apply("invert", invert);
/// assert_eq!(h.image().get(0, 0), [245, 235, 225, 255]);
/// h.undo();
/// assert_eq!(h.image().get(0, 0), [10, 20, 30, 255]);
/// ```
pub struct ImageEditHistory {
    image: ImageData,
    undo: VecDeque<Entry>,
    redo: Vec<Entry>,
    bytes: usize,
    memory_cap: usize,
    max_steps: usize,
}

impl ImageEditHistory {
    /// Default memory cap for stored snapshots: 256 MiB.
    pub const DEFAULT_MEMORY_CAP: usize = 256 << 20;
    /// Default maximum number of undo steps.
    pub const DEFAULT_MAX_STEPS: usize = 100;

    /// Start tracking `image`.
    pub fn new(image: ImageData) -> Self {
        Self {
            image,
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

    /// The current image.
    pub fn image(&self) -> &ImageData {
        &self.image
    }

    /// Mutable access *without* recording an undo step. Call
    /// [`ImageEditHistory::checkpoint`] first to make the following edits undoable.
    pub fn image_mut(&mut self) -> &mut ImageData {
        &mut self.image
    }

    /// Consume the history, returning the current image.
    pub fn into_image(self) -> ImageData {
        self.image
    }

    fn push(&mut self, label: String, snapshot: Snapshot) {
        for e in self.redo.drain(..) {
            self.bytes -= e.snapshot.bytes();
        }
        self.bytes += snapshot.bytes();
        self.undo.push_back(Entry { label, snapshot });
        self.enforce_limits();
    }

    fn enforce_limits(&mut self) {
        while self.undo.len() > 1 && (self.bytes > self.memory_cap || self.undo.len() > self.max_steps) {
            if let Some(e) = self.undo.pop_front() {
                self.bytes -= e.snapshot.bytes();
            }
        }
    }

    /// Record a full snapshot, then run `op` on the image.
    pub fn apply(&mut self, label: impl Into<String>, op: impl FnOnce(&mut ImageData)) {
        self.checkpoint(label);
        op(&mut self.image);
    }

    /// Record only `region`, then run `op`, which must not modify pixels outside `region`
    /// nor change the image size.
    ///
    /// # Panics
    /// If `op` changes the image dimensions.
    pub fn apply_in_region(&mut self, label: impl Into<String>, region: Rect, op: impl FnOnce(&mut ImageData)) {
        let rect = region.clamp_to(self.image.width, self.image.height);
        let dims = (self.image.width, self.image.height);
        let pixels = rect.extract(&self.image);
        self.push(label.into(), Snapshot::Region { rect, pixels });
        op(&mut self.image);
        assert_eq!(
            dims,
            (self.image.width, self.image.height),
            "apply_in_region must not resize the image"
        );
    }

    /// Record a full snapshot of the current image as an undo step; subsequent edits made
    /// through [`ImageEditHistory::image_mut`] are undone together (e.g. a whole stroke).
    pub fn checkpoint(&mut self, label: impl Into<String>) {
        self.push(label.into(), Snapshot::Full(self.image.clone()));
    }

    /// Undo the last step. Returns `false` if there was nothing to undo.
    pub fn undo(&mut self) -> bool {
        let Some(e) = self.undo.pop_back() else {
            return false;
        };
        self.bytes -= e.snapshot.bytes();
        let snapshot = e.snapshot.swap_into(&mut self.image);
        self.bytes += snapshot.bytes();
        self.redo.push(Entry {
            label: e.label,
            snapshot,
        });
        true
    }

    /// Redo the last undone step. Returns `false` if there was nothing to redo.
    pub fn redo(&mut self) -> bool {
        let Some(e) = self.redo.pop() else {
            return false;
        };
        self.bytes -= e.snapshot.bytes();
        let snapshot = e.snapshot.swap_into(&mut self.image);
        self.bytes += snapshot.bytes();
        self.undo.push_back(Entry {
            label: e.label,
            snapshot,
        });
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

    /// Label of the step [`ImageEditHistory::undo`] would revert.
    pub fn undo_label(&self) -> Option<&str> {
        self.undo.back().map(|e| e.label.as_str())
    }

    /// Label of the step [`ImageEditHistory::redo`] would re-apply.
    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|e| e.label.as_str())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::texture::{Brush, Filter, brush_dab, invert, resize, white_noise};

    #[test]
    fn undo_redo_roundtrip() {
        let original = white_noise(16, 16, 1);
        let mut h = ImageEditHistory::new(original.clone());
        h.apply("invert", invert);
        let inverted = h.image().clone();
        h.apply("resize", |img| *img = resize(img, 8, 8, Filter::Nearest));
        assert_eq!(h.image().width, 8);
        assert_eq!(h.undo_label(), Some("resize"));
        assert!(h.undo());
        assert_eq!(h.image(), &inverted);
        assert!(h.undo());
        assert_eq!(h.image(), &original);
        assert!(!h.undo());
        assert!(h.redo());
        assert!(h.redo());
        assert_eq!(h.image().width, 8);
        assert!(!h.redo());
        // A new edit clears the redo stack.
        h.undo();
        h.apply("invert again", invert);
        assert!(!h.can_redo());
    }

    #[test]
    fn region_snapshots_are_small_and_exact() {
        let original = white_noise(64, 64, 2);
        let mut h = ImageEditHistory::new(original.clone());
        let brush = Brush::hard([255, 0, 0, 255], 3.0);
        let region = brush.bounds(20.0, 20.0, 64, 64);
        h.apply_in_region("dab", region, |img| {
            brush_dab(img, 20.0, 20.0, &brush);
        });
        assert!(h.memory_usage() < 64 * 4 * 4 * 4);
        let painted = h.image().clone();
        assert_ne!(&painted, &original);
        h.undo();
        assert_eq!(h.image(), &original);
        h.redo();
        assert_eq!(h.image(), &painted);
    }

    #[test]
    fn memory_cap_evicts_oldest() {
        let mut h = ImageEditHistory::new(ImageData::new(8, 8)).with_memory_cap(8 * 8 * 4 * 2);
        for i in 0..5 {
            h.apply(format!("step {i}"), invert);
        }
        assert_eq!(h.undo_len(), 2);
        assert!(h.memory_usage() <= 8 * 8 * 4 * 2);
        let mut steps = ImageEditHistory::new(ImageData::new(2, 2)).with_max_steps(3);
        for _ in 0..10 {
            steps.checkpoint("x");
        }
        assert_eq!(steps.undo_len(), 3);
        steps.clear();
        assert_eq!(steps.memory_usage(), 0);
    }
}
