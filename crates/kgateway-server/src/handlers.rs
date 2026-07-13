//! HTTP handlers. M1: health + chat completions (JSON and SSE streaming).

use crate::app::SharedState;
use crate::config::{ProviderConfig, VirtualKeyInput};
use axum::extract::{Multipart, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use kgateway_core::context::Ctx;
use kgateway_core::error::{KgError, KgErrorKind};
use kgateway_core::provider::{
    EmbeddingRequest, ImageGenerationRequest, RerankRequest, SpeechRequest, TranscriptionRequest,
};
use kgateway_core::schema::ChatRequest;
use kgateway_store::{HistogramMetric, LogFilter, LogQuery, RankDimension, RankMetric, SortBy};
use serde::Deserialize;
use std::collections::HashMap;
use std::convert::Infallible;

/// Hard cap on `limit` for `/api/logs`, so a client can't request an unbounded page.
const MAX_LOG_LIMIT: usize = 200;
/// Default page size when `limit` is omitted.
const DEFAULT_LOG_LIMIT: usize = 50;

/// Extract the virtual key from an `Authorization: Bearer <token>` header.
pub(crate) fn vkey_from_headers(headers: &HeaderMap) -> Option<String> {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    auth.strip_prefix("Bearer ")
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

pub async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

/// `GET /metrics` — Prometheus text exposition.
pub async fn metrics(State(state): State<SharedState>) -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        state.metrics.render_prometheus(),
    )
        .into_response()
}

/// Query params shared by `GET /api/logs` and `GET /api/logs/stats`. All optional; the
/// filter fields map onto [`LogFilter`], the paging/sort fields onto [`LogQuery`].
#[derive(Debug, Default, Deserialize)]
pub struct LogsParams {
    /// Page size, clamped to [`MAX_LOG_LIMIT`]; defaults to [`DEFAULT_LOG_LIMIT`].
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// `created_at` | `latency` | `tokens` | `cost` (default `created_at`).
    pub sort_by: Option<String>,
    /// `asc` | `desc` (default `desc`).
    pub order: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub virtual_key: Option<String>,
    pub search: Option<String>,
    pub status: Option<u16>,
    pub since_ms: Option<i64>,
    pub cache_hit: Option<bool>,
}

impl LogsParams {
    /// Build the store [`LogFilter`] from the filter params (empty strings are ignored).
    fn to_filter(&self) -> LogFilter {
        fn nonempty(s: &Option<String>) -> Option<String> {
            s.as_ref()
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        }
        LogFilter {
            provider: nonempty(&self.provider),
            model: nonempty(&self.model),
            status: self.status,
            virtual_key: nonempty(&self.virtual_key),
            since_ms: self.since_ms,
            cache_hit: self.cache_hit,
            search: nonempty(&self.search),
        }
    }

    /// Build the full [`LogQuery`] (filter + paging + sort). `limit` is clamped to
    /// [`MAX_LOG_LIMIT`]; unknown `sort_by`/`order` values fall back to the defaults.
    fn to_query(&self) -> LogQuery {
        let sort_by = match self.sort_by.as_deref() {
            Some("latency") => SortBy::Latency,
            Some("tokens") => SortBy::Tokens,
            Some("cost") => SortBy::Cost,
            _ => SortBy::CreatedAt,
        };
        // Anything other than an explicit "asc" is descending (the default).
        let descending = self.order.as_deref() != Some("asc");
        LogQuery {
            filter: self.to_filter(),
            limit: self.limit.unwrap_or(DEFAULT_LOG_LIMIT).min(MAX_LOG_LIMIT),
            offset: self.offset.unwrap_or(0),
            sort_by,
            descending,
        }
    }
}

/// `GET /api/logs` — filtered, sorted, paginated request logs (control-plane).
pub async fn logs(State(state): State<SharedState>, Query(params): Query<LogsParams>) -> Response {
    let query = params.to_query();
    match state.log_store.query(&query).await {
        Ok(page) => Json(page).into_response(),
        Err(e) => store_error_response(e),
    }
}

/// `GET /api/logs/stats` — aggregate stats over the same filter params as `/api/logs`.
pub async fn logs_stats(
    State(state): State<SharedState>,
    Query(params): Query<LogsParams>,
) -> Response {
    let filter = params.to_filter();
    match state.log_store.stats(&filter).await {
        Ok(stats) => Json(stats).into_response(),
        Err(e) => store_error_response(e),
    }
}

/// Build a [`LogFilter`] from a raw query map (shared by the analytics endpoints, which
/// take the same filter params as `/api/logs`).
fn filter_from_map(q: &HashMap<String, String>) -> LogFilter {
    let ne = |k: &str| {
        q.get(k)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    LogFilter {
        provider: ne("provider"),
        model: ne("model"),
        status: q.get("status").and_then(|s| s.parse().ok()),
        virtual_key: ne("virtual_key"),
        since_ms: q.get("since_ms").and_then(|s| s.parse().ok()),
        cache_hit: q.get("cache_hit").and_then(|s| s.parse().ok()),
        search: ne("search"),
    }
}

/// `GET /api/logs/histogram?metric=latency|cost|tokens&buckets=N&<filters>` (M12).
pub async fn logs_histogram(
    State(state): State<SharedState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let filter = filter_from_map(&q);
    let metric = match q.get("metric").map(String::as_str) {
        Some("cost") => HistogramMetric::Cost,
        Some("tokens") => HistogramMetric::Tokens,
        _ => HistogramMetric::Latency,
    };
    let buckets = q.get("buckets").and_then(|s| s.parse().ok()).unwrap_or(20);
    match state.log_store.histogram(&filter, metric, buckets).await {
        Ok(h) => Json(h).into_response(),
        Err(e) => store_error_response(e),
    }
}

/// `GET /api/logs/timeseries?bucket_ms=N&<filters>` (M12). Default bucket: 60s.
pub async fn logs_timeseries(
    State(state): State<SharedState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let filter = filter_from_map(&q);
    let bucket_ms = q
        .get("bucket_ms")
        .and_then(|s| s.parse().ok())
        .unwrap_or(60_000);
    match state.log_store.timeseries(&filter, bucket_ms).await {
        Ok(points) => Json(serde_json::json!({ "points": points })).into_response(),
        Err(e) => store_error_response(e),
    }
}

/// `GET /api/logs/rankings?by=model|provider|virtual_key&metric=count|cost|tokens|errors&limit=N&<filters>` (M12).
pub async fn logs_rankings(
    State(state): State<SharedState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let filter = filter_from_map(&q);
    let dimension = match q.get("by").map(String::as_str) {
        Some("provider") => RankDimension::Provider,
        Some("virtual_key") => RankDimension::VirtualKey,
        _ => RankDimension::Model,
    };
    let metric = match q.get("metric").map(String::as_str) {
        Some("cost") => RankMetric::Cost,
        Some("tokens") => RankMetric::Tokens,
        Some("errors") => RankMetric::Errors,
        _ => RankMetric::Count,
    };
    let limit = q
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(10)
        .min(100);
    match state
        .log_store
        .rankings(&filter, dimension, metric, limit)
        .await
    {
        Ok(rankings) => Json(serde_json::json!({ "rankings": rankings })).into_response(),
        Err(e) => store_error_response(e),
    }
}

/// `GET /api/logs/filterdata` — distinct provider/model/virtual-key values (M12).
pub async fn logs_filterdata(State(state): State<SharedState>) -> Response {
    match state.log_store.filter_values().await {
        Ok(fd) => Json(fd).into_response(),
        Err(e) => store_error_response(e),
    }
}

/// `GET /api/logs/dropped` — count of logs dropped due to writer backpressure (the async
/// batch writer's channel filled under load). A nonzero value means the durable store
/// couldn't keep up and some audit rows were shed to protect the request hot path.
pub async fn logs_dropped(State(state): State<SharedState>) -> Response {
    let dropped = state
        .log_sinks
        .dropped
        .load(std::sync::atomic::Ordering::Relaxed);
    Json(serde_json::json!({ "dropped": dropped })).into_response()
}

/// `GET /api/logs/{id}` — a single request log by id (control-plane).
pub async fn log_detail(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    match state.log_store.get(&id).await {
        Ok(Some(log)) => Json(log).into_response(),
        Ok(None) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": { "message": "log not found", "type": "not_found" }
            })),
        )
            .into_response(),
        Err(e) => store_error_response(e),
    }
}

/// `GET /api/whoami` — the caller's role + permissions, so the UI can show/hide controls
/// (e.g. the Reveal button). In the `view` group, so any authenticated token can call it.
pub async fn whoami(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    use crate::auth::Permission;
    // Auth disabled (dev) ⇒ treat the caller as admin.
    let role = if !state.auth.is_enabled() {
        crate::config::Role::Admin
    } else {
        let presented = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .and_then(|t| state.auth.role_for(t));
        match presented {
            Some(r) => r,
            None => {
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({
                        "error": { "message": "authentication required", "type": "auth" }
                    })),
                )
                    .into_response()
            }
        }
    };
    let mut permissions = vec!["logs:view"];
    if role.permits(Permission::ConfigWrite) {
        permissions.push("config:write");
    }
    if role.permits(Permission::LogsReveal) {
        permissions.push("logs:reveal");
    }
    Json(serde_json::json!({ "role": role, "permissions": permissions })).into_response()
}

/// `GET /api/logs/{id}/reveal` — un-redact a log's captured bodies (M11). Gated by
/// `logs:reveal` (admin) at the router layer. Uses the shared redactor to decrypt the
/// stored mapping and restore original values. Every reveal is audit-logged.
pub async fn log_reveal(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    // Reveal-only store read: this is the ONLY path that loads the encrypted mapping.
    let log = match state.log_store.get_with_mapping(&id).await {
        Ok(Some(l)) => l,
        Ok(None) => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": { "message": "log not found", "type": "not_found" }
                })),
            )
                .into_response()
        }
        Err(e) => return store_error_response(e),
    };

    let Some(redactor) = &state.redactor else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "message": "redaction is not enabled; nothing to reveal", "type": "bad_request" }
            })),
        )
            .into_response();
    };

    // Mapping is a small JSON object: { "request": <blob>, "response": <blob> } (only keys
    // that had redactions). A body with no mapping entry is returned as stored.
    let mappings: serde_json::Value = log
        .redaction_mapping
        .as_deref()
        .and_then(|m| serde_json::from_str(m).ok())
        .unwrap_or(serde_json::Value::Null);

    let (request_body, request_revealed) =
        reveal_body(redactor, &log.request_body, mappings.get("request"));
    let (response_body, response_revealed) =
        reveal_body(redactor, &log.response_body, mappings.get("response"));

    // Audit: revealing secrets is privileged — capture who/when/which. `who` is the
    // presented token's configured name (or its role), resolved from the auth table.
    let who = bearer_identity(&state, &headers);
    tracing::warn!(
        request_id = %id,
        revealed_by = %who,
        "log content revealed via logs:reveal"
    );

    Json(serde_json::json!({
        "request_id": id,
        "request_body": request_body,
        "response_body": response_body,
        // Distinguishes "restored original" from "nothing to reveal / undecryptable".
        "request_revealed": request_revealed,
        "response_revealed": response_revealed,
    }))
    .into_response()
}

/// Resolve the caller's audit identity (token name/role) from the Authorization header.
fn bearer_identity(state: &SharedState, headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .and_then(|t| state.auth.identify(t))
        .map(|(_, name)| name)
        .unwrap_or_else(|| "anonymous(auth-disabled)".to_string())
}

/// Reveal one body. Returns `(body, revealed)` where `revealed` is true only when a mapping
/// entry existed AND decryption succeeded. A decrypt failure falls back to the stored
/// (still-redacted) body with `revealed = false` — fail-closed, never leaks the key or 500s.
fn reveal_body(
    redactor: &kgateway_plugins::redaction::Redactor,
    body: &Option<String>,
    mapping: Option<&serde_json::Value>,
) -> (Option<String>, bool) {
    let Some(body) = body.as_ref() else {
        return (None, false);
    };
    match mapping.and_then(|m| m.as_str()) {
        Some(blob) => match redactor.reveal(body, blob) {
            Ok(revealed) => (Some(revealed), true),
            Err(_) => (Some(body.clone()), false),
        },
        None => (Some(body.clone()), false),
    }
}

/// `GET /api/logs/stream?token=<token>` — SSE live tail of appended request logs.
///
/// Browser `EventSource` can't set an `Authorization` header, so this endpoint
/// self-authenticates from the `token` query param (rather than the header-only RBAC
/// layer). Requires `logs:view`. When auth is disabled, it's open. The broadcast never
/// carries captured bodies, so no redaction concern here.
pub async fn logs_stream(
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if state.auth.is_enabled() {
        let permitted = params
            .get("token")
            .and_then(|t| state.auth.role_for(t))
            .is_some_and(|role| role.permits(crate::auth::Permission::LogsView));
        if !permitted {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": { "message": "authentication required", "type": "auth" }
                })),
            )
                .into_response();
        }
    }

    let mut rx = state.log_sinks.broadcast.subscribe();
    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(log) => {
                    let data = serde_json::to_string(&log).unwrap_or_default();
                    yield Ok::<_, Infallible>(Event::default().data(data));
                }
                // Slow subscriber fell behind: skip the dropped records and resume.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                // Sender dropped (shouldn't happen while state is alive): end the stream.
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// `POST /v1/embeddings` — OpenAI-compatible embeddings, routed by `provider/model`.
pub async fn embeddings(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<EmbeddingRequest>,
) -> Response {
    let mut ctx = Ctx::new();
    ctx.virtual_key = vkey_from_headers(&headers);
    crate::otel::apply_trace_context(&mut ctx, &headers);
    match state.engine.load_full().embed(&ctx, req).await {
        Ok(resp) => {
            // Re-shape to the OpenAI embeddings wire format.
            let data: Vec<_> = resp
                .embeddings
                .into_iter()
                .enumerate()
                .map(|(i, e)| serde_json::json!({ "object": "embedding", "index": i, "embedding": e }))
                .collect();
            Json(serde_json::json!({
                "object": "list",
                "model": resp.model,
                "data": data,
                "usage": {
                    "prompt_tokens": resp.usage.prompt_tokens,
                    "total_tokens": resp.usage.total_tokens,
                },
            }))
            .into_response()
        }
        Err(e) => error_response(e),
    }
}

/// `POST /v1/chat/completions` — branches on `stream` for SSE vs JSON.
pub async fn chat_completions(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Response {
    let mut ctx = Ctx::new();
    ctx.virtual_key = vkey_from_headers(&headers);
    crate::otel::apply_trace_context(&mut ctx, &headers);

    if req.stream.unwrap_or(false) {
        match state.engine.load_full().chat_stream(&mut ctx, req).await {
            Ok(stream) => {
                let sse = stream.map(|item| -> Result<Event, Infallible> {
                    match item {
                        Ok(chunk) => {
                            let data = serde_json::to_string(&chunk).unwrap_or_default();
                            Ok(Event::default().data(data))
                        }
                        Err(e) => {
                            let data = serde_json::to_string(&error_body(&e)).unwrap_or_default();
                            Ok(Event::default().event("error").data(data))
                        }
                    }
                });
                // Terminal [DONE] frame for OpenAI client compatibility.
                let done = futures::stream::once(async { Ok(Event::default().data("[DONE]")) });
                Sse::new(sse.chain(done)).into_response()
            }
            Err(e) => error_response(e),
        }
    } else {
        // Use the agentic loop (auto-inject + execute MCP tools) when MCP is enabled;
        // otherwise a plain single-turn completion. Snapshot the engine once.
        let engine = state.engine.load_full();
        let result = if engine.has_mcp() {
            engine
                .chat_agentic(
                    &mut ctx,
                    req,
                    kgateway_core::engine::DEFAULT_MAX_TOOL_ROUNDS,
                )
                .await
        } else {
            engine.chat(&mut ctx, req).await
        };
        match result {
            Ok(resp) => Json(resp).into_response(),
            Err(e) => error_response(e),
        }
    }
}

/// `GET /api/mcp/tools` — tools exposed by the registered MCP servers.
pub async fn mcp_tools(State(state): State<SharedState>) -> Response {
    let tools = state.engine.load_full().list_mcp_tools().await;
    Json(serde_json::json!({ "tools": tools })).into_response()
}

/// `GET /api/providers` — configured providers + their capabilities (read-only).
pub async fn providers(State(state): State<SharedState>) -> Response {
    let summaries = state.engine.load_full().registry().summaries();
    Json(serde_json::json!({ "providers": summaries })).into_response()
}

// ---- Live config (control-plane, admin-guarded) ----

/// `GET /api/status` — non-secret runtime + config summary for the dashboard (feature
/// flags, active plugins, DB mode, semantic-cache settings). Feeds the Cache / Plugins /
/// Settings pages. `logs:view`.
pub async fn status(State(state): State<SharedState>) -> Response {
    let cfg = state.config.load_full();

    let governance = !cfg.virtual_keys.is_empty();
    let cache = cfg.semantic_cache.is_some();
    let redaction = cfg.redaction.as_ref().is_some_and(|r| r.enabled);
    let content = cfg.content_logging.as_ref().is_some_and(|c| c.enabled);
    let otlp = cfg.otlp.as_ref().is_some_and(|o| o.enabled);
    let mcp = cfg.mcp.is_some();

    // Active pipeline, derived the same way `build_configured_engine` assembles it.
    let plugins = serde_json::json!([
        { "name": "logging", "description": "Audit log of every request (tokens, cost, stop reason, latency)", "enabled": true },
        { "name": "governance", "description": "Virtual-key auth, rate limits, token + cost budgets, model allow/deny-lists (shared counters on Postgres)", "enabled": governance },
        { "name": "semantic_cache", "description": "Two-tier embedding-similarity response cache (exact + semantic; persistent on Postgres)", "enabled": cache },
        { "name": "content_capture", "description": "Capture request/response bodies (admin-only)", "enabled": content },
        { "name": "redaction", "description": "Secret/PII redaction of captured bodies", "enabled": redaction },
        { "name": "otlp", "description": "OpenTelemetry trace + metric export", "enabled": otlp },
        { "name": "mcp", "description": "MCP tool gateway", "enabled": mcp },
    ]);

    let database = match cfg.database.as_deref() {
        Some(u) if u.contains("postgres") => "postgres",
        Some(_) => "sqlite",
        None => "memory",
    };

    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "port": cfg.port,
        "database": database,
        "auth": if state.auth.is_enabled() { "enabled" } else { "open" },
        "log_retention_days": cfg.log_retention_days,
        "request_timeout_secs": cfg.request_timeout_secs.unwrap_or(120),
        "cors_allow_origins": cfg.cors_allow_origins,
        "providers": cfg.providers.keys().collect::<Vec<_>>(),
        "virtual_keys_count": cfg.virtual_keys.len(),
        "semantic_cache": cfg.semantic_cache.as_ref().map(|c| serde_json::json!({
            "embedding_provider": c.embedding_provider,
            "embedding_model": c.embedding_model,
            "threshold": c.threshold,
        })),
        "redaction_reveal": cfg.redaction.as_ref().and_then(|r| r.key.as_ref()).is_some(),
        "features": {
            "content_logging": content,
            "redaction": redaction,
            "semantic_cache": cache,
            "governance": governance,
            "mcp": mcp,
            "otlp": otlp,
        },
        "plugins": plugins,
    }))
    .into_response()
}

/// `GET /api/config/providers` — provider configs for editing, with key values redacted.
pub async fn get_config_providers(State(state): State<SharedState>) -> Response {
    let config = state.config.load_full();
    let providers: Vec<_> = config
        .providers
        .iter()
        .map(|(name, pc)| {
            serde_json::json!({
                "name": name,
                "kind": pc.kind,
                "base_url": pc.base_url,
                "keys": pc.keys.iter().map(|k| serde_json::json!({
                    "id": k.id, "value": "<redacted>", "weight": k.weight, "models": k.models,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Json(serde_json::json!({ "providers": providers })).into_response()
}

/// `PUT /api/config/providers/{name}` — create or update a provider, persist, hot-reload.
pub async fn put_provider(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(pc): Json<ProviderConfig>,
) -> Response {
    if name.trim().is_empty() {
        return error_response(KgError::new(
            KgErrorKind::BadRequest,
            "provider name required",
        ));
    }
    let mut config = (*state.config.load_full()).clone();
    config.providers.insert(name.clone(), pc);
    match crate::app::apply_config(&state, config).await {
        Ok(()) => Json(serde_json::json!({ "status": "ok", "provider": name })).into_response(),
        Err(e) => error_response(KgError::internal(e)),
    }
}

/// `DELETE /api/config/providers/{name}` — remove a provider, persist, hot-reload.
pub async fn delete_provider(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Response {
    let mut config = (*state.config.load_full()).clone();
    if config.providers.remove(&name).is_none() {
        return error_response(KgError::new(
            KgErrorKind::BadRequest,
            format!("provider '{name}' not found"),
        ));
    }
    match crate::app::apply_config(&state, config).await {
        Ok(()) => Json(serde_json::json!({ "status": "ok", "deleted": name })).into_response(),
        Err(e) => error_response(KgError::internal(e)),
    }
}

/// `GET /api/config/virtual-keys` — configured virtual keys (admin-only).
pub async fn get_config_vkeys(State(state): State<SharedState>) -> Response {
    let config = state.config.load_full();
    Json(serde_json::json!({ "virtual_keys": config.virtual_keys })).into_response()
}

/// `PUT /api/config/virtual-keys/{id}` — create/update a virtual key, persist, hot-reload.
///
/// Note: adding the first virtual key switches governance to STRICT mode — all requests
/// then require `Authorization: Bearer <vk>`.
pub async fn put_vkey(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(input): Json<VirtualKeyInput>,
) -> Response {
    if id.trim().is_empty() {
        return error_response(KgError::new(
            KgErrorKind::BadRequest,
            "virtual key id required",
        ));
    }
    let vk = input.into_config(id.clone());
    let mut config = (*state.config.load_full()).clone();
    match config.virtual_keys.iter_mut().find(|k| k.id == id) {
        Some(existing) => *existing = vk,
        None => config.virtual_keys.push(vk),
    }
    match crate::app::apply_config(&state, config).await {
        Ok(()) => Json(serde_json::json!({ "status": "ok", "virtual_key": id })).into_response(),
        Err(e) => error_response(KgError::internal(e)),
    }
}

/// `DELETE /api/config/virtual-keys/{id}` — remove a virtual key, persist, hot-reload.
pub async fn delete_vkey(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    let mut config = (*state.config.load_full()).clone();
    let before = config.virtual_keys.len();
    config.virtual_keys.retain(|k| k.id != id);
    if config.virtual_keys.len() == before {
        return error_response(KgError::new(
            KgErrorKind::BadRequest,
            format!("virtual key '{id}' not found"),
        ));
    }
    match crate::app::apply_config(&state, config).await {
        Ok(()) => Json(serde_json::json!({ "status": "ok", "deleted": id })).into_response(),
        Err(e) => error_response(KgError::internal(e)),
    }
}

/// `POST /v1/images/generations` — OpenAI-compatible image generation.
pub async fn images_generations(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<ImageGenerationRequest>,
) -> Response {
    let mut ctx = Ctx::new();
    ctx.virtual_key = vkey_from_headers(&headers);
    crate::otel::apply_trace_context(&mut ctx, &headers);
    match state.engine.load_full().image_generate(&ctx, req).await {
        Ok(resp) => Json(serde_json::json!({ "data": resp.data })).into_response(),
        Err(e) => error_response(e),
    }
}

/// `POST /v1/audio/speech` — text-to-speech; returns raw audio bytes.
pub async fn audio_speech(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<SpeechRequest>,
) -> Response {
    let mut ctx = Ctx::new();
    ctx.virtual_key = vkey_from_headers(&headers);
    crate::otel::apply_trace_context(&mut ctx, &headers);
    match state.engine.load_full().speech(&ctx, req).await {
        Ok(resp) => (
            [(axum::http::header::CONTENT_TYPE, resp.content_type)],
            resp.audio,
        )
            .into_response(),
        Err(e) => error_response(e),
    }
}

/// `POST /v1/audio/transcriptions` — multipart (file + model) → transcribed text.
pub async fn audio_transcriptions(
    State(state): State<SharedState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    let mut audio: Vec<u8> = Vec::new();
    let mut filename = "audio".to_string();
    let mut model = String::new();

    while let Ok(Some(field)) = multipart.next_field().await {
        match field.name() {
            Some("file") => {
                if let Some(fname) = field.file_name() {
                    filename = fname.to_string();
                }
                audio = field.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
            }
            Some("model") => model = field.text().await.unwrap_or_default(),
            _ => {}
        }
    }

    if model.is_empty() || audio.is_empty() {
        return error_response(KgError::new(
            KgErrorKind::BadRequest,
            "transcription requires 'model' and 'file' fields",
        ));
    }

    let mut ctx = Ctx::new();
    ctx.virtual_key = vkey_from_headers(&headers);
    crate::otel::apply_trace_context(&mut ctx, &headers);
    let req = TranscriptionRequest {
        model,
        audio,
        filename,
    };
    match state.engine.load_full().transcribe(&ctx, req).await {
        Ok(resp) => Json(serde_json::json!({ "text": resp.text })).into_response(),
        Err(e) => error_response(e),
    }
}

/// `POST /v1/rerank` — rerank documents by relevance to a query.
pub async fn rerank(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<RerankRequest>,
) -> Response {
    let mut ctx = Ctx::new();
    ctx.virtual_key = vkey_from_headers(&headers);
    crate::otel::apply_trace_context(&mut ctx, &headers);
    match state.engine.load_full().rerank(&ctx, req).await {
        Ok(resp) => Json(serde_json::json!({ "results": resp.results })).into_response(),
        Err(e) => error_response(e),
    }
}

/// Generic message returned to clients for upstream provider/network errors — the real
/// upstream error body is never forwarded, since some providers echo back masked key
/// fragments or other sensitive details in their error responses.
const UPSTREAM_ERROR_MESSAGE: &str = "upstream provider error";

fn error_body(e: &KgError) -> serde_json::Value {
    let message = match e.kind {
        KgErrorKind::Provider | KgErrorKind::Network => {
            // Log the real detail server-side so operators can still diagnose it, but
            // never forward the raw upstream body to the client.
            tracing::warn!(
                provider = ?e.provider,
                status = ?e.status,
                detail = %e.message,
                "upstream error"
            );
            UPSTREAM_ERROR_MESSAGE.to_string()
        }
        _ => e.message.clone(),
    };
    serde_json::json!({
        "error": {
            "message": message,
            "type": format!("{:?}", e.kind).to_lowercase(),
            "provider": e.provider,
        }
    })
}

fn error_response(e: KgError) -> Response {
    let status = axum::http::StatusCode::from_u16(e.http_status())
        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(error_body(&e))).into_response()
}

/// 500 response for a log-store failure, matching the gateway error JSON shape.
fn store_error_response(e: kgateway_store::StoreError) -> Response {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": { "message": e.to_string(), "type": "internal" }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod error_body_tests {
    use super::*;

    #[test]
    fn scrubs_upstream_provider_error_bodies() {
        let e = KgError::provider("SECRET-LEAK sk-abc123", 400);
        let body = error_body(&e);
        let message = body["error"]["message"].as_str().unwrap();
        assert_eq!(message, UPSTREAM_ERROR_MESSAGE);
        assert!(!message.contains("SECRET-LEAK"));
        assert!(!message.contains("sk-abc123"));
    }

    #[test]
    fn scrubs_network_error_bodies() {
        let e = KgError::new(KgErrorKind::Network, "connection reset by 10.0.0.5:443");
        let body = error_body(&e);
        let message = body["error"]["message"].as_str().unwrap();
        assert_eq!(message, UPSTREAM_ERROR_MESSAGE);
        assert!(!message.contains("10.0.0.5"));
    }

    #[test]
    fn passes_through_gateway_error_messages() {
        let e = KgError::new(KgErrorKind::BadRequest, "bad model");
        let body = error_body(&e);
        assert_eq!(body["error"]["message"].as_str().unwrap(), "bad model");
    }
}

#[cfg(test)]
mod logs_tests {
    use super::*;
    use kgateway_store::{LogStore, MemoryLogStore, RequestLog};

    fn params(pairs: &[(&str, &str)]) -> LogsParams {
        // Round-trip through the same querystring deserializer axum's `Query` uses.
        let qs = pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        serde_urlencoded::from_str(&qs).expect("parse params")
    }

    #[test]
    fn limit_defaults_and_clamps_to_max() {
        assert_eq!(params(&[]).to_query().limit, DEFAULT_LOG_LIMIT);
        assert_eq!(params(&[("limit", "10")]).to_query().limit, 10);
        // Over the cap → clamped to MAX_LOG_LIMIT (200).
        assert_eq!(params(&[("limit", "9999")]).to_query().limit, MAX_LOG_LIMIT);
    }

    #[test]
    fn sort_by_and_order_parse() {
        let q = params(&[("sort_by", "latency"), ("order", "asc")]).to_query();
        assert_eq!(q.sort_by, SortBy::Latency);
        assert!(!q.descending);

        let q = params(&[("sort_by", "cost")]).to_query();
        assert_eq!(q.sort_by, SortBy::Cost);
        assert!(q.descending); // default desc

        // Unknown sort_by falls back to CreatedAt.
        let q = params(&[("sort_by", "bogus")]).to_query();
        assert_eq!(q.sort_by, SortBy::CreatedAt);
    }

    #[test]
    fn cache_hit_and_filters_parse() {
        let f = params(&[
            ("cache_hit", "true"),
            ("provider", "openai"),
            ("status", "200"),
            ("since_ms", "1234"),
        ])
        .to_filter();
        assert_eq!(f.cache_hit, Some(true));
        assert_eq!(f.provider.as_deref(), Some("openai"));
        assert_eq!(f.status, Some(200));
        assert_eq!(f.since_ms, Some(1234));

        assert_eq!(
            params(&[("cache_hit", "false")]).to_filter().cache_hit,
            Some(false)
        );
        // Empty strings are dropped, not treated as a filter.
        assert_eq!(params(&[("provider", "")]).to_filter().provider, None);
    }

    fn log(
        id: &str,
        provider: &str,
        status: u16,
        prompt: u32,
        cost: f64,
        cache_hit: bool,
    ) -> RequestLog {
        RequestLog {
            request_id: id.to_string(),
            created_at: 0,
            virtual_key: None,
            provider: provider.to_string(),
            model: "gpt-4".to_string(),
            status,
            prompt_tokens: prompt,
            completion_tokens: 0,
            latency_ms: 10,
            cost: Some(cost),
            stream: false,
            cache_hit,
            stop_reason: None,
            error_message: None,
            request_body: None,
            response_body: None,
            redacted: false,
            redaction_mapping: None,
        }
    }

    #[tokio::test]
    async fn stats_aggregates_over_filter() {
        let store = MemoryLogStore::default();
        store
            .append(log("a", "openai", 200, 10, 0.01, true))
            .await
            .unwrap();
        store
            .append(log("b", "openai", 500, 20, 0.02, false))
            .await
            .unwrap();
        store
            .append(log("c", "anthropic", 200, 30, 0.03, false))
            .await
            .unwrap();

        // No filter → all three.
        let all = store.stats(&LogFilter::default()).await.unwrap();
        assert_eq!(all.total, 3);
        assert_eq!(all.success, 2);
        assert_eq!(all.errors, 1);
        assert_eq!(all.total_tokens, 60);
        assert_eq!(all.cache_hits, 1);
        assert!((all.total_cost - 0.06).abs() < 1e-9);

        // Filter to openai only → two rows.
        let filter = params(&[("provider", "openai")]).to_filter();
        let openai = store.stats(&filter).await.unwrap();
        assert_eq!(openai.total, 2);
        assert_eq!(openai.success, 1);
        assert_eq!(openai.errors, 1);
        assert_eq!(openai.total_tokens, 30);
    }

    #[tokio::test]
    async fn query_applies_filter_sort_and_paging() {
        let store = MemoryLogStore::default();
        store
            .append(log("a", "openai", 200, 10, 0.01, false))
            .await
            .unwrap();
        store
            .append(log("b", "openai", 200, 30, 0.03, false))
            .await
            .unwrap();
        store
            .append(log("c", "anthropic", 200, 20, 0.02, false))
            .await
            .unwrap();

        // provider=openai, sort by tokens ascending.
        let q = params(&[
            ("provider", "openai"),
            ("sort_by", "tokens"),
            ("order", "asc"),
        ])
        .to_query();
        let page = store.query(&q).await.unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.logs.len(), 2);
        assert_eq!(page.logs[0].request_id, "a"); // 10 tokens before 30
        assert_eq!(page.logs[1].request_id, "b");
    }
}
