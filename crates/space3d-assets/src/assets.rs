//! Typed asset storage and change events.

use crate::handle::{AssetId, AssetIndex, Handle, HandleProvider};
use std::any::TypeId;
use std::fmt;
use std::sync::Arc;

/// Marker for types that can be stored in [`Assets`]. Implemented for every
/// `Send + Sync + 'static` type.
pub trait Asset: Send + Sync + 'static {}
impl<T: Send + Sync + 'static> Asset for T {}

/// Change notification for assets of type `T`, delivered through
/// `space3d_core::Events<AssetEvent<T>>` (flushed in `PreUpdate` and `PostUpdate`).
pub enum AssetEvent<T> {
    /// Data became available for the first time (load finished or [`Assets::add`]).
    Created {
        /// Affected asset.
        id: AssetId<T>,
    },
    /// Data was replaced (hot reload, [`Assets::insert`]) or borrowed mutably.
    Modified {
        /// Affected asset.
        id: AssetId<T>,
    },
    /// Data was removed (last strong handle dropped or [`Assets::remove`]).
    Removed {
        /// Affected asset.
        id: AssetId<T>,
    },
}

impl<T> AssetEvent<T> {
    /// The asset this event is about.
    pub fn id(&self) -> AssetId<T> {
        match self {
            Self::Created { id } | Self::Modified { id } | Self::Removed { id } => *id,
        }
    }
    /// `Created` for `id`?
    pub fn is_created(&self, id: impl Into<AssetId<T>>) -> bool {
        matches!(self, Self::Created { id: i } if *i == id.into())
    }
    /// `Modified` for `id`?
    pub fn is_modified(&self, id: impl Into<AssetId<T>>) -> bool {
        matches!(self, Self::Modified { id: i } if *i == id.into())
    }
    /// `Removed` for `id`?
    pub fn is_removed(&self, id: impl Into<AssetId<T>>) -> bool {
        matches!(self, Self::Removed { id: i } if *i == id.into())
    }
}

impl<T> Clone for AssetEvent<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for AssetEvent<T> {}
impl<T> PartialEq for AssetEvent<T> {
    fn eq(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other) && self.id() == other.id()
    }
}
impl<T> Eq for AssetEvent<T> {}
impl<T> fmt::Debug for AssetEvent<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::Created { .. } => "Created",
            Self::Modified { .. } => "Modified",
            Self::Removed { .. } => "Removed",
        };
        write!(f, "AssetEvent::{kind}({:?})", self.id())
    }
}

/// Storage resource for all loaded/created assets of type `T`.
///
/// Registered by [`crate::AssetApp::init_asset`]. Look assets up with a [`Handle`] or
/// [`AssetId`]; data appears asynchronously once the [`crate::AssetServer`] finishes loading.
pub struct Assets<T: Asset> {
    slots: Vec<Option<(u32, T)>>,
    len: usize,
    provider: Arc<HandleProvider>,
    queued: Vec<AssetEvent<T>>,
}

impl<T: Asset> Default for Assets<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Asset> Assets<T> {
    /// Empty storage with its own id allocator.
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            len: 0,
            provider: Arc::new(HandleProvider::new(TypeId::of::<T>())),
            queued: Vec::new(),
        }
    }

    pub(crate) fn provider(&self) -> &Arc<HandleProvider> {
        &self.provider
    }

    /// Store `value` and return a strong handle to it. Emits `Created`.
    pub fn add(&mut self, value: T) -> Handle<T> {
        let handle = self.reserve_handle();
        self.insert_at(handle.id().index(), value);
        handle
    }

    /// Allocate a strong handle whose data will be provided later via [`Assets::insert`].
    pub fn reserve_handle(&self) -> Handle<T> {
        Handle::from_strong(self.provider.strong(self.provider.reserve()))
    }

    /// Store (or replace) the data for a live id. Emits `Created` or `Modified`.
    /// Returns `false` (dropping `value`) if the id is stale.
    pub fn insert(&mut self, id: impl Into<AssetId<T>>, value: T) -> bool {
        let idx = id.into().index();
        if !self.provider.is_alive(idx) {
            return false;
        }
        self.insert_at(idx, value);
        true
    }

    fn insert_at(&mut self, idx: AssetIndex, value: T) {
        let i = idx.index as usize;
        if self.slots.len() <= i {
            self.slots.resize_with(i + 1, || None);
        }
        let id = AssetId::from_index(idx);
        let replaced = matches!(&self.slots[i], Some((g, _)) if *g == idx.generation);
        if !replaced && self.slots[i].is_none() {
            self.len += 1;
        }
        self.slots[i] = Some((idx.generation, value));
        self.queued.push(if replaced {
            AssetEvent::Modified { id }
        } else {
            AssetEvent::Created { id }
        });
    }

    /// Borrow the data, if loaded.
    pub fn get(&self, id: impl Into<AssetId<T>>) -> Option<&T> {
        let idx = id.into().index();
        match self.slots.get(idx.index as usize)? {
            Some((g, v)) if *g == idx.generation => Some(v),
            _ => None,
        }
    }

    /// Mutably borrow the data, if loaded. Emits `Modified`.
    pub fn get_mut(&mut self, id: impl Into<AssetId<T>>) -> Option<&mut T> {
        let id = id.into();
        let idx = id.index();
        match self.slots.get_mut(idx.index as usize)? {
            Some((g, v)) if *g == idx.generation => {
                self.queued.push(AssetEvent::Modified { id });
                Some(v)
            }
            _ => None,
        }
    }

    /// Remove the data (handles stay valid but resolve to `None`). Emits `Removed`.
    pub fn remove(&mut self, id: impl Into<AssetId<T>>) -> Option<T> {
        let id = id.into();
        let idx = id.index();
        let slot = self.slots.get_mut(idx.index as usize)?;
        if !matches!(slot, Some((g, _)) if *g == idx.generation) {
            return None;
        }
        let (_, v) = slot.take()?;
        self.len -= 1;
        self.queued.push(AssetEvent::Removed { id });
        Some(v)
    }

    /// Whether data for `id` is present.
    pub fn contains(&self, id: impl Into<AssetId<T>>) -> bool {
        self.get(id).is_some()
    }

    /// Number of assets with data.
    pub fn len(&self) -> usize {
        self.len
    }

    /// No assets with data?
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Iterate `(id, data)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (AssetId<T>, &T)> {
        self.slots.iter().enumerate().filter_map(|(i, s)| {
            s.as_ref().map(|(g, v)| {
                (
                    AssetId::from_index(AssetIndex {
                        index: i as u32,
                        generation: *g,
                    }),
                    v,
                )
            })
        })
    }

    /// Ids of all assets with data.
    pub fn ids(&self) -> impl Iterator<Item = AssetId<T>> + '_ {
        self.iter().map(|(id, _)| id)
    }

    /// Release slots whose last strong handle was dropped. Returns the freed indices.
    pub(crate) fn release_dropped(&mut self) -> Vec<AssetIndex> {
        let dropped = self.provider.take_dropped();
        let mut freed = Vec::with_capacity(dropped.len());
        for idx in dropped {
            self.remove(AssetId::from_index(idx));
            if self.provider.free(idx) {
                freed.push(idx);
            }
        }
        freed
    }

    pub(crate) fn take_events(&mut self) -> Vec<AssetEvent<T>> {
        std::mem::take(&mut self.queued)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_get_remove_and_events() {
        let mut a = Assets::<String>::new();
        let h = a.add("hi".into());
        assert_eq!(a.get(&h).map(String::as_str), Some("hi"));
        a.get_mut(&h).unwrap().push('!');
        assert!(a.insert(&h, "yo".into()));
        assert_eq!(a.len(), 1);
        assert_eq!(a.remove(&h).as_deref(), Some("yo"));
        assert!(a.is_empty());
        let ev = a.take_events();
        assert!(ev[0].is_created(&h));
        assert!(ev[1].is_modified(&h));
        assert!(ev[2].is_modified(&h));
        assert!(ev[3].is_removed(&h));
    }

    #[test]
    fn dropping_handles_frees_slot() {
        let mut a = Assets::<u32>::new();
        let h = a.add(5);
        let id = h.id();
        drop(h);
        assert_eq!(a.release_dropped(), vec![id.index()]);
        assert!(a.get(id).is_none());
        assert!(!a.insert(id, 1), "stale ids are rejected");
        let h2 = a.add(6);
        assert_eq!(h2.id().index().index, id.index().index);
        assert_eq!(a.get(id), None);
        assert_eq!(a.get(&h2), Some(&6));
    }
}
