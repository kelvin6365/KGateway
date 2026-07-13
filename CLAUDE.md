# CLAUDE.md

Guidance for AI coding agents (and humans) working in this repository.

## What this is

**KGateway** is a high-performance, OpenAI-compatible **AI/LLM gateway**: one API in front of 21
providers, with failover, load balancing, a two-tier semantic cache, governance (virtual keys /
budgets / rate limits), reversible PII redaction + RBAC, Prometheus + OTLP observability, agentic
MCP tool-calling, and a Next.js dashboard. Backend is a Rust Cargo workspace; the UI is Next.js +
Tailwind + shadcn/ui.

- **Status & history:** see [`docs/02-roadmap.md`](docs/02-roadmap.md) and
  [`CHANGELOG.md`](CHANGELOG.md). Every milestone ships with unit tests + the quality gate green.
- **Deep dives:** [`docs/`](docs/) — architecture (01), providers (03), plugins (04), security
  (09), performance (15), configuration (16). Start at [`docs/README.md`](docs/README.md).

> **Originality:** KGateway is a standalone, original project. Do **not** attribute it to, name, or
> reference any other gateway/framework it may resemble — in code, comments, docs, commit messages,
> or config. Describe every design on its own terms.

## Golden rules

- **Always leave the workspace green.** After any Rust change, run the full gate (below). Never
  commit or hand off with a failing `clippy -D warnings`, `fmt`, or test.
- **Ship tests with the code.** Every feature/bugfix gets unit tests in the same change. Match the
  existing table-driven, `#[tokio::test]` style already in each crate.
- **Additive & backwards-compatible schema.** `ChatRequest`/`ChatResponse` derive `Default`;
  construct literals with `..Default::default()` so new fields don't break call sites.
- **Never write real secrets to disk.** No API keys in configs, tests, fixtures, or committed
  files. Keys come from `${ENV}` interpolation only.
- **Read before editing.** Prefer the dedicated file tools; understand the trait/flow first.

## Quality gate (run before finishing)

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
# UI (only if ui/ changed):
pnpm --dir ui lint && pnpm --dir ui build
```

- **MSRV is 1.88**, edition 2021. Transitive deps have bitten the MSRV before — if you bump deps,
  re-check the MSRV build (CI has a dedicated 1.88 job) and the Docker base image.
- Run the gateway: `cargo run -p kgateway-server -- --config config.json`
- Docker: `docker compose up --build`

## Workspace layout

Dependency direction: `server → plugins/providers/store → core`. Nothing depends on `server`.

| Crate | Role |
|---|---|
| `kgateway-core` | The engine. Schemas, `Provider`/`LlmPlugin`/`RequestObserver` traits, routing + failover, streaming, plugin pipeline. **No HTTP dependency — embeddable.** |
| `kgateway-providers` | Connectors: OpenAI, Anthropic, Cohere, Bedrock, Gemini, Azure + the `openai_compat` factory (15 OpenAI-wire-compatible vendors). |
| `kgateway-plugins` | Built-in plugins/observers: `logging`, `governance`, `semantic_cache`, `redaction`, `pricing`. |
| `kgateway-store` | Persistence behind traits: `LogStore`, `VectorStore`, `GovernanceStore` — each with in-memory + SQLite/Postgres impls (sqlx). |
| `kgateway-server` | axum HTTP gateway + control plane (the binary). `app.rs` wires config → engine; `handlers.rs` is the HTTP surface. |
| `ui/` | Next.js 15 (App Router) + Tailwind + shadcn/ui dashboard. Talks to the control plane (`/api/*`). |

## Architecture & request flow

- **Engine** (`kgateway-core::engine::Kgateway`) orchestrates: `pre_request` → `pre_llm` (may
  short-circuit) → routing/dispatch → provider call → `post_llm` → observers record.
- **`Provider` trait** + opt-in capability traits (`Embeddings` / `Images` / `Audio` / `Rerank`).
  A provider only implements what it supports; the engine checks capability before dispatch.
- **`LlmPlugin`** (`pre_request` / `pre_llm` / `post_llm`) — **chat-only**, ordered, `post_llm`
  runs LIFO. Plugin `Err` is non-blocking (logged); to short-circuit, return a typed
  `PreOutcome::{ShortCircuit, Reject}`.
- **`RequestObserver`** (`on_request` / `on_response`) — runs on **every** capability (governance,
  logging, OTLP). `on_request` can reject; `on_response` records usage.
- **Routing convention:** `"provider/model"` (e.g. `openai/gpt-4o`). `req.fallbacks[]` is a
  provider failover chain (capped at `MAX_FALLBACKS`); consumed by the router, never sent upstream.
- **Dispatch:** `dispatch_with_fallback` (provider failover on retryable errors) → `dispatch_one`
  (weighted key selection, per-key retry with exponential backoff + jitter, per-provider
  `Semaphore` isolation). `KgError::is_retryable()` gates provider failover;
  `is_key_rotatable()` (adds 401/402/403) gates key rotation within a provider.
- **Streaming** has parity with unary: `chat_stream` opens through the same failover + key rotation
  with a **first-chunk peek** (fail over before the client sees bytes), an idle-timeout guard, and
  a deferred capture guard that records tokens + `stop_reason` on stream end. Streamed tool-call
  fragments are modeled on `Delta` and reassembled by `ToolCallAccumulator`.

## Conventions & gotchas

- **Errors:** `KgError { kind, message, status, provider, retryable }`. `http_status()` is the
  single source of truth for response + audit status. **Never leak upstream error bodies** to
  clients — they get a scrubbed message; the raw detail stays server-side (`error_message`).
- **Governance fails OPEN** on a counter-store error (a DB blip must not block traffic). Counters
  live behind `GovernanceStore`: in-process by default, shared **Postgres** when `database` is
  Postgres. Windows are tumbling (cheap atomic upsert).
- **sqlx** uses the **runtime query API** (`sqlx::query`, not the compile-time macros) so the crate
  builds offline. Postgres integration tests are gated on env vars (`KGATEWAY_TEST_PG`,
  `KGATEWAY_TEST_PGVECTOR`) and skip when unset — never require a live DB to `cargo test`.
- **Deterministic time in tests:** use `#[tokio::test(start_paused = true)]` (tokio `test-util`)
  for timeout/backoff tests. Do not rely on wall-clock sleeps.
- **Content capture is opt-in twice:** `content_logging.enabled` covers request + non-streaming
  responses; **streamed** responses also need `content_logging.capture_streaming: true`. Captured
  bodies are admin-only (returned solely by `GET /api/logs/{id}`), never in list/SSE responses.
- **Streaming usage:** the gateway auto-injects `stream_options.include_usage: true` so
  OpenAI-compatible providers emit token usage. Anthropic-protocol adapters map usage from
  `message_start` / `message_delta` themselves.
- **Pricing** (`plugins::pricing`) is a **static best-effort table** matched by `contains` — add
  new models there; treat costs as estimates, not billing truth.
- **Config → engine** is assembled in `app.rs::build_configured_engine`; the `/api/status` plugin
  list in `handlers.rs` mirrors it — keep them in sync when adding a stage.

## Security constraints (non-negotiable)

- No real API keys in any committed file; `${ENV}` only.
- Redaction: the reverse mapping is **AES-256-GCM-encrypted**, excluded from list queries + SSE,
  and **never serialized to clients**. Reveal (`logs:reveal`) is admin-only and audited.
- **RBAC fails closed:** if `api_tokens` are declared but resolve empty, the control plane locks
  (every request 401) rather than silently opening.
- Preserve the error-body scrub and the `is_key_rotatable` classification (they are leak defenses).

## Adding things

- **New OpenAI-compatible provider:** add a `(name, base_url)` entry to `openai_compat::KNOWN`.
- **New native provider:** implement `Provider` (+ capability traits) in `kgateway-providers`,
  register in `app.rs`, add SSE-parse + error-mapping tests.
- **New plugin/observer:** implement `LlmPlugin` or `RequestObserver` in `kgateway-plugins`, wire in
  `app.rs`, add it to the `/api/status` plugin list.
- **New store backend:** implement the relevant `*Store` trait in `kgateway-store` with an
  in-memory default + a gated Postgres integration test.
- Update [`CHANGELOG.md`](CHANGELOG.md) and, for milestone-level work,
  [`docs/02-roadmap.md`](docs/02-roadmap.md).
