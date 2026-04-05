use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;

/// Error returned when a required state type is not registered.
#[derive(Debug)]
pub struct MissingStateError {
    type_name: &'static str,
}

impl fmt::Display for MissingStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "harrow: state type `{}` was not registered. \
             Call `App::state(value)` before serving.",
            self.type_name
        )
    }
}

impl std::error::Error for MissingStateError {}

impl crate::response::IntoResponse for MissingStateError {
    fn into_response(self) -> crate::response::Response {
        crate::response::Response::new(http::StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
    }
}

impl From<MissingStateError> for crate::response::Response {
    fn from(err: MissingStateError) -> Self {
        crate::response::IntoResponse::into_response(err)
    }
}

/// Error returned when a required per-request extension is not present.
#[derive(Debug)]
pub struct MissingExtError {
    pub(crate) type_name: &'static str,
}

impl fmt::Display for MissingExtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "harrow: per-request extension `{}` was not set by middleware.",
            self.type_name
        )
    }
}

impl std::error::Error for MissingExtError {}

impl crate::response::IntoResponse for MissingExtError {
    fn into_response(self) -> crate::response::Response {
        crate::response::Response::new(http::StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
    }
}

impl From<MissingExtError> for crate::response::Response {
    fn from(err: MissingExtError) -> Self {
        crate::response::IntoResponse::into_response(err)
    }
}

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
    /// Returns `Err(MissingStateError)` if `T` was not previously inserted.
    pub fn require<T: Send + Sync + 'static>(&self) -> Result<&T, MissingStateError> {
        self.try_get::<T>().ok_or(MissingStateError {
            type_name: std::any::type_name::<T>(),
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
    fn insert_and_require() {
        let mut map = TypeMap::new();
        map.insert(42u64);
        map.insert("hello".to_string());

        assert_eq!(*map.require::<u64>().unwrap(), 42);
        assert_eq!(map.require::<String>().unwrap(), "hello");
    }

    #[test]
    fn overwrite() {
        let mut map = TypeMap::new();
        map.insert(1u32);
        map.insert(2u32);
        assert_eq!(*map.require::<u32>().unwrap(), 2);
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

    #[test]
    fn require_present_returns_ok() {
        let mut map = TypeMap::new();
        map.insert(42u64);
        assert_eq!(*map.require::<u64>().unwrap(), 42);
    }

    #[test]
    fn require_missing_returns_err() {
        let map = TypeMap::new();
        let err = map.require::<u64>().unwrap_err();
        assert!(err.to_string().contains("was not registered"));
    }
}
