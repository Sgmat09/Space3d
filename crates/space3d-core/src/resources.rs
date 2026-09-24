use std::any::{Any, TypeId, type_name};
use std::collections::HashMap;
use std::hash::BuildHasherDefault;

/// Type-keyed singleton storage (one value per type).
#[derive(Default)]
pub struct Resources {
    map: HashMap<TypeId, Box<dyn Any + Send + Sync>, BuildHasherDefault<ahash::AHasher>>,
}

impl Resources {
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) -> Option<T> {
        self.map
            .insert(TypeId::of::<T>(), Box::new(value))
            .and_then(|b| b.downcast().ok().map(|b| *b))
    }

    pub fn init<T: Default + Send + Sync + 'static>(&mut self) -> &mut T {
        self.map
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(T::default()))
            .downcast_mut()
            .expect("resource type mismatch")
    }

    pub fn remove<T: 'static>(&mut self) -> Option<T> {
        self.map
            .remove(&TypeId::of::<T>())
            .and_then(|b| b.downcast().ok().map(|b| *b))
    }

    pub fn contains<T: 'static>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }

    pub fn get<T: 'static>(&self) -> Option<&T> {
        self.map.get(&TypeId::of::<T>()).and_then(|b| b.downcast_ref())
    }

    pub fn get_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.map.get_mut(&TypeId::of::<T>()).and_then(|b| b.downcast_mut())
    }

    /// Panics with the type name if missing.
    pub fn expect<T: 'static>(&self) -> &T {
        self.get()
            .unwrap_or_else(|| panic!("missing resource {}", type_name::<T>()))
    }

    /// Panics with the type name if missing.
    pub fn expect_mut<T: 'static>(&mut self) -> &mut T {
        self.get_mut()
            .unwrap_or_else(|| panic!("missing resource {}", type_name::<T>()))
    }

    /// Temporarily take a resource out so it can be used alongside `&mut self`.
    pub fn scope<T: Send + Sync + 'static, R>(&mut self, f: impl FnOnce(&mut Self, &mut T) -> R) -> R {
        // Move the existing Box out and back in: no reallocation per call (hot path for
        // per-frame systems such as the renderer).
        let boxed = self
            .map
            .remove(&TypeId::of::<T>())
            .unwrap_or_else(|| panic!("missing resource {}", type_name::<T>()));
        let mut boxed: Box<T> = boxed.downcast().expect("resource type mismatch");
        let out = f(self, &mut boxed);
        self.map.insert(TypeId::of::<T>(), boxed);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn insert_get_scope() {
        let mut r = Resources::default();
        r.insert(5u32);
        r.insert(String::from("a"));
        *r.expect_mut::<u32>() += 1;
        r.scope::<String, _>(|r, s| s.push_str(&r.expect::<u32>().to_string()));
        assert_eq!(r.expect::<String>(), "a6");
        assert_eq!(r.remove::<u32>(), Some(6));
        assert!(!r.contains::<u32>());
    }

    #[test]
    fn scope_keeps_the_same_allocation() {
        let mut r = Resources::default();
        r.insert([0u8; 4096]);
        let before = r.expect::<[u8; 4096]>().as_ptr();
        r.scope::<[u8; 4096], _>(|_, v| v[0] = 1);
        assert_eq!(r.expect::<[u8; 4096]>().as_ptr(), before);
        assert_eq!(r.expect::<[u8; 4096]>()[0], 1);
    }
}
