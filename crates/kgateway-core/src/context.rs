//! Request-scoped context — an owned struct threaded explicitly (no
//! RwLock-on-context needed — ownership gives safe mutation).

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::time::Instant;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(pub Uuid);

impl RequestId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Per-request state passed through the pipeline. Typed extensions (`ext`) play the
/// role of reserved context keys, but are type-safe.
pub struct Ctx {
    pub request_id: RequestId,
    pub virtual_key: Option<String>,
    pub attempt: u32,
    pub started_at: Instant,
    ext: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl Ctx {
    pub fn new() -> Self {
        Self {
            request_id: RequestId::new(),
            virtual_key: None,
            attempt: 0,
            started_at: Instant::now(),
            ext: HashMap::new(),
        }
    }

    /// Insert a typed extension value.
    pub fn insert<T: Any + Send + Sync>(&mut self, value: T) {
        self.ext.insert(TypeId::of::<T>(), Box::new(value));
    }

    /// Get a typed extension value.
    pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.ext
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref::<T>())
    }
}

impl Default for Ctx {
    fn default() -> Self {
        Self::new()
    }
}
