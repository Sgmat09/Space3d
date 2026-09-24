//! Typed generational handles with reference counting.

use parking_lot::Mutex;
use std::any::TypeId;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::sync::Arc;

/// Untyped generational slot index. A freed slot bumps its generation, so stale ids never
/// alias a newer asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AssetIndex {
    /// Slot number.
    pub index: u32,
    /// Generation of the slot when this id was issued.
    pub generation: u32,
}

impl AssetIndex {
    /// An index that never refers to a live asset.
    pub const INVALID: Self = Self {
        index: u32::MAX,
        generation: u32::MAX,
    };
}

/// Copyable typed id of an asset in [`crate::Assets<T>`]. Does not keep the asset alive.
pub struct AssetId<T> {
    index: AssetIndex,
    marker: PhantomData<fn() -> T>,
}

impl<T> AssetId<T> {
    /// An id that never refers to a live asset.
    pub const INVALID: Self = Self::from_index(AssetIndex::INVALID);

    /// Wrap a raw index. Mostly useful for serialization/tools.
    pub const fn from_index(index: AssetIndex) -> Self {
        Self {
            index,
            marker: PhantomData,
        }
    }

    /// The raw generational index.
    pub fn index(self) -> AssetIndex {
        self.index
    }

    /// Erase the type.
    pub fn untyped(self) -> UntypedAssetId
    where
        T: 'static,
    {
        UntypedAssetId {
            type_id: TypeId::of::<T>(),
            index: self.index,
        }
    }
}

impl<T> Clone for AssetId<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for AssetId<T> {}
impl<T> PartialEq for AssetId<T> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}
impl<T> Eq for AssetId<T> {}
impl<T> Hash for AssetId<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.index.hash(state)
    }
}
impl<T> PartialOrd for AssetId<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for AssetId<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.index.cmp(&other.index)
    }
}
impl<T> fmt::Debug for AssetId<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "AssetId<{}>({}v{})",
            short_type_name::<T>(),
            self.index.index,
            self.index.generation
        )
    }
}
impl<T> Default for AssetId<T> {
    fn default() -> Self {
        Self::INVALID
    }
}

/// Asset id with its type erased (type is kept as a [`TypeId`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UntypedAssetId {
    /// `TypeId` of the asset type.
    pub type_id: TypeId,
    /// Slot index in that type's [`crate::Assets`].
    pub index: AssetIndex,
}

impl UntypedAssetId {
    /// Recover the typed id if `T` matches.
    pub fn typed<T: 'static>(self) -> Option<AssetId<T>> {
        (self.type_id == TypeId::of::<T>()).then(|| AssetId::from_index(self.index))
    }
}

impl<T: 'static> From<AssetId<T>> for UntypedAssetId {
    fn from(id: AssetId<T>) -> Self {
        id.untyped()
    }
}
impl<T: 'static> From<&Handle<T>> for UntypedAssetId {
    fn from(h: &Handle<T>) -> Self {
        h.id().untyped()
    }
}

#[derive(Default)]
struct Allocator {
    generations: Vec<u32>,
    live: Vec<bool>,
    free: Vec<u32>,
}

/// Per-type id allocator + drop queue shared by [`crate::Assets<T>`], the server and every
/// strong handle of that type.
pub(crate) struct HandleProvider {
    pub(crate) type_id: TypeId,
    alloc: Mutex<Allocator>,
    dropped: Mutex<Vec<AssetIndex>>,
}

impl HandleProvider {
    pub(crate) fn new(type_id: TypeId) -> Self {
        Self {
            type_id,
            alloc: Mutex::default(),
            dropped: Mutex::default(),
        }
    }

    pub(crate) fn reserve(&self) -> AssetIndex {
        let mut a = self.alloc.lock();
        if let Some(index) = a.free.pop() {
            a.live[index as usize] = true;
            AssetIndex {
                index,
                generation: a.generations[index as usize],
            }
        } else {
            let index = a.generations.len() as u32;
            a.generations.push(0);
            a.live.push(true);
            AssetIndex { index, generation: 0 }
        }
    }

    /// Release a slot. Returns false if it was already freed / stale.
    pub(crate) fn free(&self, idx: AssetIndex) -> bool {
        let mut a = self.alloc.lock();
        let i = idx.index as usize;
        if a.generations.get(i) == Some(&idx.generation) && a.live[i] {
            a.generations[i] = a.generations[i].wrapping_add(1);
            a.live[i] = false;
            a.free.push(idx.index);
            true
        } else {
            false
        }
    }

    pub(crate) fn is_alive(&self, idx: AssetIndex) -> bool {
        let a = self.alloc.lock();
        let i = idx.index as usize;
        a.generations.get(i) == Some(&idx.generation) && a.live[i]
    }

    pub(crate) fn take_dropped(&self) -> Vec<AssetIndex> {
        std::mem::take(&mut *self.dropped.lock())
    }

    pub(crate) fn strong(self: &Arc<Self>, index: AssetIndex) -> Arc<StrongHandle> {
        Arc::new(StrongHandle {
            index,
            provider: self.clone(),
        })
    }
}

/// Shared ref-count token; when the last clone drops, the slot is queued for release.
pub(crate) struct StrongHandle {
    pub(crate) index: AssetIndex,
    provider: Arc<HandleProvider>,
}

impl Drop for StrongHandle {
    fn drop(&mut self) {
        self.provider.dropped.lock().push(self.index);
    }
}

/// Handle to an asset of type `T` stored in [`crate::Assets<T>`].
///
/// *Strong* handles (returned by [`crate::AssetServer::load`] and [`crate::Assets::add`]) are
/// reference counted: once every strong clone is dropped the asset is removed and an
/// [`crate::AssetEvent::Removed`] is emitted. *Weak* handles ([`Handle::clone_weak`],
/// [`Handle::weak`]) only name the asset.
pub struct Handle<T> {
    id: AssetId<T>,
    strong: Option<Arc<StrongHandle>>,
}

impl<T> Handle<T> {
    pub(crate) fn from_strong(strong: Arc<StrongHandle>) -> Self {
        Self {
            id: AssetId::from_index(strong.index),
            strong: Some(strong),
        }
    }

    /// A weak handle to `id`.
    pub const fn weak(id: AssetId<T>) -> Self {
        Self { id, strong: None }
    }

    /// The asset id.
    pub fn id(&self) -> AssetId<T> {
        self.id
    }

    /// Whether this handle keeps the asset alive.
    pub fn is_strong(&self) -> bool {
        self.strong.is_some()
    }

    /// Whether this handle is weak.
    pub fn is_weak(&self) -> bool {
        self.strong.is_none()
    }

    /// A weak handle to the same asset.
    pub fn clone_weak(&self) -> Self {
        Self::weak(self.id)
    }

    /// Number of strong handles alive for this asset (0 for weak handles).
    pub fn strong_count(&self) -> usize {
        self.strong.as_ref().map_or(0, Arc::strong_count)
    }
}

impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            strong: self.strong.clone(),
        }
    }
}
impl<T> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl<T> Eq for Handle<T> {}
impl<T> Hash for Handle<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state)
    }
}
impl<T> fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = if self.is_strong() { "Strong" } else { "Weak" };
        write!(f, "Handle::{kind}({:?})", self.id)
    }
}
impl<T> Default for Handle<T> {
    /// A weak handle to [`AssetId::INVALID`].
    fn default() -> Self {
        Self::weak(AssetId::INVALID)
    }
}
impl<T> From<&Handle<T>> for AssetId<T> {
    fn from(h: &Handle<T>) -> Self {
        h.id
    }
}
impl<T> From<Handle<T>> for AssetId<T> {
    fn from(h: Handle<T>) -> Self {
        h.id
    }
}

fn short_type_name<T>() -> &'static str {
    let full = std::any::type_name::<T>();
    full.rsplit("::").next().unwrap_or(full)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generations_prevent_aliasing() {
        let p = Arc::new(HandleProvider::new(TypeId::of::<u32>()));
        let a = p.reserve();
        assert!(p.is_alive(a));
        assert!(p.free(a));
        assert!(!p.free(a));
        let b = p.reserve();
        assert_eq!(a.index, b.index);
        assert_ne!(a.generation, b.generation);
        assert!(!p.is_alive(a));
        assert!(p.is_alive(b));
    }

    #[test]
    fn dropping_last_strong_queues_release() {
        let p = Arc::new(HandleProvider::new(TypeId::of::<u32>()));
        let h: Handle<u32> = Handle::from_strong(p.strong(p.reserve()));
        let h2 = h.clone();
        let weak = h.clone_weak();
        assert_eq!(h.strong_count(), 2);
        drop(h);
        assert!(p.take_dropped().is_empty());
        drop(h2);
        assert_eq!(p.take_dropped(), vec![weak.id().index()]);
        assert!(weak.is_weak());
    }
}
