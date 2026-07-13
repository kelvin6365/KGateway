//! kgateway-plugins — built-in plugins. M3+ adds logging, telemetry, governance,
//! semantic cache. This scaffold ships a no-op example so the trait wiring compiles
//! and the engine's plugin path is exercised in tests. See `docs/04-plugins.md`.

use async_trait::async_trait;
use kgateway_core::context::Ctx;
use kgateway_core::error::KgError;
use kgateway_core::plugin::{LlmPlugin, Plugin, PreOutcome};
use kgateway_core::schema::{ChatRequest, ChatResponse};

pub mod governance;
pub mod logging;
pub mod pricing;
pub mod redaction;
pub mod semantic_cache;

pub use governance::{GovernancePlugin, VirtualKey};
pub use logging::{run_log_writer, LoggingPlugin};
pub use semantic_cache::{Embedder, ProviderEmbedder, SemanticCachePlugin};

/// A plugin that does nothing but log its name — placeholder proving the pipeline.
pub struct NoopPlugin;

impl Plugin for NoopPlugin {
    fn name(&self) -> &str {
        "noop"
    }
}

#[async_trait]
impl LlmPlugin for NoopPlugin {
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
