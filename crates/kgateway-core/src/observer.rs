//! Request observers — capability-generic cross-cutting concerns (governance, audit
//! logging, telemetry) that run on EVERY engine method (chat, embeddings, images,
//! audio, rerank), not just the chat pipeline.
//!
//! This exists because `LlmPlugin` operates on `ChatRequest`/`ChatResponse` and so can
//! only run on chat. Enforcement (auth / rate limits / budgets) and audit must apply to
//! all capabilities — a rate-limited virtual key must not bypass its limit via
//! `/v1/embeddings`. Observers close that gap.

use crate::context::Ctx;
use crate::error::KgError;
use async_trait::async_trait;

/// A record of one completed (or failed) upstream call, passed to `on_response`.
#[derive(Debug, Clone, Default)]
pub struct CallRecord {
    pub provider: String,
    /// Bare model id (provider prefix stripped).
    pub model: String,
    pub status: u16,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Whether this was a streaming request.
    pub stream: bool,
    /// Whether the response was served from the semantic cache (no upstream call).
    pub cache_hit: bool,
    /// The upstream finish/stop reason, if any (e.g. "stop", "length", "tool_calls").
    pub stop_reason: Option<String>,
    /// The error detail for a failed call (server-side audit only — never sent to
    /// clients, which get a scrubbed message).
    pub error_message: Option<String>,
    /// Captured request payload as (truncated) JSON — only populated when content
    /// capture is enabled (M10 Phase 2). Off by default; admin-only on read.
    pub request_body: Option<String>,
    /// Captured response payload as (truncated) JSON — only populated when content
    /// capture is enabled. Empty for error/binary/vector responses per the capability
    /// capture matrix (see `docs/12-content-capture-plan.md`).
    pub response_body: Option<String>,
}

impl CallRecord {
    /// Minimal constructor for a chat/capability outcome; extra fields default off.
    pub fn new(provider: String, model: String, status: u16, prompt: u32, completion: u32) -> Self {
        Self {
            provider,
            model,
            status,
            prompt_tokens: prompt,
            completion_tokens: completion,
            ..Default::default()
        }
    }

    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens as u64 + self.completion_tokens as u64
    }
}

/// Cross-cutting hook run on every capability request.
#[async_trait]
pub trait RequestObserver: Send + Sync {
    fn name(&self) -> &str;

    /// Pre-flight check keyed on the virtual key (`ctx.virtual_key`) and the requested
    /// model (full `provider/model` string). Return `Err` to REJECT the request (e.g.
    /// auth failure, rate-limit or budget exceeded, model not allowed). The engine maps
    /// the error to its HTTP status and does not dispatch.
    async fn on_request(&self, _ctx: &Ctx, _model: &str) -> Result<(), KgError> {
        Ok(())
    }

    /// Post-flight record of the outcome (audit log, token accounting, metrics). Never
    /// fails the request — observers must swallow their own errors.
    async fn on_response(&self, _ctx: &Ctx, _record: &CallRecord) {}
}
