//! Request-scoped context — an owned struct threaded explicitly (no
//! RwLock-on-context needed — ownership gives safe mutation).

use crate::trace::{SpanCategory, SpanCollector};
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
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
    /// Client-supplied session identifier, when present, used to group a working
    /// session's many calls into one journey (e.g. a Claude Code CLI run). Resolved at
    /// ingress from the `x-session-id` header, or derived from the request body's user
    /// hint (OpenAI `user` / Anthropic `metadata.user_id`). Unlike `virtual_key` (one
    /// key = every session of its holder) this distinguishes individual sessions. `None`
    /// when the caller sends no hint.
    pub session_id: Option<String>,
    pub attempt: u32,
    pub started_at: Instant,
    /// Trace spans for this request's waterfall. Behind a mutex because most of
    /// the pipeline holds `&Ctx`, not `&mut Ctx`; behind an `Arc` because a
    /// streamed response outlives the borrow — the deferred capture guard keeps
    /// a handle and emits the audit record after the stream ends.
    pub spans: Arc<SpanCollector>,
    ext: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl Ctx {
    pub fn new() -> Self {
        Self {
            request_id: RequestId::new(),
            virtual_key: None,
            session_id: None,
            attempt: 0,
            started_at: Instant::now(),
            spans: Arc::new(SpanCollector::new()),
            ext: HashMap::new(),
        }
    }

    /// Time a stage and record it as a trace span. Returns the closure's value,
    /// so an existing call can be wrapped without restructuring it:
    ///
    /// ```ignore
    /// let outcome = ctx.timed("route.resolve", SpanCategory::Gateway, 1, || {
    ///     router.resolve(&req)
    /// });
    /// ```
    pub fn timed<T>(
        &self,
        name: &str,
        category: SpanCategory,
        depth: u8,
        f: impl FnOnce() -> T,
    ) -> T {
        let at = Instant::now();
        let out = f();
        self.spans
            .record(self.started_at, at, at.elapsed(), name, category, depth);
        out
    }

    /// Record a stage that was timed manually — for async work, where a closure
    /// can't span the await, and for spans that need an outcome chip or detail.
    #[allow(clippy::too_many_arguments)]
    pub fn span_at(
        &self,
        at: Instant,
        dur: Duration,
        name: impl Into<String>,
        category: SpanCategory,
        depth: u8,
        outcome: Option<String>,
        detail: Option<String>,
    ) {
        self.spans.record_detailed(
            self.started_at,
            at,
            dur,
            name,
            category,
            depth,
            outcome,
            detail,
        );
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
