//! Config loading. Reads `config.json`, interpolates `${ENV_VAR}` in key values.
//! Uses a `config.json` schema (trimmed to M1). See `docs/06-deployment.md`.

use kgateway_core::provider::ApiKey;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Optional SQLite/Postgres URL for request-log persistence. When absent, an
    /// in-memory store is used. Supports `${ENV}` interpolation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Optional admin token. When set, the control-plane routes (`/api/*`, `/metrics`)
    /// require `Authorization: Bearer <token>`. When absent they are open (a warning is
    /// logged at startup). Supports `${ENV}` interpolation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_token: Option<String>,
    /// Virtual keys for governance. When present, the governance plugin is enabled in
    /// strict mode (requests must present a known `Authorization: Bearer <id>`).
    #[serde(default)]
    pub virtual_keys: Vec<VirtualKeyConfig>,
    /// Optional semantic (embedding-similarity) response cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_cache: Option<SemanticCacheConfig>,
    /// Optional MCP (Model Context Protocol) tool gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpConfig>,
    /// Global per-request timeout, in seconds. Requests exceeding this return `408
    /// Request Timeout`. Defaults to 120s when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_secs: Option<u64>,
    /// Explicit CORS allow-list (origins, e.g. `"https://app.example.com"`). When unset
    /// or empty, CORS falls back to permissive (any origin) — fine for local dev, but a
    /// production deployment should set this explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors_allow_origins: Option<Vec<String>>,
    /// Request-log retention, in days. When set (and > 0), a background task periodically
    /// deletes logs older than this. When unset, logs are kept indefinitely (fine for
    /// dev/in-memory; set this for any durable deployment so the table can't grow without
    /// bound).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_retention_days: Option<u32>,
    /// Request/response **content capture** (M10 Phase 2). Off unless present and
    /// `enabled`. Captured bodies can carry secrets/PII, so this is opt-in and only ever
    /// returned from the admin-guarded detail endpoint. See `docs/12-content-capture-plan.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_logging: Option<ContentLoggingConfig>,
    /// RBAC tokens (M11). Each maps a bearer token to a role. In addition to (and
    /// alongside) the legacy single `admin_token`, which is treated as an `admin` token.
    #[serde(default)]
    pub api_tokens: Vec<ApiTokenConfig>,
    /// Redaction of captured bodies (M11). Off unless present and `enabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redaction: Option<RedactionConfig>,
    /// OTLP / OpenTelemetry export (M15). Off unless present and `enabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp: Option<OtlpConfig>,
}

/// OTLP export settings (M15). Traces + metrics over OTLP HTTP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtlpConfig {
    /// Master switch. When false (default), no OTel SDK is initialized.
    #[serde(default)]
    pub enabled: bool,
    /// OTLP HTTP base endpoint (signal paths `/v1/traces`, `/v1/metrics` are appended).
    #[serde(default = "default_otlp_endpoint")]
    pub endpoint: String,
    /// `service.name` resource attribute.
    #[serde(default = "default_otlp_service_name")]
    pub service_name: String,
    /// Export spans (one per request).
    #[serde(default = "default_true")]
    pub traces: bool,
    /// Export metrics (request counter, latency histogram, token counter).
    #[serde(default = "default_true")]
    pub metrics: bool,
}

fn default_otlp_endpoint() -> String {
    "http://localhost:4318".to_string()
}
fn default_otlp_service_name() -> String {
    "kgateway".to_string()
}
fn default_true() -> bool {
    true
}

/// Control-plane role. Permissions are derived from this (see `auth::Role::permits`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Read logs/metrics/config. Least privilege — the default for a token with no `role`.
    #[default]
    Viewer,
    /// Viewer + mutate config (providers / virtual keys).
    Operator,
    /// Operator + reveal redacted log content (`logs:reveal`).
    Admin,
}

/// One RBAC bearer token → role binding. `token` supports `${ENV}` interpolation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiTokenConfig {
    pub token: String,
    #[serde(default)]
    pub role: Role,
    /// Human label for audit logs (optional).
    #[serde(default)]
    pub name: String,
}

/// Redaction settings (M11). Maps onto `kgateway_plugins::redaction::Redactor`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactionConfig {
    /// Master switch. When false (default), no redaction is applied.
    #[serde(default)]
    pub enabled: bool,
    /// Encryption key/passphrase for the reversible mapping (`${ENV}` supported). When
    /// unset, redaction still masks but the mapping is dropped (reveal unavailable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Extra regex patterns to redact, in addition to the built-in set.
    #[serde(default)]
    pub patterns: Vec<String>,
}

/// Content-capture settings. Maps onto `kgateway_core::engine::ContentCapture`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentLoggingConfig {
    /// Master switch. When false (the default), no payloads are captured.
    #[serde(default)]
    pub enabled: bool,
    /// Per-body truncation budget in bytes. Defaults to 16 KiB. `0` disables truncation
    /// (bodies are captured in full).
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    /// Capture the assembled response of streamed chat (tee + accumulate). When false,
    /// streamed requests capture the request body only.
    #[serde(default)]
    pub capture_streaming: bool,
}

fn default_max_body_bytes() -> usize {
    16 * 1024
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    /// Register a built-in in-process tool set (currently an `echo` tool) so agentic
    /// tool-calling can be exercised without an external MCP server.
    #[serde(default)]
    pub builtin_tools: bool,
    /// External MCP tool servers to connect over stdio (spawned as subprocesses).
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Label for logs / `/api/mcp/tools`.
    pub name: String,
    /// Executable to spawn (the MCP server).
    pub command: String,
    /// Arguments passed to the command.
    #[serde(default)]
    pub args: Vec<String>,
}

fn default_port() -> u16 {
    8080
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticCacheConfig {
    /// Provider used to embed requests (must support embeddings — "openai" or an
    /// OpenAI-compatible one). Reuses that provider's base_url + first key.
    pub embedding_provider: String,
    pub embedding_model: String,
    /// Minimum cosine similarity (0..1) for a cache hit.
    #[serde(default = "default_cache_threshold")]
    pub threshold: f32,
}

fn default_cache_threshold() -> f32 {
    0.95
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VirtualKeyConfig {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub allowed_models: Vec<String>,
    #[serde(default)]
    pub denied_models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests_per_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_tokens: Option<u64>,
    /// Max estimated USD cost per rolling period. `None` = unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_per_period: Option<f64>,
    /// Length of the cost-budget period in seconds (defaults to 60 when a cost budget is set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_period_secs: Option<u64>,
}

/// Body for the virtual-key write API — the `id` comes from the URL path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualKeyInput {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub allowed_models: Vec<String>,
    #[serde(default)]
    pub denied_models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests_per_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_per_period: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_period_secs: Option<u64>,
}

impl VirtualKeyInput {
    pub fn into_config(self, id: String) -> VirtualKeyConfig {
        VirtualKeyConfig {
            id,
            name: self.name,
            allowed_models: self.allowed_models,
            denied_models: self.denied_models,
            max_requests_per_min: self.max_requests_per_min,
            max_total_tokens: self.max_total_tokens,
            max_cost_per_period: self.max_cost_per_period,
            max_cost_period_secs: self.max_cost_period_secs,
        }
    }
}

// Manual Default so the empty-config fallback uses the real default port (8080),
// not u16's 0 — `#[derive(Default)]` would ignore the serde default.
impl Default for Config {
    fn default() -> Self {
        Self {
            providers: HashMap::new(),
            port: default_port(),
            database: None,
            admin_token: None,
            virtual_keys: Vec::new(),
            semantic_cache: None,
            mcp: None,
            request_timeout_secs: None,
            cors_allow_origins: None,
            log_retention_days: None,
            content_logging: None,
            api_tokens: Vec::new(),
            redaction: None,
            otlp: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Wire format for a custom-named provider: `"openai"` (OpenAI-compatible) or
    /// `"anthropic"` (Anthropic Messages API — e.g. z.ai's GLM Coding Plan). When
    /// omitted, the format is inferred from the provider name (openai/anthropic/cohere/
    /// known compat vendors).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Optional base URL override (for openai/anthropic-compatible / self-hosted providers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default)]
    pub keys: Vec<KeyConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyConfig {
    #[serde(default = "default_key_id")]
    pub id: String,
    /// May contain `${ENV_VAR}` which is resolved at load time.
    pub value: String,
    #[serde(default = "default_weight")]
    pub weight: u32,
    #[serde(default)]
    pub models: Vec<String>,
}

fn default_key_id() -> String {
    "default".to_string()
}
fn default_weight() -> u32 {
    1
}

impl KeyConfig {
    /// Resolve `${ENV}` references in the key value.
    pub fn resolve(&self) -> ApiKey {
        ApiKey {
            id: self.id.clone(),
            value: interpolate_env(&self.value),
            weight: self.weight,
            models: self.models.clone(),
        }
    }
}

/// Replace `${VAR}` occurrences with the environment value (empty if unset).
pub fn interpolate_env(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        if let Some(end) = after.find('}') {
            let var = &after[..end];
            out.push_str(&std::env::var(var).unwrap_or_default());
            rest = &after[end + 1..];
        } else {
            out.push_str(&rest[start..]);
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

impl Config {
    pub fn from_file(path: &str) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Persist the config back to `path` as pretty JSON. Raw values (including `${ENV}`
    /// references) are preserved as-is — env interpolation only happens at resolve time,
    /// so no resolved secret is written out.
    pub fn to_file(&self, path: &str) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_env_vars() {
        std::env::set_var("KG_TEST_KEY", "secret123");
        assert_eq!(interpolate_env("Bearer ${KG_TEST_KEY}"), "Bearer secret123");
        assert_eq!(interpolate_env("no vars here"), "no vars here");
        assert_eq!(interpolate_env("${KG_MISSING_VAR}"), "");
    }
}
