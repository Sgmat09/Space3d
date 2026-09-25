//! Generational handles and the slot map that backs them.
//!
//! Handles are small `Copy` ids (`index` + `generation`). Removing an entry bumps the slot's
//! generation, so stale handles are detected instead of silently aliasing a new asset.

use std::collections::HashMap;
use std::fmt;
use std::hash::BuildHasherDefault;
use std::marker::PhantomData;

/// Fast, deterministic (fixed-key) hash map used for internal caches.
pub(crate) type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<ahash::AHasher>>;

macro_rules! define_handle {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name {
            index: u32,
            generation: u32,
        }

        impl $name {
            /// Slot index (stable while the handle is alive; reused after removal).
            pub fn index(self) -> u32 {
                self.index
            }
            /// Generation counter of the slot at creation time.
            pub fn generation(self) -> u32 {
                self.generation
            }
            pub(crate) fn from_raw(raw: RawHandle) -> Self {
                Self { index: raw.index, generation: raw.generation }
            }
            pub(crate) fn raw(self) -> RawHandle {
                RawHandle { index: self.index, generation: self.generation }
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({}v{})", stringify!($name), self.index, self.generation)
            }
        }
    };
}

define_handle!(
    /// Handle to a mesh stored in [`crate::RenderAssets`].
    MeshHandle
);
define_handle!(
    /// Handle to a texture stored in [`crate::RenderAssets`].
    TextureHandle
);
define_handle!(
    /// Handle to a type-erased instance buffer stored in [`crate::InstanceBuffers`].
    InstanceBufferHandle
);
define_handle!(
    /// Handle to a [`crate::CustomShader`] stored in [`crate::RenderAssets`].
    ShaderHandle
);

/// Untyped generational id used internally by [`SlotMap`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RawHandle {
    pub index: u32,
    pub generation: u32,
}

struct Slot<T> {
    generation: u32,
    value: Option<T>,
}

/// Dense generational arena with a free list. `O(1)` insert/get/remove.
pub(crate) struct SlotMap<T> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
    len: usize,
    _marker: PhantomData<T>,
}

impl<T> Default for SlotMap<T> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            len: 0,
            _marker: PhantomData,
        }
    }
}

impl<T> SlotMap<T> {
    pub fn insert(&mut self, value: T) -> RawHandle {
        self.len += 1;
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.value = Some(value);
            RawHandle {
                index,
                generation: slot.generation,
            }
        } else {
            let index = self.slots.len() as u32;
            self.slots.push(Slot {
                generation: 0,
                value: Some(value),
            });
            RawHandle { index, generation: 0 }
        }
    }

    pub fn get(&self, h: RawHandle) -> Option<&T> {
        self.slots
            .get(h.index as usize)
            .filter(|s| s.generation == h.generation)
            .and_then(|s| s.value.as_ref())
    }

    pub fn get_mut(&mut self, h: RawHandle) -> Option<&mut T> {
        self.slots
            .get_mut(h.index as usize)
            .filter(|s| s.generation == h.generation)
            .and_then(|s| s.value.as_mut())
    }

    pub fn remove(&mut self, h: RawHandle) -> Option<T> {
        let slot = self.slots.get_mut(h.index as usize)?;
        if slot.generation != h.generation || slot.value.is_none() {
            return None;
        }
        slot.generation = slot.generation.wrapping_add(1);
        self.free.push(h.index);
        self.len -= 1;
        slot.value.take()
    }

    pub fn len(&self) -> usize {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_handles_are_rejected() {
        let mut m = SlotMap::default();
        let a = m.insert(1);
        assert_eq!(m.get(a), Some(&1));
        assert_eq!(m.remove(a), Some(1));
        let b = m.insert(2);
        assert_eq!(a.index, b.index, "slot is reused");
        assert_ne!(a.generation, b.generation);
        assert_eq!(m.get(a), None);
        assert_eq!(m.get(b), Some(&2));
        assert_eq!(m.remove(a), None);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn typed_handles_roundtrip() {
        let h = MeshHandle::from_raw(RawHandle {
            index: 3,
            generation: 7,
        });
        assert_eq!((h.index(), h.generation()), (3, 7));
        assert_eq!(format!("{h:?}"), "MeshHandle(3v7)");
    }
}
