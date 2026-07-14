//! axum application: routes + shared state. The HTTP transport is a thin binder over
//! `kgateway_core::engine::Kgateway`. See `docs/01-architecture.md`.

use crate::auth::AuthContext;
use crate::config::Config;
use crate::metrics::Metrics;
use crate::otel::{self, OtelObserver, OtelProviders};
use crate::{handlers, metrics};
use arc_swap::ArcSwap;
use axum::routing::{get, post};
use axum::Router;
use kgateway_core::engine::{ContentCapture, Kgateway};
use kgateway_core::mcp::{StaticMcpClient, StdioMcpClient};
use kgateway_core::provider::Embeddings;
use kgateway_core::router::Registry;
use kgateway_plugins::redaction::Redactor;
use kgateway_plugins::{
    run_log_writer, GovernancePlugin, LoggingPlugin, ProviderEmbedder, SemanticCachePlugin,
    VirtualKey,
};
use kgateway_providers::{
    openai_compat, AnthropicProvider, AzureProvider, BedrockProvider, CohereProvider,
    GeminiProvider, OpenAiProvider,
};
use kgateway_store::{
    GovernanceStore, InMemoryGovernanceStore, InMemoryVectorStore, LogStore, MemoryLogStore,
    PgVectorStore, PostgresLogStore, RequestLog, SqlGovernanceStore, SqliteLogStore, VectorStore,
};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc, Notify};
use tokio::task::JoinHandle;

/// Default global request timeout when `request_timeout_secs` is unset in config.
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 120;

/// Capacity of the bounded channel feeding the async log batch-writer. When full (a slow
/// store under load), the logging observer drops records and counts them rather than
/// blocking the request hot path.
const LOG_WRITER_CAPACITY: usize = 1024;

/// How often the log-retention task sweeps for expired rows. The first sweep runs at
/// startup (tokio's interval fires its first tick immediately), then hourly.
const RETENTION_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// Milliseconds in a day, for translating `log_retention_days` into a cutoff timestamp.
const MS_PER_DAY: i64 = 86_400_000;

/// Capacity of the live-tail broadcast channel. Slow SSE subscribers that fall this far
/// behind get a `Lagged` marker and resume from the newest logs (the durable store still
/// has the full history), so a stuck browser tab can't grow memory unbounded.
const LOG_BROADCAST_CAPACITY: usize = 256;

/// The log sinks a `LoggingPlugin` writes to. Bundled so they can be threaded through
/// engine (re)builds together. All are stable across config reloads.
#[derive(Clone)]
pub struct LogSinks {
    /// Live-tail fan-out (SSE).
    pub broadcast: broadcast::Sender<RequestLog>,
    /// Durable-write channel to the async batch writer.
    pub writer: mpsc::Sender<RequestLog>,
    /// Count of logs dropped due to writer backpressure (`GET /api/logs/dropped`).
    pub dropped: Arc<AtomicU64>,
}

pub struct AppState {
    /// The engine, hot-swappable on config reload (SIGHUP / live-config writes). Read via
    /// `engine.load_full()`.
    pub engine: ArcSwap<Kgateway>,
    /// The current config, kept in sync with `config_path` on disk. Live-config write
    /// endpoints clone this, mutate it, persist, and rebuild the engine.
    pub config: ArcSwap<Config>,
    /// Path the config is loaded from / persisted to.
    pub config_path: String,
    /// Shared request-log store. In-memory by default; SQLite when `database` is set in
    /// config (Postgres behind the same trait later). Held here so `/api/logs` can read it
    /// and so it survives config reloads.
    pub log_store: Arc<dyn LogStore>,
    /// Log sinks (SSE broadcast + async writer channel + dropped counter). The
    /// `LoggingPlugin` is rebuilt with clones of these on every config reload, so they
    /// survive reloads. `/api/logs/stream` subscribes to `log_sinks.broadcast`;
    /// `/api/logs/dropped` reads `log_sinks.dropped`.
    pub log_sinks: LogSinks,
    /// Notify handle to signal the batch writer to flush + stop on graceful shutdown.
    pub writer_shutdown: Arc<Notify>,
    /// The writer task handle, awaited during graceful shutdown (see [`flush_logs`]).
    pub writer_handle: Mutex<Option<JoinHandle<()>>>,
    /// Prometheus metrics, updated by the tracking middleware, rendered at `/metrics`.
    pub metrics: Arc<Metrics>,
    /// RBAC token→role table guarding the control plane. When it has no tokens, auth is
    /// disabled (dev) — a startup warning is logged.
    pub auth: Arc<AuthContext>,
    /// Redactor for un-redacting content on the `logs:reveal` endpoint (same instance used
    /// by the logging plugin to redact). `None` when redaction is disabled.
    pub redactor: Option<Arc<Redactor>>,
    /// OTLP export observer (M15), re-attached to the engine on each reload. `None` when
    /// OTLP is disabled.
    pub otel_observer: Option<Arc<OtelObserver>>,
    /// OTel SDK providers, flushed + shut down on graceful exit.
    pub otel_providers: OtelProviders,
    /// Semantic-cache vector store (persistent pgvector or in-memory). Stable across reloads.
    pub vector_store: Arc<dyn VectorStore>,
}

pub type SharedState = Arc<AppState>;

/// Build a fully-configured engine from config, reusing an existing `log_store` (so it
/// survives config reloads). Registers providers, observers (logging + governance),
/// semantic cache, built-in + external MCP servers.
pub async fn build_configured_engine(
    config: &Config,
    log_store: Arc<dyn LogStore>,
    sinks: &LogSinks,
    redactor: &Option<Arc<Redactor>>,
    otel_observer: &Option<Arc<OtelObserver>>,
    vector_store: &Arc<dyn VectorStore>,
) -> Kgateway {
    // Logging + governance are RequestObservers: they run on EVERY capability
    // (chat/embeddings/images/audio/rerank), not just chat. The logger fans each record
    // out on the broadcast (SSE live tail) and hands durable writes to the batch writer.
    let mut logger = LoggingPlugin::new(log_store)
        .with_broadcast(sinks.broadcast.clone())
        .with_writer(sinks.writer.clone(), sinks.dropped.clone());
    if let Some(r) = redactor {
        logger = logger.with_redactor(r.clone());
    }
    let mut engine = build_engine(config).with_observer(Arc::new(logger));

    // OTLP export observer (M15): emits a span + metrics per request. Same Arc across
    // reloads so the SDK providers stay alive.
    if let Some(obs) = otel_observer {
        engine = engine.with_observer(obs.clone());
    }

    // Content capture (M10 Phase 2): off unless configured + enabled. Rebuilt each reload
    // so toggling it needs no restart.
    if let Some(cc) = &config.content_logging {
        engine = engine.with_content_capture(ContentCapture {
            enabled: cc.enabled,
            max_body_bytes: cc.max_body_bytes,
            capture_streaming: cc.capture_streaming,
        });
        if cc.enabled {
            if cc.max_body_bytes == 0 {
                tracing::warn!(
                    capture_streaming = cc.capture_streaming,
                    "content capture ENABLED with max_body_bytes = 0 (UNBOUNDED) — full \
                     request/response payloads (may contain secrets/PII) are persisted and \
                     returned to admins via GET /api/logs/{{id}}"
                );
            } else {
                tracing::warn!(
                    max_body_bytes = cc.max_body_bytes,
                    capture_streaming = cc.capture_streaming,
                    "content capture ENABLED — request/response payloads (may contain \
                     secrets/PII) are logged and returned to admins via GET /api/logs/{{id}}"
                );
            }
        }
    }

    // Governance: enabled (strict mode) when virtual keys are configured.
    if !config.virtual_keys.is_empty() {
        let keys: Vec<VirtualKey> = config
            .virtual_keys
            .iter()
            .map(|vk| VirtualKey {
                id: vk.id.clone(),
                name: if vk.name.is_empty() {
                    vk.id.clone()
                } else {
                    vk.name.clone()
                },
                allowed_models: vk.allowed_models.clone(),
                denied_models: vk.denied_models.clone(),
                max_requests_per_min: vk.max_requests_per_min,
                max_total_tokens: vk.max_total_tokens,
                max_cost_per_period: vk.max_cost_per_period,
                cost_period: vk.max_cost_period_secs.map(std::time::Duration::from_secs),
            })
            .collect();
        tracing::info!(count = keys.len(), "governance enabled (strict mode)");
        let store = build_governance_store(config).await;
        engine = engine.with_observer(Arc::new(GovernancePlugin::with_store(keys, true, store)));
    }

    // Semantic cache: enabled when configured and the embedding provider supports it.
    if let Some(cache_cfg) = &config.semantic_cache {
        match build_embedder(config, cache_cfg) {
            Some(embedder) => {
                tracing::info!(
                    provider = %cache_cfg.embedding_provider,
                    threshold = cache_cfg.threshold,
                    "semantic cache enabled"
                );
                engine = engine.with_plugin(Arc::new(SemanticCachePlugin::new(
                    embedder,
                    vector_store.clone(),
                    cache_cfg.threshold,
                )));
            }
            None => tracing::warn!(
                provider = %cache_cfg.embedding_provider,
                "semantic cache configured but embedding provider is missing/unsupported; disabled"
            ),
        }
    }

    // MCP tool gateway: built-in demo tools + external stdio MCP servers.
    if let Some(mcp_cfg) = &config.mcp {
        if mcp_cfg.builtin_tools {
            engine = engine.with_mcp(Arc::new(builtin_mcp_client()));
            tracing::info!("MCP built-in tools enabled");
        }
        for srv in &mcp_cfg.servers {
            match StdioMcpClient::connect(&srv.name, &srv.command, &srv.args).await {
                Ok(client) => {
                    tracing::info!(name = %srv.name, command = %srv.command, "connected MCP server");
                    engine = engine.with_mcp(Arc::new(client));
                }
                Err(e) => tracing::error!(
                    name = %srv.name, command = %srv.command, error = %e,
                    "failed to connect MCP server; skipping"
                ),
            }
        }
    }

    engine
}

/// Build the shared application state from config. The engine + config are hot-swappable
/// via [`reload_engine`] / [`apply_config`]; the log store, metrics, and admin token are
/// stable across reloads.
pub async fn build_state(config: Config, config_path: String) -> SharedState {
    let log_store = build_log_store(&config).await;

    // Log sinks: SSE broadcast + bounded writer channel + dropped counter. The batch
    // writer owns the store and drains the channel off the request hot path.
    let (broadcast_tx, _) = broadcast::channel(LOG_BROADCAST_CAPACITY);
    let (writer_tx, writer_rx) = mpsc::channel(LOG_WRITER_CAPACITY);
    let sinks = LogSinks {
        broadcast: broadcast_tx,
        writer: writer_tx,
        dropped: Arc::new(AtomicU64::new(0)),
    };
    let writer_shutdown = Arc::new(Notify::new());
    let writer_handle = tokio::spawn(run_log_writer(
        log_store.clone(),
        writer_rx,
        writer_shutdown.clone(),
    ));

    // Redactor (M11): built once and stable across reloads so the encryption key stays
    // consistent for decrypting previously-stored mappings.
    let redactor = build_redactor(&config);

    // OTLP export (M15): init the SDK once; the observer is re-attached on reloads and the
    // providers are flushed on shutdown.
    let (otel_obs, otel_providers) = otel::init(config.otlp.as_ref());
    let otel_observer = otel_obs.map(Arc::new);

    // Semantic-cache vector store (M5): persistent pgvector when Postgres is configured, else
    // in-memory. Built once + stable across reloads (the cache survives config changes).
    let vector_store = build_vector_store(&config).await;

    let engine = build_configured_engine(
        &config,
        log_store.clone(),
        &sinks,
        &redactor,
        &otel_observer,
        &vector_store,
    )
    .await;

    // RBAC: resolve the token→role table from the legacy admin_token + api_tokens.
    let auth = Arc::new(AuthContext::from_config(
        config.admin_token.as_deref(),
        &config.api_tokens,
    ));
    if auth.is_locked() {
        tracing::error!(
            "RBAC tokens are declared in config but ALL resolved empty (check ${{ENV}} vars) — \
             the control plane is LOCKED (fail-closed): every /api/* request will be rejected"
        );
    } else if auth.is_enabled() {
        tracing::info!(
            tokens = config.api_tokens.len() + config.admin_token.is_some() as usize,
            "control-plane RBAC enabled"
        );
    } else {
        tracing::warn!(
            "no admin_token / api_tokens configured — /api/* and /metrics are UNAUTHENTICATED (set tokens for production)"
        );
    }

    Arc::new(AppState {
        engine: ArcSwap::from_pointee(engine),
        config: ArcSwap::from_pointee(config),
        config_path,
        log_store,
        log_sinks: sinks,
        writer_shutdown,
        writer_handle: Mutex::new(Some(writer_handle)),
        metrics: Arc::new(Metrics::new()),
        auth,
        redactor,
        otel_observer,
        otel_providers,
        vector_store,
    })
}

/// Build the governance counter store. Uses a shared Postgres store when a Postgres
/// `database` is configured (so rate limits and token/cost budgets stay correct across
/// horizontally-scaled replicas); otherwise the in-process store. A connect/migrate failure
/// logs a warning and falls back to in-process counters rather than failing startup.
async fn build_governance_store(config: &Config) -> Arc<dyn GovernanceStore> {
    let is_postgres = config
        .database
        .as_deref()
        .is_some_and(|u| crate::config::interpolate_env(u).contains("postgres"));
    if is_postgres {
        let url = crate::config::interpolate_env(config.database.as_deref().unwrap());
        match SqlGovernanceStore::connect(&url).await {
            Ok(store) => {
                tracing::info!("governance: shared Postgres counter store");
                return Arc::new(store);
            }
            Err(e) => tracing::warn!(
                error = %e,
                "shared governance store unavailable; using in-process counters (per-replica limits)"
            ),
        }
    }
    Arc::new(InMemoryGovernanceStore::new())
}

/// Build the semantic-cache vector store. Uses Postgres + `pgvector` when a Postgres
/// `database` is configured AND the semantic cache is enabled (so the cache persists across
/// restarts and is shared across replicas); otherwise an in-memory store. A pgvector
/// connect/migrate failure (e.g. the `vector` extension isn't installed) logs a warning and
/// falls back to in-memory rather than failing startup.
async fn build_vector_store(config: &Config) -> Arc<dyn VectorStore> {
    let persistent = config.semantic_cache.is_some()
        && config
            .database
            .as_deref()
            .is_some_and(|u| crate::config::interpolate_env(u).contains("postgres"));
    if persistent {
        let url = crate::config::interpolate_env(config.database.as_deref().unwrap());
        match PgVectorStore::connect(&url).await {
            Ok(store) => {
                tracing::info!("semantic cache: persistent pgvector store");
                return Arc::new(store);
            }
            Err(e) => tracing::warn!(
                error = %e,
                "pgvector unavailable (is the `vector` extension installed?); semantic cache using in-memory store"
            ),
        }
    }
    Arc::new(InMemoryVectorStore::new())
}

/// Build the redactor from config (M11). Returns `None` when redaction is disabled. When
/// enabled without a key, redaction still masks but the reversible mapping is dropped
/// (reveal unavailable) — a loud warning is logged rather than failing startup.
fn build_redactor(config: &Config) -> Option<Arc<Redactor>> {
    let cfg = config.redaction.as_ref()?;
    if !cfg.enabled {
        return None;
    }
    let key = cfg
        .key
        .as_ref()
        .map(|k| crate::config::interpolate_env(k))
        .filter(|k| !k.is_empty());
    match &key {
        Some(_) => tracing::info!("redaction enabled (reversible; reveal available to admins)"),
        None => tracing::warn!(
            "redaction enabled but no key configured — bodies are masked but NOT revealable (set redaction.key for reversible redaction)"
        ),
    }
    match Redactor::new(&cfg.patterns, key.as_deref()) {
        Ok(r) => Some(Arc::new(r)),
        Err(e) => {
            tracing::error!(error = %e, "invalid redaction config; redaction DISABLED");
            None
        }
    }
}

/// Flush the async log writer on graceful shutdown: signal it to drain the channel, then
/// await the task so buffered logs are persisted before the process exits. Idempotent.
pub async fn flush_logs(state: &SharedState) {
    state.writer_shutdown.notify_one();
    let handle = state.writer_handle.lock().unwrap().take();
    if let Some(handle) = handle {
        if let Err(e) = handle.await {
            tracing::warn!(error = %e, "log writer task did not join cleanly");
        } else {
            tracing::info!("log writer flushed on shutdown");
        }
    }
}

/// Spawn the background log-retention task. It sweeps every [`RETENTION_SWEEP_INTERVAL`]
/// (first sweep at startup) and deletes logs older than `log_retention_days`. The window
/// is re-read from the live config each sweep, so a hot-reload that changes it takes
/// effect on the next sweep. A no-op each tick while retention is unset or zero (kept
/// running so enabling it via reload needs no restart). Returns the task handle.
pub fn spawn_retention(state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(RETENTION_SWEEP_INTERVAL);
        loop {
            ticker.tick().await;
            let days = state.config.load().log_retention_days.unwrap_or(0);
            if days == 0 {
                continue;
            }
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let cutoff = now_ms - i64::from(days) * MS_PER_DAY;
            match state.log_store.purge_older_than(cutoff).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(deleted = n, retention_days = days, "log retention sweep"),
                Err(e) => tracing::warn!(error = %e, "log retention sweep failed"),
            }
        }
    })
}

/// Apply a new config: persist it to `config_path`, rebuild the engine, and swap both the
/// engine and the stored config atomically. Used by the live-config write endpoints.
pub async fn apply_config(state: &SharedState, new_config: Config) -> Result<(), String> {
    new_config
        .to_file(&state.config_path)
        .map_err(|e| format!("failed to persist config: {e}"))?;
    let engine = build_configured_engine(
        &new_config,
        state.log_store.clone(),
        &state.log_sinks,
        &state.redactor,
        &state.otel_observer,
        &state.vector_store,
    )
    .await;
    state.engine.store(Arc::new(engine));
    state.config.store(Arc::new(new_config));
    tracing::info!(path = %state.config_path, "live config applied; engine hot-swapped");
    Ok(())
}

/// Reload config from `config_path` and hot-swap the engine (providers, observers,
/// virtual keys, cache, MCP). Reuses the existing log store. Called on SIGHUP.
///
/// Not reloaded (require a restart): listen port and `admin_token`.
pub async fn reload_engine(state: &SharedState, config_path: &str) {
    match Config::from_file(config_path) {
        Ok(config) => {
            let engine = build_configured_engine(
                &config,
                state.log_store.clone(),
                &state.log_sinks,
                &state.redactor,
                &state.otel_observer,
                &state.vector_store,
            )
            .await;
            state.engine.store(Arc::new(engine));
            state.config.store(Arc::new(config));
            tracing::info!(path = config_path, "config reloaded; engine hot-swapped");
        }
        Err(e) => {
            tracing::error!(path = config_path, error = %e, "config reload failed; keeping current engine");
        }
    }
}

/// A small in-process MCP client with demo tools, so agentic tool-calling can be
/// exercised without an external server.
fn builtin_mcp_client() -> StaticMcpClient {
    StaticMcpClient::new("builtin").with_tool(
        "echo",
        "Echo back the provided text.",
        serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        }),
        Arc::new(|args| {
            let v: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
            Ok(v.get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string())
        }),
    )
}

/// Build a `ProviderEmbedder` from the cache config: reuse the named provider's base_url
/// + first key. Only OpenAI / OpenAI-compatible providers support embeddings.
fn build_embedder(
    config: &Config,
    cache_cfg: &crate::config::SemanticCacheConfig,
) -> Option<Arc<ProviderEmbedder>> {
    let pc = config.providers.get(&cache_cfg.embedding_provider)?;
    let key = pc.keys.first()?.resolve();
    let name = cache_cfg.embedding_provider.as_str();
    let provider: Arc<dyn Embeddings> = match name {
        "openai" => match &pc.base_url {
            Some(url) => Arc::new(OpenAiProvider::with_base_url(url.clone())),
            None => Arc::new(OpenAiProvider::new()),
        },
        // OpenAI-compatible vendors are OpenAiProvider under the hood → support embeddings.
        other => Arc::new(openai_compat::build(other, pc.base_url.clone())?),
    };
    Some(Arc::new(ProviderEmbedder::new(
        provider,
        key,
        cache_cfg.embedding_model.clone(),
    )))
}

/// Select the request-log store from config: Postgres for a `postgres://` URL, SQLite
/// for any other URL, in-memory when no `database` is set. Falls back to in-memory if a
/// configured store fails to open (so the gateway still serves traffic).
async fn build_log_store(config: &Config) -> Arc<dyn LogStore> {
    let Some(raw) = &config.database else {
        return Arc::new(MemoryLogStore::default());
    };
    let url = crate::config::interpolate_env(raw);

    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        match PostgresLogStore::connect(&url).await {
            Ok(store) => {
                tracing::info!("connected Postgres log store");
                return Arc::new(store);
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to open Postgres store; falling back to in-memory");
                return Arc::new(MemoryLogStore::default());
            }
        }
    }

    match SqliteLogStore::connect(&url).await {
        Ok(store) => {
            tracing::info!(%url, "connected SQLite log store");
            Arc::new(store)
        }
        Err(e) => {
            tracing::error!(%url, error = %e, "failed to open SQLite store; falling back to in-memory");
            Arc::new(MemoryLogStore::default())
        }
    }
}

/// Build the engine from config: register each configured provider with its keys.
pub fn build_engine(config: &Config) -> Kgateway {
    let mut registry = Registry::new();

    for (name, pc) in &config.providers {
        let keys: Vec<_> = pc.keys.iter().map(|k| k.resolve()).collect();

        // Explicit `kind` wins: register a custom-named provider under a given wire
        // format. Enables e.g. z.ai's GLM Coding Plan (Anthropic-compatible):
        //   "zai": { "kind": "anthropic", "base_url": "https://api.z.ai/api/anthropic", ... }
        match pc.kind.as_deref() {
            Some("anthropic") => {
                let base = pc
                    .base_url
                    .clone()
                    .unwrap_or_else(|| "https://api.anthropic.com".to_string());
                registry.register(Arc::new(AnthropicProvider::with_identity(name, base)), keys);
                continue;
            }
            Some("openai") => {
                let base = pc
                    .base_url
                    .clone()
                    .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
                registry.register(Arc::new(OpenAiProvider::with_identity(name, base)), keys);
                continue;
            }
            Some("bedrock") => {
                // AWS Bedrock: `base_url` carries the region (e.g. "us-east-1"); each key
                // value is "ACCESS_KEY_ID:SECRET_ACCESS_KEY".
                let region = pc
                    .base_url
                    .clone()
                    .unwrap_or_else(|| "us-east-1".to_string());
                registry.register(Arc::new(BedrockProvider::with_identity(name, region)), keys);
                continue;
            }
            Some("gemini") => {
                // Google Gemini (native generateContent, API-key auth).
                let base = pc.base_url.clone().unwrap_or_else(|| {
                    "https://generativelanguage.googleapis.com/v1beta".to_string()
                });
                registry.register(Arc::new(GeminiProvider::with_identity(name, base)), keys);
                continue;
            }
            Some("azure") => {
                // Azure OpenAI: `base_url` = the resource endpoint
                // (https://<resource>.openai.azure.com); the model is the deployment name.
                match &pc.base_url {
                    Some(url) => registry.register(
                        Arc::new(AzureProvider::with_identity(name, url.clone())),
                        keys,
                    ),
                    None => {
                        tracing::warn!(provider = %name, "azure provider requires base_url; skipping")
                    }
                }
                continue;
            }
            Some(other) => {
                tracing::warn!(provider = %name, kind = %other, "unknown provider kind; falling back to name inference");
            }
            None => {}
        }

        match name.as_str() {
            "openai" => {
                let provider = match &pc.base_url {
                    Some(url) => OpenAiProvider::with_base_url(url.clone()),
                    None => OpenAiProvider::new(),
                };
                registry.register(Arc::new(provider), keys);
            }
            "anthropic" => {
                let provider = match &pc.base_url {
                    Some(url) => AnthropicProvider::with_base_url(url.clone()),
                    None => AnthropicProvider::new(),
                };
                registry.register(Arc::new(provider), keys);
            }
            "cohere" => {
                let provider = match &pc.base_url {
                    Some(url) => CohereProvider::with_base_url(url.clone()),
                    None => CohereProvider::new(),
                };
                registry.register(Arc::new(provider), keys);
            }
            // Known OpenAI-compatible vendors (groq, ollama, openrouter, xai, ...): use
            // the built-in default base URL, overridable via config.
            other if openai_compat::default_base_url(other).is_some() => {
                if let Some(provider) = openai_compat::build(other, pc.base_url.clone()) {
                    registry.register(Arc::new(provider), keys);
                }
            }
            // Any other name is treated as OpenAI-compatible IF a base_url is given.
            other => {
                if let Some(url) = &pc.base_url {
                    let provider = OpenAiProvider::with_identity(other, url.clone());
                    registry.register(Arc::new(provider), keys);
                } else {
                    tracing::warn!(
                        provider = other,
                        "skipping unknown provider without base_url"
                    );
                }
            }
        }
    }

    Kgateway::new(registry)
}

pub fn build_router(state: SharedState) -> Router {
    // Data-plane: OpenAI-compatible endpoints (auth is per-request virtual key).
    let data_plane = Router::new()
        .route("/health", get(handlers::health))
        .route("/v1/chat/completions", post(handlers::chat_completions))
        .route("/v1/messages", post(crate::anthropic_ingress::messages))
        .route("/v1/embeddings", post(handlers::embeddings))
        .route("/v1/images/generations", post(handlers::images_generations))
        .route("/v1/audio/speech", post(handlers::audio_speech))
        .route(
            "/v1/audio/transcriptions",
            post(handlers::audio_transcriptions),
        )
        .route("/v1/rerank", post(handlers::rerank))
        // Live-tail SSE: self-authenticates via `?token=` (browser EventSource can't send
        // an Authorization header), so it lives OUTSIDE the header-only require_admin layer.
        .route("/api/logs/stream", get(handlers::logs_stream));

    // Control-plane, split by RBAC permission (M11):
    //  - view group   (logs:view)    — reads: logs, stats, config reads, metrics
    //  - write group  (config:write) — config mutations (providers / virtual keys)
    //  - reveal group (logs:reveal)  — un-redact captured content
    let view_group = Router::new()
        .route("/api/logs", get(handlers::logs))
        // Static segment registered alongside the `{id}` param route; matchit prioritizes
        // the static `/api/logs/stats` over `/api/logs/{id}`, so both coexist safely.
        .route("/api/logs/stats", get(handlers::logs_stats))
        .route("/api/logs/dropped", get(handlers::logs_dropped))
        .route("/api/logs/histogram", get(handlers::logs_histogram))
        .route("/api/logs/timeseries", get(handlers::logs_timeseries))
        .route("/api/logs/rankings", get(handlers::logs_rankings))
        .route("/api/logs/filterdata", get(handlers::logs_filterdata))
        .route("/api/logs/{id}", get(handlers::log_detail))
        .route("/api/mcp/tools", get(handlers::mcp_tools))
        .route("/api/providers", get(handlers::providers))
        .route("/api/config/providers", get(handlers::get_config_providers))
        .route("/api/config/virtual-keys", get(handlers::get_config_vkeys))
        .route("/metrics", get(handlers::metrics))
        .route("/api/whoami", get(handlers::whoami))
        .route("/api/status", get(handlers::status))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_view,
        ));

    let write_group = Router::new()
        .route(
            "/api/config/providers/{name}",
            axum::routing::put(handlers::put_provider).delete(handlers::delete_provider),
        )
        .route(
            "/api/config/virtual-keys/{id}",
            axum::routing::put(handlers::put_vkey).delete(handlers::delete_vkey),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_write,
        ));

    let reveal_group = Router::new()
        .route("/api/logs/{id}/reveal", get(handlers::log_reveal))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_reveal,
        ));

    let control_plane = view_group.merge(write_group).merge(reveal_group);

    let timeout_secs = state
        .config
        .load_full()
        .request_timeout_secs
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS);
    let cors_layer = build_cors_layer(&state.config.load_full());

    data_plane
        .merge(control_plane)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            metrics::track_metrics,
        ))
        // Reject pathologically nested JSON before it reaches serde (stack-overflow DoS).
        .layer(axum::middleware::from_fn(crate::auth::json_depth_guard))
        // CORS for the browser dashboard. Configurable via `cors_allow_origins`; falls
        // back to permissive (any origin) when unset — fine for local dev, but a
        // production deployment should set an explicit allow-list.
        .layer(cors_layer)
        // Global per-request timeout; returns 408 on expiry. Configurable via
        // `request_timeout_secs` (default 120s).
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(timeout_secs),
        ))
        .with_state(state)
}

/// Build the CORS layer from config: an explicit allow-list when
/// `cors_allow_origins` is set and non-empty, otherwise permissive (with a warning).
fn build_cors_layer(config: &Config) -> tower_http::cors::CorsLayer {
    use axum::http::HeaderValue;
    use tower_http::cors::{Any, CorsLayer};

    match &config.cors_allow_origins {
        Some(origins) if !origins.is_empty() => {
            let parsed: Vec<HeaderValue> = origins
                .iter()
                .filter_map(|origin| match HeaderValue::from_str(origin) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        tracing::warn!(origin = %origin, error = %e, "invalid CORS origin; skipping");
                        None
                    }
                })
                .collect();
            CorsLayer::new()
                .allow_origin(parsed)
                .allow_methods(Any)
                .allow_headers(Any)
        }
        _ => {
            tracing::warn!(
                "no cors_allow_origins configured — CORS is permissive (any origin); set cors_allow_origins for production"
            );
            CorsLayer::permissive()
        }
    }
}
