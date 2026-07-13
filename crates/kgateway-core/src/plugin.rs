//! Capability-segmented plugin system. Small role traits composing a `Plugin` base.
//! See `docs/04-plugins.md`.

use crate::context::Ctx;
use crate::error::KgError;
use crate::schema::{ChatRequest, ChatResponse};
use async_trait::async_trait;

/// Base plugin.
pub trait Plugin: Send + Sync {
    fn name(&self) -> &str;
}

/// Outcome of a `pre_llm` hook. A short-circuit is a **typed value**, never a plugin
/// `Err` (those are non-blocking and only logged).
// `Continue` carries a full `ChatRequest` by value on the common path; boxing it (as clippy
// suggests) would add a heap allocation to every request's pre_llm outcome to shrink a
// stack-only enum. Not worth it on the hot path.
#[allow(clippy::large_enum_variant)]
pub enum PreOutcome {
    /// Proceed with the (possibly mutated) request.
    Continue(ChatRequest),
    /// Skip the provider call and remaining pre-hooks; return this success response
    /// (e.g. a cache hit). Yields HTTP 200.
    ShortCircuit(ChatResponse),
    /// Reject the request with this error (e.g. auth failure, rate-limit or budget
    /// exceeded). Yields the error's HTTP status. Post-hooks still run on the error.
    Reject(KgError),
}

/// Core LLM-layer plugin.
///
/// Semantics:
/// - `pre_request` runs ONCE per top-level request; mutations are committed and seen
///   by every fallback attempt. Errors are non-blocking (logged, cannot abort).
/// - `pre_llm` runs PER attempt; may short-circuit. Errors non-blocking (logged; the
///   engine continues with the request unchanged).
/// - `post_llm` runs in REVERSE (LIFO) order relative to `pre_llm`. Its returned `Ok`
///   value REPLACES the pipeline result (so a content filter transforms via `Ok(...)`);
///   an `Err` is treated as plugin-internal failure — logged, and the PRIOR result is
///   kept (a failing telemetry plugin must not turn a good response into an error).
#[async_trait]
pub trait LlmPlugin: Plugin {
    async fn pre_request(&self, _ctx: &Ctx, _req: &mut ChatRequest) -> Result<(), KgError> {
        Ok(())
    }

    async fn pre_llm(&self, _ctx: &Ctx, req: ChatRequest) -> Result<PreOutcome, KgError> {
        Ok(PreOutcome::Continue(req))
    }

    async fn post_llm(
        &self,
        _ctx: &Ctx,
        resp: Result<ChatResponse, KgError>,
    ) -> Result<ChatResponse, KgError> {
        resp
    }
}
