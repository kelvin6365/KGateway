# Changelog

All notable changes to KGateway are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). The project is pre-1.0, so changes are
collected under a single `Unreleased` section until the first tagged release.

## [Unreleased]

### Added

- **Session journeys.** Requests can now be grouped into a **session** — the full arc of one
  agent's AI usage (e.g. a Claude Code CLI run), instead of scattered log rows. The gateway
  resolves a session id at ingress: the `x-session-id` header wins, else it derives one from the
  OpenAI `user` field or Claude Code's Anthropic `metadata.user_id` (keying on the
  `session_<uuid>` segment) — so Claude Code groups automatically with no client change. The id
  is threaded through the request context (including the streaming deferred-capture path) into a
  new `session_id` log column (SQLite + Postgres, with idempotent `ALTER TABLE` migrations); it's
  an opaque grouping label, never request content, so it's stored unconditionally.

  Two new endpoints back the view: **`GET /api/sessions`** (grouped summaries — call count,
  tokens, cost, errors, cache hits, models/providers touched, and the session's time span; sorted
  by recency/cost/tokens/calls) and **`GET /api/sessions/{id}`** (a session's summary plus every
  call, oldest first). The dashboard gains a **Sessions** page and a per-session **journey** view:
  a Provider→Model→Outcome **Sankey** (weightable by calls / tokens / cost), a model→model
  transition Sankey, a chronological call timeline, and an expandable call list that reuses the
  trace waterfall. Grouping is computed over the recent log window, like the other analytics.

- **Startup banner.** On boot the server prints an ASCII wordmark plus a one-glance summary of
  the running gateway — version/MSRV/os-arch, the bound listen + dashboard URLs, and the salient
  config (provider count, storage backend, admin-API auth state, virtual-key count, semantic
  cache, MCP gateway). It writes to stdout (so it stays legible with `RUST_LOG` turned down) once
  the listener binds, so it only appears on a successful start. Colour auto-disables when stdout
  is not a TTY or `NO_COLOR` is set. It reports presence only — never a token, DB URL, or other
  secret. New `kgateway_server::banner`.

- **Generated API reference + agent-readable docs.** A new `api_catalog` module lists every
  endpoint the gateway serves — method, path, auth tier, parameters, and a runnable example —
  and four surfaces render from it: **`GET /openapi.json`** (OpenAPI 3.1, so the API also
  imports into Postman, Insomnia, or a client generator), **`GET /llms.txt`** (an index in the
  llms.txt convention, linking to per-endpoint Markdown), **`GET /llms-full.txt`** (everything
  inlined for a single fetch into a model's context), and **`GET /docs/{slug}.md`** (the
  Markdown twin the index links to). All four are unauthenticated: they describe the admin
  surface but contain no secrets, and an agent pointed at a gateway should be able to discover
  its API. The dashboard's new **API Docs** page renders the same spec, with cURL / Python /
  JavaScript samples, per-endpoint auth chips, and a copy-the-whole-page button.

  **The reference cannot drift.** A test parses the `.route(...)` table out of `app.rs` and
  asserts it matches the catalog exactly in both directions, so registering an endpoint
  without documenting it — or documenting one that no longer exists — fails `cargo test`.

- **`public_url` config.** Sets the origin the generated docs advertise. With it unset the
  base URL is derived from the request's `Host` (validated as a bare `host[:port]`), which is
  fine locally but attacker-influenceable — set it in production.

- **Per-request call tracing + dashboard waterfall.** Every request now records per-stage
  trace spans — each observer check, each plugin's `pre_llm` (so a cache hit reads as
  "served here, no upstream call"), **every dispatch attempt including the ones that failed
  before a successful retry**, backoff sleeps, contended concurrency permits,
  time-to-first-token, and stream-body transfer. A full-screen request dialog renders them as
  a waterfall, so a slow request shows *where* the time went instead of one opaque
  `latency_ms`. The dialog's open state lives in the URL (`/logs?request=<id>`), so a trace
  is linkable and the browser back button closes it; it replaces the old detail drawer. New `kgateway_core::trace` (`Span` / `SpanCategory` / `SpanCollector`) plus
  `Ctx::timed` / `Ctx::span_at` for instrumenting a stage.

  Spans carry **no request content and no upstream error text** — only stage names, timings,
  and gateway-authored outcomes — so unlike content capture they are recorded unconditionally
  with nothing to opt into. They are bounded (256 spans/request, 400-byte details), so a
  provider returning huge error pages across a retry chain cannot inflate an audit row. They are
  returned **only** by `GET /api/logs/{id}` (as a real JSON array, not a JSON-encoded
  string); list and SSE live-tail responses omit them, matching the captured-body contract.
  Storage cost is roughly 300–600 bytes per audit row, in a new nullable `spans` column
  (SQLite + Postgres migrations included, so existing databases upgrade in place).

- **`/v1/models` caching + strict-mode gating.** The aggregated listing is now cached for 5
  minutes behind a **provider-set fingerprint** (provider names, kinds, base URLs, key *ids* —
  never key values), so config edits / SIGHUP reloads invalidate it immediately rather than
  serving stale inventory. **Behavior change:** when `virtual_keys` are configured (strict
  mode), `GET /v1/models` now requires a known virtual key — it was previously the only
  anonymous data-plane route, and it exposes provider + model inventory.

- **Playground model picker from the live listing.** The dashboard Playground's model
  suggestions now merge the aggregated `/v1/models` inventory (new `getModels()` in
  `ui/lib/api.ts`) with configured providers and recent-traffic pairs. The listing needs no
  admin token, so the picker is useful for non-admin users too.

### Fixed

- **Startup banner dashboard URL.** The `Dashboard` URL now points at the separate Next.js
  dashboard on port 3000 (matching `docs/08-getting-started.md`), instead of reusing the
  backend listen port. `Listening` is unchanged.

- **Anthropic streamed tool-calls now covered by tests.** The connector's
  `content_block_start` → tool id/name and `input_json_delta` → argument-fragment mapping
  existed since M21 but was untested and documented as deferred. Added a reassembly test
  (fragmented JSON args + interleaved text block) and verified live end-to-end through the
  gateway against GLM-5.2; the roadmap's deferred note now correctly lists only
  Gemini/Bedrock/Cohere.

- **Moonshot (Kimi) + MiniMax as known OpenAI-compatible providers.** `moonshot`
  (`https://api.moonshot.ai/v1`; override base_url for the China platform) and `minimax`
  (`https://api.minimax.io/v1`) join `openai_compat::KNOWN` — keys-only config. Both vendors
  also expose Anthropic-compatible endpoints (`…/anthropic`) usable via `kind: "anthropic"`
  for Claude Code / OMP / Pi. **Prepared pending real keys**: official docs cross-checked and
  live keyless probes confirm both wires + list-models endpoints exist (401-gated); gateway
  routing, scrubbed error mapping, and `/v1/models` graceful skip verified. A new
  **[verification-status table](docs/03-providers.md#verification-status)** in the providers
  doc tracks which providers are fully live-tested vs prepared/unit-tested-only.

- **Aggregated model listing (`GET /v1/models`).** OpenAI-compatible endpoint that fans out to
  every configured provider's official list-models API concurrently (OpenAI-compat
  `GET {base}/models` with Bearer auth; Anthropic `GET {base}/v1/models` with `x-api-key`) and
  returns the union with `provider/model`-prefixed ids — directly routable back through the
  gateway. Best-effort: providers that error, have no listable endpoint (bedrock / azure /
  gemini / cohere), or whose `${ENV}` key is unset are skipped, each fetch bounded by a 10s
  timeout. Verified live against z.ai's Anthropic-compat and OpenAI-compat model-list APIs.

- **z.ai GLM as known OpenAI-compatible providers.** `zai` (pay-as-you-go,
  `https://api.z.ai/api/paas/v4`) and `zai-coding` (GLM Coding Plan,
  `https://api.z.ai/api/coding/paas/v4`) join `openai_compat::KNOWN` — configure with just keys,
  no `base_url` needed. The Coding Plan's Anthropic-compatible endpoint remains available via
  `kind: "anthropic"` (see docs 08 §3b). Explicit `kind` still wins over name inference, so
  existing `"zai": { "kind": "anthropic", ... }` configs are unaffected.

- **Pi CLI setup guide (docs 08 §3d).** Companion to the Claude Code (§3b) and OMP CLI (§3c)
  guides: register KGateway in `~/.pi/agent/models.json` with `api: "anthropic-messages"` and
  `provider/model`-prefixed ids. All three CLIs verified end-to-end against the GLM Coding Plan
  through the gateway.

- **Unbounded content capture (`max_body_bytes: 0`).** `content_logging.max_body_bytes` now
  treats `0` as "no cap": request/response bodies are captured **in full** (no `…[truncated]`
  marker), for both unary and streamed responses. The default stays 16 KiB. **Behavior change
  for pre-existing `max_body_bytes: 0` configs:** `0` previously truncated every captured body
  to an *empty string* (record the call, store no content) — it now stores the *complete*
  payload. If you relied on `0` to suppress bodies, set `enabled: false` instead.

- **Anthropic Messages ingress (`POST /v1/messages`).** KGateway now accepts inbound
  Anthropic-protocol requests, so Anthropic clients — **Claude Code**, the Anthropic SDKs — can
  route *through* the gateway to any provider, with governance / logging / failover / cache all
  applied. Full bidirectional translation including **streaming SSE and tool use**: Anthropic
  request → internal → provider, and internal response / stream / `tool_use` → Anthropic events.
  The outbound Anthropic connector gained matching tool support (`tools` / `tool_use` /
  `tool_result`, streamed `input_json_delta`, alternating-turn merge), so e.g. **Claude Code →
  KGateway → z.ai GLM** works end to end. Point Claude Code at it with
  `ANTHROPIC_BASE_URL=http://localhost:8080` and `ANTHROPIC_MODEL="zai/glm-4.6"`.
- **Streamed tool calls (M21).** The streaming `Delta` now models `tool_calls` fragments, so
  function-/tool-calling works over SSE — previously the internal schema had no field for them and
  every streamed tool-call fragment was silently dropped in the gateway's parse→reserialize
  round-trip (fixing all OpenAI-compatible providers at once, since they share the SSE parser). A
  new `ToolCallAccumulator` reassembles the fragments (id + name first, `arguments` concatenated
  across chunks, keyed by `index`, parallel tool calls supported) into complete `ToolCall`s. The
  streaming content-capture guard uses it so a tool-call response — which has no text deltas — is
  still recorded in the audit log as the assembled call.
- **Shared governance counters (M20).** Per-virtual-key rate limits and token/cost budgets now
  live behind a `GovernanceStore` abstraction instead of in-process state. With a Postgres
  `database` configured, counters are shared across replicas via atomic upserts, so a key capped
  at N/min stays capped at N/min no matter how many gateway instances run — closing a correctness
  gap under horizontal scaling (previously each replica enforced its own local limit). The default
  in-process store keeps single-node behavior; a counter-store outage fails **open** (a DB blip
  never blocks traffic). Windows are tumbling (a single cheap atomic op), trading the old
  in-process sliding window's precision for cross-replica correctness. Reuses the existing
  `database` connection — no new service.
- **Streaming resilience (M19).** Streamed chat completions now get the same reliability
  guarantees as non-streaming requests. The stream is opened with **provider failover +
  per-key rotation** and a **first-chunk peek**: an error surfaced at stream-open or on the
  first chunk rotates keys / fails over to the next provider *before any bytes reach the
  client*; once the first chunk is delivered the provider is committed. An **idle
  (inter-chunk) timeout** (incl. time-to-first-chunk) aborts a hung upstream and releases its
  concurrency permit so a stalled stream can't pin capacity. **Token usage** from the final
  stream chunk is now recorded on stream end (and counts against governance budgets), via a
  single deferred audit guard that emits exactly once on completion or early disconnect.
- **Parity/gap audit (M18).** A systematic internal audit surfaced a
  ranked gap list ([`docs/18-parity-audit.md`](docs/18-parity-audit.md)); the
  high-value / low-risk findings are now closed:
  - **Full request-param fidelity.** `ChatRequest` now models ~18 previously-dropped OpenAI params
    (`max_completion_tokens`, `frequency_penalty`, `presence_penalty`, `seed`, `n`, `stop`,
    `logit_bias`, `logprobs`/`top_logprobs`, `response_format`, `tool_choice`,
    `parallel_tool_calls`, `reasoning_effort`, `user`, `stream_options`) plus an `extra` flatten
    passthrough — so any client field survives to the provider instead
    of vanishing. The semantic cache now scopes on the whole serialized request (minus messages),
    so no new param can collide across cache entries.
  - **Key rotation on auth failure.** A per-key `401/402/403` now rotates to the next eligible key
    within the provider (dead-key vs used-key split), instead of failing the provider outright.
  - **Exponential backoff + jitter** between key-retry attempts (200 ms → 3 s cap, ±20% jitter).
  - **Governance: cost budgets + deny-lists.** Virtual keys gain a `max_cost_per_period` USD budget
    (tumbling window via `max_cost_period_secs`, priced from the static table) and a `denied_models`
    list that wins over the allow-list. A **presented-but-unrecognized** virtual key is now a `401`
    even in open mode (only a truly-absent key is anonymous).
  - **More OpenAI-compatible providers.** Added Fireworks and Parasail defaults.
- **Semantic cache upgrade (M17).** Adopted a proven, battle-tested design: a
  **persistent `PgVectorStore`** (Postgres + pgvector — the
  cache now survives restarts and is shared across replicas, auto-selected when `database` is
  Postgres); a **two-tier lookup** (O(1) exact-match by request hash *before* embedding, so
  identical repeats skip the embed + similarity scan); and **params/model scoping** so a cached
  response is never served across different `temperature`/params/tools.
- **Dashboard completeness (M8 close-out).** The **Cache**, **Plugins**, and **Settings** pages
  are built out (previously stubs), backed by a new read-only `GET /api/status` endpoint
  (config summary + feature flags + active pipeline). Cache shows hit-rate/config + recent
  cache-served requests; Plugins lists the active request pipeline; Settings shows the config
  summary + admin-token management.
- **OTLP / OpenTelemetry export (M15).** Opt-in `otlp` config block exports **traces** (one span
  per request, with provider/model/status/token/stream/cache-hit/vkey/error attributes) and
  **metrics** (request counter, latency histogram, token counter) over OTLP-over-HTTP to any
  collector (Grafana/Tempo, Jaeger, Datadog). Off by default; providers flushed on graceful
  shutdown. Inbound **W3C `traceparent`** context is propagated so gateway spans nest under
  the caller's distributed trace.
- **Observability / logs platform (M10).** Expanded request records (`created_at`, `virtual_key`,
  `stream`, `stop_reason`, `error_message`, `cost` from a static per-model pricing table,
  `cache_hit`). New filtered/paginated logs API — `GET /api/logs` with `limit`/`offset`/`sort_by`/
  `order` plus provider/model/status/vkey/time/cache/search filters, `GET /api/logs/{id}` detail,
  and `GET /api/logs/stats` aggregates. **SSE live tail** at `GET /api/logs/stream` replaces the
  poll. Configurable log retention via `log_retention_days` with an hourly background purge sweep.
  Dashboard **Logs** page gained a stats bar, filter sidebar, server pagination, sortable columns,
  and a detail drawer.
- **Request/response content capture (M10 Phase 2).** Opt-in `content_logging` captures
  request/response bodies (per-capability matrix; never vectors/binary), size-capped
  (`max_body_bytes`, default 16 KiB), with optional streamed-response capture. Bodies are captured
  off the hot path via an **async batch writer** (bounded channel + background task, per-batch
  transactions, backpressure `dropped` counter at `GET /api/logs/dropped`, shutdown flush).
  Streaming capture finalizes on end-of-stream and on early client disconnect. Bodies are
  admin-only — excluded from list queries and the SSE tail, returned solely by
  `GET /api/logs/{id}`. Detail drawer shows Request/Response sections with pretty/raw toggle and a
  truncation badge.
- **Analytics (M12).** New aggregation endpoints (all `logs:view`): `GET /api/logs/histogram`
  (latency/cost/tokens), `/timeseries` (count/errors over time), `/rankings` (top-N model/provider/
  vkey), and `/filterdata` (distinct dropdown values) — all taking the same filter params as
  `/api/logs`. Dashboard **Analytics** view: requests-over-time (success/error stacked),
  distribution histograms, and top model/provider ranking tables, rendered with self-contained
  inline SVG (no chart library).
- **RBAC control-plane tokens (M11).** New `api_tokens[]` config binding bearer tokens to
  `viewer` / `operator` / `admin` roles, alongside the legacy `admin_token`. `GET /api/whoami`
  returns the caller's role for a role-aware UI.
- **Redaction reveal (M11).** `GET /api/logs/{id}/reveal` returns un-redacted bodies for holders of
  the `logs:reveal` permission (admin), with a role-gated Reveal button in the UI.

### Changed

- **Control-plane auth is now role-based.** The single `require_admin` gate became per-permission
  route groups (`logs:view` / `config:write` / `logs:reveal`). The legacy `admin_token` maps to an
  `admin`-role token, so existing deployments keep working unchanged.
- **Performance benchmark pass (M13).** Full-observability overhead measured at **~3.5 µs/request**
  (bare engine ~2.9 µs; +content-capture ~4.3 µs), comfortably inside a typical
  ~11–59 µs range for comparable gateways. Redaction was optimized with a `RegexSet` prefilter so secret-free bodies (the
  common case) cost **~0.3 µs** instead of a full per-pattern scan. Methodology and numbers in
  [`docs/15-performance.md`](docs/15-performance.md).

### Fixed

- **Anthropic ingress: mid-conversation `system` turns.** A `"role": "system"` message inside
  `messages[]` (Claude Code emits one alongside the top-level `system` field) was demoted to a
  user turn, blurring instructions into the conversation. It now maps to a system turn.
- **Streamed requests now log token usage + stop reason.** Three gaps meant streamed responses
  recorded 0 tokens (and therefore 0 cost) and no finish reason:
  - The gateway now auto-injects `stream_options.include_usage: true` for streamed chat requests
    (respecting a client-supplied value), since OpenAI-compatible providers only emit a usage
    chunk when asked.
  - The Anthropic streaming adapter now emits the `message_delta` event's `output_tokens` +
    `stop_reason` (both were previously dropped) and captures `input_tokens` from **either**
    `message_start` or `message_delta` — some bridges (e.g. z.ai) report the prompt count only at
    the end. Fixes usage/stop-reason for all Anthropic-protocol providers (incl. z.ai-style
    `kind: "anthropic"`).
  - Added `glm-5` (and family) to the static pricing table so GLM-5.x models show an estimated
    cost instead of blank. Rates are best-effort estimates — adjust in `pricing.rs` for your plan.
  - The stream capture guard now records the `finish_reason` (`stop_reason`) seen on the stream.
- **Clearer log UI hint for streamed responses.** The "no content captured" empty state now tells
  you a streamed response needs `content_logging.capture_streaming: true` (not just `enabled`),
  instead of the generic message. `config.example.json` documents the `content_logging` block.

### Security

- **Reversible redaction of captured bodies (M11).** `redaction` config scans bodies with a
  built-in pattern set (emails, JWTs, API-key shapes, AWS keys, bearer tokens, card/phone/IP) plus
  custom regexes, replacing secrets with stable placeholders. The reverse mapping is stored
  **AES-256-GCM-encrypted** (key from `redaction.key`, random nonce); with no key configured,
  redaction still masks but the mapping is dropped and reveal is unavailable — it never blocks boot.
  Redaction runs in the write path before persist/broadcast, so raw secrets survive only inside the
  encrypted mapping (excluded from list queries and the SSE tail, never serialized to clients).
- **Reveal is audited.** Every `logs:reveal` call is audit-logged with the caller's token
  name/role (`revealed_by`).
- **Fail-closed RBAC.** If `api_tokens` are declared but all values resolve empty (e.g. broken
  `${ENV}` injection), the control plane locks (every request `401`) with a startup `error!` rather
  than silently opening. Only a config with no tokens at all runs open.
- Two adversarial security reviews on the redaction/RBAC work resolved 7 findings — including a
  HIGH placeholder-injection fix (unforgeable per-record markers) and defense-in-depth so ordinary
  detail reads never load the encrypted mapping.

[Unreleased]: https://github.com/kgateway/kgateway
</content>
