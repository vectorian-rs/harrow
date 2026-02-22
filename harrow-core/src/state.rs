use std::any::{Any, TypeId};
use std::collections::HashMap;

/// A type-erased map keyed by `TypeId`.
/// Used for application state injection.
#[derive(Default)]
pub struct TypeMap {
    inner: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl TypeMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a value of type `T`. Overwrites any previous value of the same type.
    pub fn insert<T: Send + Sync + 'static>(&mut self, val: T) {
        self.inner.insert(TypeId::of::<T>(), Box::new(val));
    }

    /// Try to retrieve a reference to the value of type `T`.
    /// Returns `None` if `T` was not previously inserted.
    pub fn try_get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.inner
            .get(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref::<T>())
    }

    /// Retrieve a reference to the value of type `T`.
    /// Panics if `T` was not previously inserted — this is a programmer error.
    pub fn get<T: Send + Sync + 'static>(&self) -> &T {
        self.inner
            .get(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref::<T>())
            .unwrap_or_else(|| {
                panic!(
                    "harrow: state type `{}` was not registered. \
                     Call `App::state(value)` before serving.",
                    std::any::type_name::<T>()
                )
            })
    }

    /// Check whether a value of type `T` has been inserted.
    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.inner.contains_key(&TypeId::of::<T>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_get() {
        let mut map = TypeMap::new();
        map.insert(42u64);
        map.insert("hello".to_string());

        assert_eq!(*map.get::<u64>(), 42);
        assert_eq!(map.get::<String>(), "hello");
    }

    #[test]
    #[should_panic(expected = "was not registered")]
    fn get_missing_panics() {
        let map = TypeMap::new();
        let _ = map.get::<u64>();
    }

    #[test]
    fn overwrite() {
        let mut map = TypeMap::new();
        map.insert(1u32);
        map.insert(2u32);
        assert_eq!(*map.get::<u32>(), 2);
    }

    #[test]
    fn try_get_missing_returns_none() {
        let map = TypeMap::new();
        assert!(map.try_get::<u64>().is_none());
    }

    #[test]
    fn try_get_present_returns_some() {
        let mut map = TypeMap::new();
        map.insert(42u64);
        assert_eq!(map.try_get::<u64>(), Some(&42u64));
    }
}
