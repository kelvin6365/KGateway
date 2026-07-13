//! Governance: virtual keys with model allow/deny-lists, rate limits, and token/cost
//! budgets. Implemented as a [`RequestObserver`] so enforcement runs on EVERY capability
//! (chat, embeddings, images, audio, rerank) — not just chat. Checks in `on_request`
//! (rejecting via `Err`), usage accounting in `on_response`.
//!
//! Counters live behind a [`GovernanceStore`]: the default in-process store is correct for a
//! single replica; a shared (Postgres) store keeps limits correct across horizontally-scaled
//! replicas. The virtual key is taken from `ctx.virtual_key` (the HTTP transport sets it from
//! the `Authorization: Bearer <vk>` header).

use async_trait::async_trait;
use kgateway_core::context::Ctx;
use kgateway_core::error::{KgError, KgErrorKind};
use kgateway_core::observer::{CallRecord, RequestObserver};
use kgateway_store::{GovernanceStore, InMemoryGovernanceStore};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Fixed rate-limit window: `max_requests_per_min` counts over this many seconds.
const RATE_WINDOW_SECS: u64 = 60;
/// Default cost-budget period when a key sets a cost cap but no explicit period.
const DEFAULT_COST_PERIOD: Duration = Duration::from_secs(60);

/// Configuration for one virtual key.
#[derive(Debug, Clone, Default)]
pub struct VirtualKey {
    /// The token value clients present (e.g. "vk_live_abc").
    pub id: String,
    pub name: String,
    /// Allowed models. Empty = all. Matches either the full `provider/model` or the
    /// bare model id.
    pub allowed_models: Vec<String>,
    /// Denied models. A match here is ALWAYS rejected — deny wins over the allow-list.
    /// Matches either the full `provider/model` or the bare model id.
    pub denied_models: Vec<String>,
    /// Max requests per fixed 60s window. `None` = unlimited.
    pub max_requests_per_min: Option<u32>,
    /// Cumulative total-token budget. `None` = unlimited.
    pub max_total_tokens: Option<u64>,
    /// Max estimated USD cost per rolling period (see `cost_period`). `None` = unlimited.
    /// Cost is estimated from the static price table; unpriced models accrue no cost.
    pub max_cost_per_period: Option<f64>,
    /// Length of the cost-budget period (tumbling window). Defaults to 60s when a cost
    /// budget is set but no period is given.
    pub cost_period: Option<Duration>,
}

/// Enforces per-virtual-key policy. Key *configs* are static (read-only after construction);
/// the mutable counters live in the [`GovernanceStore`].
pub struct GovernancePlugin {
    configs: HashMap<String, VirtualKey>,
    store: Arc<dyn GovernanceStore>,
    /// When true, a request without a recognized virtual key is rejected (401).
    /// When false (open mode), an ABSENT key passes; a PRESENTED-but-unknown key is still 401.
    require_key: bool,
}

impl GovernancePlugin {
    /// Build with the default in-process counter store (single-replica correct).
    pub fn new(keys: Vec<VirtualKey>, require_key: bool) -> Self {
        Self::with_store(keys, require_key, Arc::new(InMemoryGovernanceStore::new()))
    }

    /// Build with a shared counter store (e.g. Postgres) so limits hold across replicas.
    pub fn with_store(
        keys: Vec<VirtualKey>,
        require_key: bool,
        store: Arc<dyn GovernanceStore>,
    ) -> Self {
        let configs = keys.into_iter().map(|k| (k.id.clone(), k)).collect();
        Self {
            configs,
            store,
            require_key,
        }
    }

    /// The cost-budget period (seconds) for a key, defaulting when unset.
    fn cost_period_secs(cfg: &VirtualKey) -> u64 {
        cfg.cost_period.unwrap_or(DEFAULT_COST_PERIOD).as_secs()
    }

    /// Tokens consumed so far by a virtual key (for tests / a future control-plane API).
    pub async fn consumed_tokens(&self, vkey_id: &str) -> u64 {
        self.store.consumed_tokens(vkey_id).await.unwrap_or(0)
    }
}

#[async_trait]
impl RequestObserver for GovernancePlugin {
    fn name(&self) -> &str {
        "governance"
    }

    async fn on_request(&self, ctx: &Ctx, model: &str) -> Result<(), KgError> {
        // Distinguish an ABSENT key (anonymous) from a PRESENT-but-unrecognized one.
        let key_presented = ctx.virtual_key.is_some();
        let (vkey_id, cfg) = match ctx
            .virtual_key
            .as_ref()
            .and_then(|id| self.configs.get(id).map(|c| (id.as_str(), c)))
        {
            Some(pair) => pair,
            None => {
                // Presenting an unrecognized credential is an auth error even in open mode —
                // only a truly-absent key is treated as anonymous pass-through.
                if self.require_key || key_presented {
                    return Err(KgError::new(
                        KgErrorKind::Auth,
                        "missing or unknown virtual key",
                    ));
                }
                return Ok(());
            }
        };

        let bare = model.split_once('/').map(|(_, m)| m).unwrap_or(model);

        // Model deny-list — a match here ALWAYS rejects, winning over the allow-list.
        if cfg.denied_models.iter().any(|m| m == model || m == bare) {
            return Err(KgError::new(
                KgErrorKind::BadRequest,
                format!("model '{model}' is denied for this virtual key"),
            ));
        }

        // Model allow-list — match the full `provider/model` or the bare model id.
        if !cfg.allowed_models.is_empty()
            && !cfg.allowed_models.iter().any(|m| m == model || m == bare)
        {
            return Err(KgError::new(
                KgErrorKind::BadRequest,
                format!("model '{model}' is not allowed for this virtual key"),
            ));
        }

        // Fixed-window rate limit. Store errors fail OPEN (a counter-DB blip must not take
        // down traffic); the request is counted even if a later budget check rejects it.
        if let Some(max) = cfg.max_requests_per_min {
            match self.store.incr_requests(vkey_id, RATE_WINDOW_SECS).await {
                Ok(count) if count > max as u64 => {
                    return Err(KgError::new(KgErrorKind::RateLimit, "rate limit exceeded"));
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "governance rate-limit store error (fail-open)")
                }
            }
        }

        // Cumulative token budget.
        if let Some(max) = cfg.max_total_tokens {
            let consumed = self.store.consumed_tokens(vkey_id).await.unwrap_or(0);
            if consumed >= max {
                return Err(KgError::new(
                    KgErrorKind::RateLimit,
                    "token budget exhausted",
                ));
            }
        }

        // Cost budget for the current period (tumbling window in the store).
        if let Some(max_cost) = cfg.max_cost_per_period {
            let period = Self::cost_period_secs(cfg);
            let spent = self.store.period_cost(vkey_id, period).await.unwrap_or(0.0);
            if spent >= max_cost {
                return Err(KgError::new(
                    KgErrorKind::RateLimit,
                    "cost budget exhausted for period",
                ));
            }
        }

        Ok(())
    }

    async fn on_response(&self, ctx: &Ctx, record: &CallRecord) {
        // Account usage against the virtual key's budget (successful calls only).
        if record.status != 200 {
            return;
        }
        let Some(vkey_id) = ctx.virtual_key.as_ref() else {
            return;
        };
        let Some(cfg) = self.configs.get(vkey_id) else {
            return; // unknown key — nothing to account against
        };

        let tokens = record.total_tokens();
        if tokens > 0 {
            if let Err(e) = self.store.add_tokens(vkey_id, tokens).await {
                tracing::warn!(error = %e, "governance token accounting failed");
            }
        }
        if let Some(cost) = crate::pricing::estimate_cost(
            &record.model,
            record.prompt_tokens,
            record.completion_tokens,
        ) {
            let period = Self::cost_period_secs(cfg);
            if let Err(e) = self.store.add_cost(vkey_id, period, cost).await {
                tracing::warn!(error = %e, "governance cost accounting failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_key(id: &str) -> Ctx {
        let mut ctx = Ctx::new();
        ctx.virtual_key = Some(id.into());
        ctx
    }

    fn record(status: u16, total: u32) -> CallRecord {
        CallRecord {
            provider: "openai".into(),
            model: "m".into(),
            status,
            prompt_tokens: 0,
            completion_tokens: total,
            ..Default::default()
        }
    }

    fn record_priced(status: u16, model: &str, prompt: u32, completion: u32) -> CallRecord {
        CallRecord {
            provider: "openai".into(),
            model: model.into(),
            status,
            prompt_tokens: prompt,
            completion_tokens: completion,
            ..Default::default()
        }
    }

    fn vkey(id: &str) -> VirtualKey {
        VirtualKey {
            id: id.into(),
            name: id.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn unknown_key_rejected_in_strict_mode() {
        let g = GovernancePlugin::new(vec![vkey("vk1")], true);
        let ctx = ctx_with_key("nope");
        let err = g.on_request(&ctx, "openai/gpt-4o").await.unwrap_err();
        assert_eq!(err.kind, KgErrorKind::Auth);
    }

    #[tokio::test]
    async fn open_mode_allows_unknown_key() {
        let g = GovernancePlugin::new(vec![], false);
        let ctx = Ctx::new(); // no vkey
        assert!(g.on_request(&ctx, "openai/gpt-4o").await.is_ok());
    }

    #[tokio::test]
    async fn model_not_in_allow_list_rejected() {
        let mut k = vkey("vk1");
        k.allowed_models = vec!["openai/gpt-4o".into()];
        let g = GovernancePlugin::new(vec![k], true);
        let ctx = ctx_with_key("vk1");
        let err = g.on_request(&ctx, "anthropic/claude-3").await.unwrap_err();
        assert_eq!(err.kind, KgErrorKind::BadRequest);
        // allowed model passes
        assert!(g.on_request(&ctx, "openai/gpt-4o").await.is_ok());
    }

    #[tokio::test]
    async fn rate_limit_trips_after_max() {
        let mut k = vkey("vk1");
        k.max_requests_per_min = Some(2);
        let g = GovernancePlugin::new(vec![k], true);
        let ctx = ctx_with_key("vk1");
        assert!(g.on_request(&ctx, "m").await.is_ok());
        assert!(g.on_request(&ctx, "m").await.is_ok());
        // third within the window is rejected
        let err = g.on_request(&ctx, "m").await.unwrap_err();
        assert_eq!(err.kind, KgErrorKind::RateLimit);
    }

    #[tokio::test]
    async fn budget_exhaustion_rejects_next_request() {
        let mut k = vkey("vk1");
        k.max_total_tokens = Some(10);
        let g = GovernancePlugin::new(vec![k], true);
        let ctx = ctx_with_key("vk1");
        // First request allowed; then a successful response consumes 12 tokens.
        assert!(g.on_request(&ctx, "m").await.is_ok());
        g.on_response(&ctx, &record(200, 12)).await;
        assert_eq!(g.consumed_tokens("vk1").await, 12);
        // Now over budget → next request rejected.
        let err = g.on_request(&ctx, "m").await.unwrap_err();
        assert_eq!(err.kind, KgErrorKind::RateLimit);
    }

    #[tokio::test]
    async fn present_but_unknown_key_rejected_even_in_open_mode() {
        // Open mode (require_key=false): an ABSENT key passes, but a PRESENTED-yet-unknown
        // key is an auth error.
        let g = GovernancePlugin::new(vec![vkey("vk1")], false);
        assert!(g.on_request(&Ctx::new(), "m").await.is_ok()); // anonymous OK
        let err = g.on_request(&ctx_with_key("bogus"), "m").await.unwrap_err();
        assert_eq!(err.kind, KgErrorKind::Auth);
    }

    #[tokio::test]
    async fn denied_model_rejected_and_wins_over_allow_list() {
        let mut k = vkey("vk1");
        k.allowed_models = vec!["openai/gpt-4o".into()];
        k.denied_models = vec!["gpt-4o".into()]; // deny wins even though allow-listed
        let g = GovernancePlugin::new(vec![k], true);
        let ctx = ctx_with_key("vk1");
        let err = g.on_request(&ctx, "openai/gpt-4o").await.unwrap_err();
        assert_eq!(err.kind, KgErrorKind::BadRequest);
    }

    #[tokio::test]
    async fn cost_budget_exhaustion_rejects_next_request() {
        let mut k = vkey("vk1");
        k.max_cost_per_period = Some(0.000_001); // tiny budget
        let g = GovernancePlugin::new(vec![k], true);
        let ctx = ctx_with_key("vk1");
        assert!(g.on_request(&ctx, "gpt-4o").await.is_ok());
        // A priced call accrues cost above the tiny budget.
        g.on_response(&ctx, &record_priced(200, "gpt-4o", 1_000, 1_000))
            .await;
        let err = g.on_request(&ctx, "gpt-4o").await.unwrap_err();
        assert_eq!(err.kind, KgErrorKind::RateLimit);
        assert!(err.message.contains("cost budget"));
    }

    #[tokio::test]
    async fn failed_calls_do_not_consume_budget() {
        let g = GovernancePlugin::new(vec![vkey("vk1")], true);
        let ctx = ctx_with_key("vk1");
        g.on_response(&ctx, &record(500, 12)).await; // error → no accounting
        assert_eq!(g.consumed_tokens("vk1").await, 0);
    }

    #[tokio::test]
    async fn shared_store_enforces_budget_across_replicas() {
        // Two plugin instances (simulating two replicas) sharing ONE store enforce a single
        // combined budget — the whole point of a shared counter store.
        let store: Arc<dyn GovernanceStore> = Arc::new(InMemoryGovernanceStore::new());
        let mut k = vkey("vk1");
        k.max_total_tokens = Some(10);
        let replica_a = GovernancePlugin::with_store(vec![k.clone()], true, store.clone());
        let replica_b = GovernancePlugin::with_store(vec![k], true, store.clone());
        let ctx = ctx_with_key("vk1");

        // Replica A serves a request that consumes 12 tokens.
        assert!(replica_a.on_request(&ctx, "m").await.is_ok());
        replica_a.on_response(&ctx, &record(200, 12)).await;

        // Replica B now sees the SHARED budget as exhausted — not its own fresh counter.
        let err = replica_b.on_request(&ctx, "m").await.unwrap_err();
        assert_eq!(err.kind, KgErrorKind::RateLimit);
    }
}
