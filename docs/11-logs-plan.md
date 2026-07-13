# 11 — Logs / Observability plan (reference parity)

Gap analysis of a mature reference gateway's logs module vs KGateway's current audit log, and
the plan to reach parity. Milestone entry: `02-roadmap.md` → **M10**.

## The reference logs module = an observability suite (not just a table)

**Data model** — a full request/response audit record, not a metrics row. Notable field groups
KGateway lacks:
- **Full content** (the big one): per-request-type typed input/output — `input_history`,
  `output_message`, `embedding_output`, `rerank_output`, speech/transcription/image/video
  in/out, etc.
- **Cost**: per-log `cost` (indexed) + `token_usage` with `cached_read_tokens` /
  `reasoning_tokens`, plus denormalized token columns for fast analytics.
- **Timing**: `timestamp` (event) + `created_at` (insert), both indexed.
- **Governance attribution**: virtual-key / team / customer / business-unit / user ids+names,
  selected key, routing rule, budgets, rate limits, cluster node.
- **Lineage/session**: `parent_request_id` links retry/fallback + multi-turn into sessions.
- **Diagnostics**: `params`, `tools`, `tool_calls`, `error_details`, `stop_reason`, `stream`,
  `cache_debug` (hit/type/similarity), `plugin_logs`, `routing_engine_logs`.
- **Raw + safety**: `raw_request`/`raw_response` (opt-in config flags), reversible
  `redaction_mapping` gated by `Logs:Reveal` RBAC, `is_large_payload_*` bypass, object-storage
  offload (`HybridLogStore`).
- Backends: SQLite / Postgres / **ClickHouse**, one `LogStore` interface with **40+ methods**.
- Retention: batched background deletion by age + ClickHouse native TTL.

**API** (the reference's logging handler, ~2,468 lines): `/api/logs` (20+ filter dims,
pagination, sort, full-text content search), `/api/logs/{id}` (detail + raw + redaction reveal),
`/api/logs/sessions/{id}[/summary]`, `/api/logs/stats`, `/api/logs/histogram/{tokens,cost,latency,models}`
(+ `/by-provider`, `/by-dimension`), `/api/logs/rankings[/by-dimension]`, `/api/logs/filterdata`,
`/api/logs/dropped`, `/api/logs/dashboard`, bulk `DELETE`, cost-recalculation jobs, and a parallel
`/api/mcp-logs/*` set.

**"Live"**: not a raw content stream — a WS `store_update` event triggers RTK Query cache
invalidation + refetch, backed by a 10s poll fallback.

**UI** (the reference dashboard's logs view): URL-state (nuqs) filters; table with 8 base +
attribution + dynamic metadata columns (sortable, hideable, pinnable); a ~17-section filter
sidebar (each dropdown lazy-fetches distinct values from `/api/logs/filterdata`); a 5-tab detail
sheet (Messages / Tools / Routing / Plugins / Raw) with prev/next nav, session view, redaction gate.

## KGateway today
`RequestLog { request_id, provider, model, status, prompt_tokens, completion_tokens, latency_ms }`
→ written by the `LoggingPlugin` `RequestObserver` on every capability call → `LogStore`
(Memory/SQLite/Postgres). `GET /api/logs` returns the newest 100, no filters/pagination/detail.
UI: a table + client-side substring filter, 4s poll.

**We're better-positioned than a naive read suggests:** KGateway already has virtual keys
(`ctx.virtual_key` is in the observer) and a semantic cache — so vkey attribution and cache-hit
logging are *cheap*, not blocked.

## The one hard change
`RequestObserver::on_response` receives only a `CallRecord` summary (`crates/kgateway-core/src/observer.rs`)
— not the request/response payloads. Capturing content requires widening `CallRecord` (or adding an
`on_request` hook) to thread the request/response through, plus:
- **Async batched writes** — a bounded `tokio::mpsc` + a background writer task, NOT an inline DB
  write in the observer (hot-path latency).
- **Off by default / opt-in / admin-only** — raw messages can carry secrets/PII; KGateway has no
  reversible-redaction/RBAC yet.

## Plan (see M10 in the roadmap)
- **MVP:** scalar fields (`created_at`, `virtual_key`, `stream`, `stop_reason`, `error_message`,
  `cost` via a static pricing table, `cache_hit`) + retention job; filtered/paginated `/api/logs`
  + `/api/logs/{id}` + `/api/logs/stats`; UI filter sidebar + pagination + detail drawer + **SSE
  live tail**. No core-engine change.
- **Phase 2:** request/response content capture (the hard change above) — its own review.
- **Full parity (later):** histograms/rankings, team/customer attribution (needs a team model),
  redaction + RBAC, multi-tab detail, sessions, object-storage offload.
