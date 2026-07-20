# 16 — Configuration Reference

The authoritative reference for `config.json` — every field the gateway reads, its type,
default, and meaning. The schema is defined by the Rust `Config` struct in
[`crates/kgateway-server/src/config.rs`](../crates/kgateway-server/src/config.rs); this doc
tracks it. When this doc and the code disagree, the code wins.

For a fast local walkthrough (build + run + first request) see
[08-getting-started.md](./08-getting-started.md). For deployment (Docker / Helm) see
[06-deployment.md](./06-deployment.md).

## Loading & `${ENV}` interpolation

The server reads a single JSON file (`--config config.json`). Any string value may embed
`${ENV_VAR}` references, which are resolved **at load time** — an unset variable expands to the
empty string. Interpolation is used for secrets and connection strings: key `value`s,
`admin_token`, `api_tokens[].token`, `redaction.key`, and `database`. Raw `${ENV}` references are
preserved when the config is written back out (live edits via the control-plane API), so no
resolved secret is ever persisted to disk.

Unknown top-level keys are ignored. Every field is optional except where noted; an empty
`{}` boots a valid (provider-less) gateway on port 8080 with an in-memory log store.

## Top-level `Config`

| Field | Type | Default | Description |
|---|---|---|---|
| `providers` | map<string, [ProviderConfig](#providerconfig)> | `{}` | Upstream LLM providers, keyed by the name used in the `provider/model` route prefix. |
| `port` | u16 | `8080` | TCP listen port. Changing it requires a restart (not hot-reloadable). |
| `database` | string? | *none* (in-memory) | SQLite or Postgres URL for request-log persistence. `sqlite://` → `SqliteLogStore`, `postgres://` → `PostgresLogStore`. Supports `${ENV}`. |
| `admin_token` | string? | *none* (control plane open) | Legacy single control-plane token. When set, `/api/*` and `/metrics` require `Authorization: Bearer <token>`; treated as an **admin**-role token. Supports `${ENV}`. |
| `api_tokens` | [[ApiTokenConfig](#apitokenconfig--role)] | `[]` | RBAC bearer-token → role bindings (M11). Coexist with `admin_token`. |
| `virtual_keys` | [[VirtualKeyConfig](#virtualkeyconfig)] | `[]` | Data-plane governance keys. When non-empty, governance runs in **strict** mode (every request must present a known `Authorization: Bearer <id>`). |
| `semantic_cache` | [SemanticCacheConfig](#semanticcacheconfig)? | *none* (off) | Embedding-similarity response cache. |
| `mcp` | [McpConfig](#mcpconfig)? | *none* (off) | Model Context Protocol tool gateway for agentic tool-calling. |
| `request_timeout_secs` | u64? | *none* (`120`) | Global per-request timeout in seconds; exceeding it returns `408 Request Timeout`. |
| `public_url` | string? | *none* (derived from `Host`) | Origin this gateway is reached at, e.g. `https://gw.example.com`. Used as the base URL in the generated docs (`/openapi.json`, `/llms.txt`, examples). **Set this in production:** with it unset the base URL comes from the request's `Host`, which is attacker-controlled, so a spoofed header behind a cache that doesn't vary on it could hand readers a spec pointing at another origin. |
| `cors_allow_origins` | [string]? | *none* (permissive) | Explicit CORS allow-list. When unset or empty, any origin is allowed (fine for local dev; set it in production). |
| `log_retention_days` | u32? | *none* (kept forever) | When set and > 0, a background sweep deletes logs older than this (hourly, first sweep at startup). Re-read from live config, so hot-reload needs no restart. |
| `content_logging` | [ContentLoggingConfig](#contentloggingconfig)? | *none* (off) | Opt-in request/response **body** capture (M10 Phase 2). Bodies are admin-only. |
| `redaction` | [RedactionConfig](#redactionconfig)? | *none* (off) | Reversible redaction of captured bodies (M11). |
| `otlp` | [OtlpConfig](#otlpconfig)? | *none* (off) | OTLP/OpenTelemetry export of traces + metrics (M15). |

Note: the JSON is order-independent; the table above orders fields for readability, not to match the file.

> **Request tracing needs no configuration.** Every request records per-stage trace spans
> (governance, cache, each dispatch attempt, time-to-first-token, stream body) that power the
> dashboard's call waterfall. Unlike content capture there is nothing to opt into: spans hold
> only stage names, timings, and outcomes — never upstream error text or request content — so
> they carry no disclosure risk. They are bounded (256 spans/request, 400 bytes/detail) and
> returned **only** by `GET /api/logs/{id}`, never on list or live-tail responses.

---

## `ProviderConfig`

One entry per upstream. The map key (e.g. `"openai"`, `"anthropic"`, `"groq"`, `"zai-coding"`,
or any custom name) is what clients use as the `provider/` prefix in the `model` field. Known
OpenAI-compatible names (see [03-providers.md](./03-providers.md)) need no `base_url` — just keys.

| Field | Type | Default | Description |
|---|---|---|---|
| `kind` | string? | *inferred from name* | Wire format for a custom-named provider: `"openai"` (OpenAI-compatible) or `"anthropic"` (Anthropic Messages API, e.g. z.ai's GLM Coding Plan). Also selects native connectors like `"bedrock"` / `"gemini"` / `"azure"`. When omitted, inferred from the provider name. |
| `base_url` | string? | *provider default* | Base URL override for OpenAI/Anthropic-compatible or self-hosted endpoints. |
| `keys` | [[KeyConfig](#keyconfig)] | `[]` | One or more API keys (weighted load-balancing + model filtering). |

### `KeyConfig`

| Field | Type | Default | Description |
|---|---|---|---|
| `id` | string | `"default"` | Key identifier (appears in logs / key selection). |
| `value` | string | *(required)* | The API key. May contain `${ENV_VAR}`, resolved at load. |
| `weight` | u32 | `1` | Relative weight for weighted-random key selection. |
| `models` | [string] | `[]` | Optional model allow-list for this key. Empty = eligible for all models on the provider. |

---

## `VirtualKeyConfig`

Data-plane governance. Clients present `Authorization: Bearer <id>`. Adding the first virtual
key flips governance to **strict** mode (requests without a known key get `401`).

| Field | Type | Default | Description |
|---|---|---|---|
| `id` | string | *(required)* | The bearer token clients send (e.g. `"vk_team_alpha"`). |
| `name` | string | `""` | Human label. |
| `allowed_models` | [string] | `[]` | Model allow-list (`provider/model` or bare model id). Empty = all models allowed. A disallowed model returns `400`. |
| `denied_models` | [string] | `[]` | Model deny-list. A match here is **always** rejected (`400`) — deny wins over the allow-list. |
| `max_requests_per_min` | u32? | *none* (unlimited) | Fixed 60s-window rate limit; trips `429`. |
| `max_total_tokens` | u64? | *none* (unlimited) | Cumulative token budget; exhaustion rejects further requests (`429`). |
| `max_cost_per_period` | f64? | *none* (unlimited) | Max estimated USD cost per rolling period; exhaustion trips `429`. Cost is estimated from the static price table (unpriced models accrue nothing). |
| `max_cost_period_secs` | u64? | `60` | Length of the cost-budget period (tumbling window), in seconds — only meaningful with `max_cost_per_period`. |

> **Shared counters (horizontal scaling).** Rate-limit / token / cost counters are per-replica by
> default. When `database` is a Postgres URL, they are shared across replicas via the DB, so a
> limit stays correct no matter how many gateway instances run. Windows are tumbling.

---

## `ApiTokenConfig` + `Role`

Control-plane RBAC (M11). Each token maps to a role; roles carry a fixed permission set. The
legacy `admin_token`, when set, is treated as an additional `admin` token (backward-compatible).

### `ApiTokenConfig`

| Field | Type | Default | Description |
|---|---|---|---|
| `token` | string | *(required)* | Bearer token. Supports `${ENV}`. |
| `role` | [Role](#role) | `"viewer"` | Role granted to this token. |
| `name` | string | `""` | Human label, surfaced in reveal audit logs (`revealed_by`). |

### `Role`

Serialized lowercase. Permissions are cumulative (each role includes the ones below it).

| Role | Permissions | Permits |
|---|---|---|
| `viewer` (default) | `logs:view`, `metrics:view` | Read logs, metrics, analytics, and config. Least privilege. |
| `operator` | viewer + `config:write` | Also mutate config — add/edit/remove providers and virtual keys. |
| `admin` | operator + `logs:reveal` | Also **reveal** redacted log content (`GET /api/logs/{id}/reveal`). |

Enforcement: `401` for an unknown token, `403` for a known token lacking the route's required
permission. **Fail-closed:** if `api_tokens` are declared but every token resolves empty (e.g. a
broken `${ENV}` injection), the control plane *locks* (every request `401`) and logs a startup
`error!` — it does not silently open. Only a config with no tokens at all runs the control plane
open (with a startup warning).

---

## `SemanticCacheConfig`

Embedding-similarity response cache. A cache hit short-circuits the upstream call.

| Field | Type | Default | Description |
|---|---|---|---|
| `embedding_provider` | string | *(required)* | Provider used to embed requests — must support embeddings (`"openai"` or an OpenAI-compatible one). Reuses that provider's `base_url` + first key. |
| `embedding_model` | string | *(required)* | Embedding model name. |
| `threshold` | f32 | `0.95` | Minimum cosine similarity (0..1) for a hit. |

**Backend + behavior:** the cache is a **two-tier** lookup — an O(1) exact-match tier (identical
repeats skip embedding) then the embedding-similarity tier, **scoped by model + sampling params +
tools** (so a hit is never served across different `temperature`/params). The vector store is
**auto-selected**: a persistent Postgres + `pgvector` store when `database` is a Postgres URL
(survives restarts, shared across replicas — requires the `vector` extension; falls back to
in-memory with a warning if unavailable), else an in-memory store.

---

## `McpConfig`

Model Context Protocol tool gateway for agentic tool-calling.

| Field | Type | Default | Description |
|---|---|---|---|
| `builtin_tools` | bool | `false` | Register an in-process demo tool set (an `echo` tool) so tool-calling can be exercised without an external server. |
| `servers` | [[McpServerConfig](#mcpserverconfig)] | `[]` | External MCP tool servers spawned as stdio subprocesses. |

### `McpServerConfig`

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | *(required)* | Label shown in logs and `GET /api/mcp/tools`. |
| `command` | string | *(required)* | Executable to spawn (the MCP server). |
| `args` | [string] | `[]` | Arguments passed to the command. |

---

## `ContentLoggingConfig`

Opt-in capture of request/response **bodies** (M10 Phase 2). Off unless present with
`enabled: true`. Captured bodies can carry secrets/PII and are returned **only** from the
admin-guarded detail endpoint (`GET /api/logs/{id}`) — never from list queries or the SSE tail.
See [12-content-capture-plan.md](./12-content-capture-plan.md).

| Field | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Master switch. When false, no bodies are captured (zero-cost). |
| `max_body_bytes` | usize | `16384` (16 KiB) | Per-body truncation budget in bytes. `0` disables truncation entirely — bodies are captured **in full** (accepting unbounded log-store growth and, for streamed responses, holding the whole completion in memory per active stream). |
| `capture_streaming` | bool | `false` | Capture the assembled response of streamed chat (tee + accumulate). When false, streamed requests capture the request body only. |

---

## `RedactionConfig`

Reversible redaction of captured bodies (M11). Off unless present with `enabled: true`. Strongly
recommended whenever `content_logging.enabled` is true. See
[13-redaction-rbac-plan.md](./13-redaction-rbac-plan.md).

| Field | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Master switch. When false, no redaction is applied. |
| `key` | string? | *none* (mask-only) | Passphrase/key for the reversible AES-256-GCM mapping (`${ENV}` supported). Without it, redaction still masks but the mapping is dropped and **reveal is unavailable**. Never blocks boot. |
| `patterns` | [string] | `[]` | Extra regex patterns to redact, in addition to the built-in set (emails, JWTs, `sk-`/API-key shapes, AWS keys, bearer tokens, card/phone/IP patterns). |

---

## `OtlpConfig`

OTLP / OpenTelemetry export (M15). Off unless present with `enabled: true`. Transport is
OTLP-over-HTTP (protobuf); the signal paths `/v1/traces` and `/v1/metrics` are appended to
`endpoint`. Emits one span per request plus request/latency/token metrics. See
[17-otlp-plan.md](./17-otlp-plan.md).

| Field | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `false` | Master switch. When false, no OTel SDK is initialized. |
| `endpoint` | string | `http://localhost:4318` | OTLP HTTP base endpoint of your collector. |
| `service_name` | string | `kgateway` | `service.name` resource attribute. |
| `traces` | bool | `true` | Export a span per request (start/end from request timing; provider/model/status/token/stream/cache_hit/vkey/error attributes). |
| `metrics` | bool | `true` | Export a request counter, latency histogram (ms), and token counter. |

---

## Complete annotated example

A valid `config.json` exercising the major features — two providers (one OpenAI-compatible with a
`base_url`, one Anthropic-wire custom provider via `kind`), virtual keys, all three RBAC roles,
content capture, redaction with a key and a custom pattern, retention, semantic cache, and a
persistent database. Every field name matches `config.rs`. JSON does not allow comments, so this
block is valid as-is; what each block does is annotated below it.

```json
{
  "port": 8080,
  "database": "sqlite:///var/lib/kgateway/kgateway.db?mode=rwc",
  "request_timeout_secs": 120,
  "cors_allow_origins": ["https://dashboard.example.com"],
  "log_retention_days": 30,

  "admin_token": "${KGATEWAY_ADMIN_TOKEN}",
  "api_tokens": [
    { "token": "${KG_VIEWER_TOKEN}",   "role": "viewer",   "name": "ci-dashboards" },
    { "token": "${KG_OPERATOR_TOKEN}", "role": "operator", "name": "platform-team" },
    { "token": "${KG_ADMIN_TOKEN}",    "role": "admin",    "name": "sre-oncall" }
  ],

  "providers": {
    "openai": {
      "keys": [
        { "id": "default", "value": "${OPENAI_API_KEY}", "weight": 1 }
      ]
    },
    "groq": {
      "base_url": "https://api.groq.com/openai/v1",
      "keys": [
        { "id": "default", "value": "${GROQ_API_KEY}", "weight": 1, "models": ["llama-3.1-70b"] }
      ]
    },
    "zai": {
      "kind": "anthropic",
      "base_url": "https://api.z.ai/api/anthropic",
      "keys": [
        { "id": "coding-plan", "value": "${ZAI_API_KEY}", "weight": 1 }
      ]
    }
  },

  "virtual_keys": [
    {
      "id": "vk_team_alpha",
      "name": "Team Alpha",
      "allowed_models": ["openai/gpt-4o", "zai/glm-4.6"],
      "denied_models": [],
      "max_requests_per_min": 60,
      "max_total_tokens": 1000000,
      "max_cost_per_period": 50.0,
      "max_cost_period_secs": 86400
    }
  ],

  "semantic_cache": {
    "embedding_provider": "openai",
    "embedding_model": "text-embedding-3-small",
    "threshold": 0.95
  },

  "content_logging": {
    "enabled": true,
    "max_body_bytes": 16384,
    "capture_streaming": true
  },

  "redaction": {
    "enabled": true,
    "key": "${KGATEWAY_REDACTION_KEY}",
    "patterns": ["ACME-[A-Z0-9]{16}"]
  }
}
```

What it exercises:

- **Persistence + retention** — SQLite store with a 30-day retention sweep.
- **RBAC** — `admin_token` plus three tokens spanning `viewer` / `operator` / `admin`; all secrets via `${ENV}`.
- **Providers** — `openai` (default wire), `groq` (OpenAI-compatible with a `base_url` and a per-key model allow-list), `zai` (Anthropic wire via `kind`).
- **Governance** — one virtual key with a model allow-list, rate limit, and token budget (its presence enables strict mode).
- **Semantic cache** — OpenAI embeddings at a 0.95 similarity threshold.
- **Content capture + redaction** — bodies captured (incl. streamed responses), then redacted with the built-in set plus a custom `ACME-…` license-key pattern, reversible via the configured key.

---

## Security notes

- **`redaction.key` must be high-entropy.** The key is derived with a single unsalted SHA-256 (treated as key material, not a password). If the database is exfiltrated *and* a low-entropy key was chosen, the encrypted mappings are cheaply brute-forceable. A slow salted KDF and key rotation are deferred — pick a strong, random `redaction.key` and inject it via `${ENV}`.
- **Content capture stores raw bodies.** `content_logging` records request/response payloads that can contain secrets and PII. They are admin-only (returned solely by `GET /api/logs/{id}`, excluded from list queries and the SSE tail). Enable `redaction` alongside it so bodies don't sit in the store in the clear.
- **RBAC is fail-closed.** Declaring `api_tokens` whose values all resolve empty locks the control plane (every request `401`) rather than opening it. Only a config with no tokens at all runs open — and then a startup warning is logged. Set `admin_token` (or `api_tokens`) in any production deployment, and keep the gateway behind TLS / an ingress you control.
- **Reveal is privileged and audited.** Un-redacting a body (`GET /api/logs/{id}/reveal`) requires the `logs:reveal` permission (admin only) and is itself audit-logged with the caller's token name/role.
</content>
</invoke>
