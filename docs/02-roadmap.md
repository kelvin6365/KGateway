# 02 — Roadmap

Phased delivery. Every milestone ships with **unit tests + a code review** before the next begins (see [07-quality.md](./07-quality.md)). Milestones are vertical where possible (something works end-to-end at each step).

Legend: 🟢 this session · 🟡 near-term · ⚪ later

---

## M0 — Planning & scaffold ✅ DONE
**Goal:** repo skeleton compiles; docs complete.
- [x] Planning docs (`docs/*`)
- [x] Cargo workspace + 5 crates, each `cargo check`-green with stubs
- [x] Next.js app scaffolded (`ui/`), builds — `pnpm build` + `pnpm lint` green, dev server boots
- [x] `Dockerfile` (multi-stage, repo root) + `docker-compose.yml` (gateway + optional postgres)
- [x] CI skeleton (`.github/workflows/ci.yml`: fmt, clippy, test)
**Exit:** ✅ `cargo build --workspace` and `pnpm --dir ui build` both succeed.

## M1 — Core vertical slice: OpenAI chat proxy ✅ DONE
**Goal:** a real request flows end-to-end.
- [x] `schema.rs` — `ChatRequest`, `ChatResponse`, `StreamChunk`, `Message`, `Tool`, `Usage` (serde, OpenAI-compatible wire format)
- [x] `Provider` trait + `OpenAiProvider` (`chat` + `chat_stream` via `reqwest`)
- [x] `engine.chat()` — single provider, plugin pipeline wired (pre/post + short-circuit)
- [x] `kgateway-server`: `POST /v1/chat/completions` (JSON + SSE streaming), `GET /health`
- [x] Config load from `config.json` (env-var interpolation for keys)
- [x] Unit tests: schema round-trip, SSE stream parsing (incl. cross-chunk split), config interpolation, e2e mocked-upstream HTTP round-trip + error mapping
- [x] **Code review** of the slice
**Exit:** ✅ verified at runtime — `curl` non-streaming chat through the gateway to a mock upstream returns a valid response; streaming covered by unit tests.

## M2 — Provider abstraction + more connectors + failover ✅ DONE
**Goal:** the gateway core (approved first feature).
- [x] `AnthropicProvider` (native Messages API ↔ internal schema; system extraction, Anthropic SSE events)
- [x] `openai_compat` factory → Groq, Ollama, OpenRouter, Together, xAI, DeepSeek, Cerebras, Perplexity
- [x] Router: primary + `fallbacks[]`, retry on retryable `KgError` (non-retryable stops immediately)
- [x] Weighted-random **key selection** with model filtering (`keyselect.rs`)
- [x] Key-level retry within a provider (up to 3 distinct keys, without replacement)
- [x] Per-provider **isolation**: `Semaphore` concurrency cap (held across the call / stream lifetime)
- [x] Providers wired into the server (`app.rs`) from config
- [x] Tests: fallback triggers, non-retryable stops failover, key-level retry, weighted distribution · **Review** (connector agent + self-review; findings folded in)
**Exit:** ✅ unit tests prove a retryable primary failure transparently succeeds on a fallback (`fallback_on_retryable_error`, `ctx.attempt == 2`).

> Note: streaming currently does single-provider dispatch (no failover) + weighted key pick + isolation permit; stream-path `pre_llm`/failover is the tracked `TODO(M3)`.

## M3 — Plugin pipeline + Embeddings ✅ DONE
- [x] `LlmPlugin` pipeline wired into engine (pre/post ordering, LIFO, short-circuit) — done M1/M2
- [x] **Run `pre_llm` in `chat_stream` too** — closed the streaming governance-bypass gap; a
      `ShortCircuit` response is emitted as a single-chunk stream (`stream_short_circuit_yields_single_chunk`)
- [x] Built-in **logging plugin** (request audit → `LogStore`), wired into the server + verified capturing live requests
- [x] `GET /api/logs` control-plane endpoint (reads the captured logs)
- [x] `Embeddings` capability (slim-trait accessor `Provider::as_embeddings`) + `OpenAiProvider` impl (all OpenAI-compat providers inherit it) + `POST /v1/embeddings`
- [x] Tests: stream short-circuit, embeddings dispatch + `Unsupported`, logging capture, embeddings decode/order · **Review** (agent-built logging plugin; self-verified)
**Exit:** ✅ runtime-verified — chat + embeddings flow through the gateway and the request is captured in `/api/logs`.

> Deferred (tracked): `HttpPlugin` + tower stack (no concrete consumer until an HTTP-edge plugin is needed); `post_llm` for streams (needs stream accumulation); embeddings currently skip the chat `LlmPlugin` pipeline (different request type); logging `provider` field is empty when the response echoes a bare model — engine could stash the resolved provider in `Ctx` (M4).

## M4 — Persistence + governance ✅ DONE
- [x] `kgateway-store`: `SqliteLogStore` (sqlx, runtime queries, inline DDL migration) behind the `LogStore` trait; `MemoryLogStore` default
- [x] `PreOutcome::Reject(KgError)` added to core so `pre_llm` can short-circuit with an error status (not just a success response)
- [x] **Virtual keys** (`GovernancePlugin`): per-key model allow-lists, sliding-window rate limits, cumulative token budgets
- [x] **Rate limits** + **budgets** enforced in `pre_llm` (reject) and accounted in `post_llm` (in-memory; Redis for shared counters is M9)
- [x] Server wiring: `Authorization: Bearer <vk>` → `ctx.virtual_key`; governance enabled (strict) when `virtual_keys` configured; SQLite used when `database` set
- [x] `KgError::http_status()` — single kind→status mapping shared by the HTTP handler and audit logger (fixed rejections being logged as 500)
- [x] Tests: budget exhaustion, rate-limit trip, model-allow-list, strict/open mode, SQLite round-trip · **Review** (store + governance)
**Exit:** ✅ runtime-verified — no-key→401, valid→200, disallowed-model→400, rate-limited→429, all persisted to SQLite with correct audit statuses `[200,200,400,401,429]`.

> Deferred (tracked): Postgres `LogStore` impl (`// TODO(M4)` in store); a config *store* (only the log store is persisted so far — provider/vkey config is still file-driven); vkey team/customer hierarchy; `sqlite::memory:` + pool caveat (each pooled conn gets a separate in-memory DB — use a file URL, or set max_connections(1) / shared-cache).

## M5 — Semantic cache + observability ✅ DONE
- [x] Embedding-based cache: `SemanticCachePlugin` — similarity lookup + short-circuit in `pre_llm`, store in `post_llm` (pending-embedding parked by `request_id` since `post_llm` lacks the request)
- [x] `Embedder` abstraction (`ProviderEmbedder` wraps a provider's embeddings capability) — keeps the cache unit-testable
- [x] Vector store: `VectorStore` trait + `InMemoryVectorStore` (brute-force cosine similarity) + `cosine_similarity` (agent-built, 13 tests) — `pgvector`/`sqlite-vec` persistent impls deferred
- [x] **Prometheus metrics** at `/metrics` via an axum middleware (requests_total, by_status, latency summary)
- [x] Config: `semantic_cache { embedding_provider, embedding_model, threshold }`, wired in `build_state`
- [x] Tests: cache miss→hit short-circuit, dissimilar→miss, post_llm unchanged, cosine edge cases, metrics render · **Review** (vector agent + self-verified)
**Exit:** ✅ runtime-verified — same prompt twice returns identical cached content with the upstream called **once**; `/metrics` renders Prometheus text.

> Status: OTLP export **shipped in M15**; observability is now the full M10–M15 stack. The
> semantic cache was **upgraded (M17) to a hardened design**: **(1) persistent `PgVectorStore`** (Postgres + pgvector — survives
> restart, shared across replicas; auto-selected when `database` is Postgres, else in-memory;
> live-verified against a real pgvector container); **(2) two-tier lookup** — an O(1) exact-match
> tier (SHA-256 request hash) before the embedding+similarity tier, so identical repeats skip
> embedding entirely; **(3) params/model scoping** via a `scope_key` (hash of model + sampling
> params + tools) that filters the semantic tier — fixing a real correctness bug where the old
> model-prefix heuristic could serve a cached response across different `temperature`/params.

## M6 — MCP gateway 🟡 FIRST CUT DONE
- [x] `McpClient` trait (transport-agnostic) + `StaticMcpClient` (in-process tools) in `core::mcp`
- [x] **Tool discovery + injection**: `chat_agentic` merges MCP tools into the request
- [x] **Tool-call execution loop**: LLM → execute tool-calls via the owning client → feed results back → re-prompt, capped at `DEFAULT_MAX_TOOL_ROUNDS` (infinite-loop guard)
- [x] Server: chat handler uses `chat_agentic` when MCP is enabled; `GET /api/mcp/tools`; `mcp.builtin_tools` config registers a demo `echo` tool
- [x] Tests: tool injected + executed + result fed back (2 LLM turns), immediate return without tool calls · **Review**
**Exit:** ✅ runtime-verified — model requests `echo` → gateway executes it → final answer uses the result (`tool said: hello tools`); `/api/mcp/tools` lists it.

> Follow-on: **stdio transport ✅ done** (Phase 11) — `StdioMcpClient` connects external MCP servers from config (`mcp.servers[]`), verified end-to-end. Remaining: streamable-HTTP transport, MCP auth types (headers/OAuth2/per-user), per-vkey tool allow-lists, MCP-as-server.

## M7 — Multimodal + remaining connectors 🟡 FIRST CUT DONE
- [x] Capability traits + accessors: `Images` (generate), `Audio` (speech + transcribe), `Rerank` — each opt-in, checked before dispatch (clean `Unsupported`)
- [x] Engine dispatch methods (`image_generate`/`speech`/`transcribe`/`rerank`) via a shared `resolve` (routing + weighted key + isolation permit)
- [x] **OpenAI** multimodal impls: DALL·E images, TTS speech (raw bytes), Whisper transcribe (multipart) — agent-built
- [x] **Cohere** native connector: `v2/embed` + `v2/rerank` (chat deferred) — agent-built
- [x] More OpenAI-compatible vendors: Mistral, Nebius, HuggingFace, vLLM, SGLang (added to `openai_compat`) → **13 connectors total**
- [x] Endpoints: `/v1/images/generations`, `/v1/audio/speech`, `/v1/audio/transcriptions` (multipart), `/v1/rerank`
- [x] Per-capability tests (image/speech/transcribe/rerank decode, error mapping) · **Review** (2 agents + self-verified)
**Exit:** ✅ runtime-verified — images/speech/rerank all flow through; rerank on a provider lacking the capability returns **501** (capability system works).

> Follow-on connectors — **done:** **Bedrock** (Converse + hand-rolled SigV4, validated vs AWS's `get-vanilla` vector); **Google Gemini** (native `generateContent`, `x-goog-api-key`); **Azure OpenAI** (deployment routing + `api-key` header + `api-version`). Config via `kind` (`bedrock`/`gemini`/`azure`). **Remaining:** full Vertex OAuth (service-account JWT), Replicate, ElevenLabs; Bedrock/Gemini streaming; `Files`/`Batch`; image edit/variation; streaming audio.

## M8 — Frontend (Next.js dashboard) ✅ DONE (read + write plane)
See [05-frontend.md](./05-frontend.md).
- [x] Scaffold: Next.js App Router + Tailwind v4 + TanStack Query + lucide, sidebar nav shell, dark-mode
- [x] Typed API client (`ui/lib/api.ts`) with SSE-streaming chat reader + Prometheus text parser
- [x] **Dashboard** — live health badge + real metrics from `/metrics` (total requests, avg latency, status breakdown), 5s refresh
- [x] **Playground** — working chat completion (streaming + non-streaming)
- [x] **Logs** — live request table from `/api/logs` (color-coded status, tokens, latency, filter), 4s refresh
- [x] **MCP** — tool cards from `/api/mcp/tools` (name, description, JSON param schema)
- [x] **Providers** — capability view + **live add/edit/remove** (form → `PUT/DELETE /api/config/providers` → persist to config.json → hot-reload). Presets for openai/anthropic/z.ai/groq. Admin-token input (localStorage).
- [x] **Virtual Keys** — **live create/edit/remove** (`PUT/DELETE /api/config/virtual-keys`) with model allow-lists, rate limits, token budgets. Warns that the first key enables strict mode.
- [x] Backend: permissive **CORS** so the browser dashboard can call the gateway
**Exit:** ✅ full-stack verified — Providers + Virtual Keys pages add/remove config live (no restart); creating a vkey flips governance to strict mode; deleting returns to open.

> Status: the **Cache / Plugins / Settings** pages are now built (status + stats driven; a `GET /api/status` summary feeds them). **Usage/cost charts** shipped in M12 (Analytics view). **Logs** gained SSE live tail + redaction reveal (M10–M11) — no longer a 4s poll. Remaining follow-ons: swap hand-rolled primitives for full shadcn/ui; auto-generate TS types from the backend.

## M9 — Hardening & deployment ✅ DONE
- [x] **Benchmarks** (`criterion`, `crates/kgateway-core/benches/hotpath.rs`): gateway per-request overhead **~2.8 µs**, key selection ~97 ns, serde ~300–400 ns
- [x] **Helm chart** (`charts/kgateway/`) — `helm lint` + `helm template` clean; SQLite (PVC) or Postgres mode, HPA, Ingress, ConfigMap-rendered config, secrets, health probes, Prometheus annotations
- [x] **Postgres `LogStore`** — completes "SQLite default + Postgres option behind the same trait"; server picks the store by URL scheme
- [x] **Graceful shutdown** — SIGINT/SIGTERM drain in-flight requests (`with_graceful_shutdown`), verified
- [x] Cluster/scale notes (docs/06-deployment.md)
- [ ] Follow-on: config hot-reload, WASM plugin host (`wasmtime`), Redis shared counters, security review

**Exit:** ✅ benches run, Helm renders in both DB modes, graceful shutdown verified, Postgres wired.

## M10 — Observability / Logs platform ✅ MVP + Phase 2 DONE

A full observability suite (see `docs/11-logs-plan.md` for the full gap analysis). "Logs" here means more than an audit trail — rich per-request records (full request/response content, cost, governance attribution), a filtered/paginated list + detail + stats + histogram/ranking API, live updates, and a filter-sidebar + multi-tab detail UI. KGateway today has a 7-field audit row, `GET /api/logs` (recent 100), and a table + client-side filter.

**MVP (no core-engine change needed — uses data already at hand) ✅ DONE — runtime-verified end-to-end:**
- [x] Expanded `RequestLog`: `created_at`, `virtual_key`, `stream`, `stop_reason`, `error_message`, `cost` (static per-model pricing table in `kgateway-plugins/src/pricing.rs`), `cache_hit`
- [x] Store query layer: `LogFilter`/`LogQuery`/`LogPage`/`LogStats`/`SortBy` + `query`/`get`/`stats` on the `LogStore` trait (default fetch-and-filter impls; SQLite/Postgres gain the columns + idempotent ALTER migrations for pre-M10 DBs). SQL push-down of filters/paging is a noted optimization, not blocking.
- [x] `GET /api/logs` — `limit`(clamp 200)/`offset`/`sort_by`/`order` + filters (provider/model/status/vkey/`since_ms`/`cache_hit`/search); `GET /api/logs/{id}` detail (404 shape); `GET /api/logs/stats` (aggregates)
- [x] UI: stats bar, filter sidebar (provider/model/vkey/status/cache/search + time-range quick-pick), server pagination, sortable columns, detail drawer (`ui/app/logs/page.tsx`)
- [x] **Live tail via SSE** (`GET /api/logs/stream`, `tokio::sync::broadcast` fed by `LoggingPlugin`) — self-auths via `?token=` since `EventSource` can't set headers; replaces the 4s poll
- [x] Retention cleanup job — `LogStore::purge_older_than(cutoff_ms)` (real `DELETE` for SQLite/Postgres, `retain` for Memory) driven by a background `tokio` sweep task (hourly, first sweep at startup) gated on `log_retention_days` config; re-reads the window from live config so hot-reload needs no restart. Runtime-verified end-to-end (seeded old row purged at boot).

**Phase 2 (the one hard architectural change — its own review) ✅ DONE — runtime-verified, adversarially reviewed (5 findings fixed):** design in `docs/12-content-capture-plan.md`.
- [x] Full **request/response content** capture threaded from the engine call sites as pre-serialized, truncated JSON on a widened `CallRecord` (`ContentCapture` policy, off by default, zero-cost when disabled). Per-capability matrix (chat full; embeddings/images/speech request-only; transcribe response-only; rerank both — never vectors/binary).
- [x] **Streaming** capture via a `Drop`-guard that finalizes the record at end-of-stream AND on early client disconnect (partial content logged; single record, no double-count).
- [x] **Async batch writer**: bounded `tokio::mpsc` + background task with per-batch transactions (SQLite/Postgres), `dropped` counter on backpressure (`GET /api/logs/dropped`), graceful shutdown-flush.
- [x] Bodies stored in `request_logs` but **excluded from list queries and the SSE tail** (`CAST(NULL AS TEXT)` + broadcast strip) — returned only by `GET /api/logs/{id}`. Off by default, opt-in, size-capped, admin-only.
- [x] UI: detail-drawer Request/Response sections (pretty/raw toggle, copy, truncation badge, calm "capture disabled" empty state) + dropped indicator.

**Full parity (later, gated on other features):** cost histograms + rankings, team/customer attribution (needs a team model), multi-tab detail (routing/plugins/raw), sessions (parent-request grouping), object-storage offload for large payloads.

---

## M11 — Redaction + RBAC ✅ DONE (two security reviews, 7 findings fixed)

The security layer M10 Phase 2 was deferred against. Design + decisions + review resolutions in `docs/13-redaction-rbac-plan.md`.
- [x] **Redaction** (`kgateway-plugins/src/redaction.rs`): built-in + custom regex patterns; secrets replaced with `⟦REDACTED:n⟧`; **reversible** AES-256-GCM-encrypted mapping (key = SHA-256 of passphrase, random nonce). No key ⇒ mask-only (reveal unavailable), never blocks boot.
- [x] Applied in the write path (`LoggingPlugin::apply_redaction`) before persist/broadcast; raw survives only inside the encrypted mapping. New store columns `redacted` + `redaction_mapping` (excluded from list + SSE; mapping never serialized to clients).
- [x] **RBAC**: `token → role → permission` model (`viewer`/`operator`/`admin`); `require_admin` → `require_view`/`require_write`/`require_reveal` route groups. Legacy `admin_token` = an admin token (backward-compatible). `GET /api/whoami` for role-aware UI.
- [x] **Reveal**: `GET /api/logs/{id}/reveal` gated by `logs:reveal`; decrypts the mapping, audit-logs the reveal. Runtime-verified: viewer 403, admin restores originals.
- [x] UI: redacted badge + role-gated Reveal button (whoami-driven) in the detail drawer.
- [x] Two adversarial security reviews → 7 findings fixed (placeholder-injection HIGH via unforgeable per-record marker; fail-closed auth; reveal audit "who"; `get_with_mapping` defense-in-depth; reveal flags; timing comment) + KDF hardening documented as accepted risk.

**Out of scope (later):** per-user accounts / SSO, per-team log scoping, redaction-key rotation, format-preserving redaction, ML/entropy detection, redacting live proxied traffic.

---

## M12 — Analytics (histograms + rankings + timeseries) ✅ DONE

Remaining M10 full-parity observability. Design in `docs/14-analytics-plan.md`. Aggregations computed in Rust over the filtered scan window (same pattern as `stats`/`query`; SQL push-down is a noted optimization).
- [x] Store: `histogram` (latency/cost/tokens), `timeseries` (count/errors over time), `rankings` (top-N model/provider/vkey by count/cost/tokens/errors), `filter_values` (distinct dropdown values) on the `LogStore` trait + compute-function unit tests (empty/single/spread, ordering, bucketing, distinctness).
- [x] Server API (all `logs:view`): `GET /api/logs/{histogram,timeseries,rankings,filterdata}` — same `LogFilter` params as `/api/logs`. Runtime-verified.
- [x] UI: Analytics view (Logs|Analytics toggle) — inline-SVG requests-over-time (success/error stacked), distribution histogram (latency/cost/tokens selector), top-model/provider ranking tables (count/cost/tokens/errors selector), sharing the logs filter state; provider/model filter datalists from `filterdata`. No chart library — self-contained bundle.

**Out of scope (later):** percentiles (p50/p95/p99), by-dimension cross-tabs, cost-recalc jobs, saved/scheduled reports, CSV export.

---

## M13 — Performance benchmark pass ✅ DONE

Validated the founding "Rust for performance" premise with measured numbers. Full methodology + results in `docs/15-performance.md`.
- [x] Criterion micro-benchmarks: hot-path primitives (keyselect ~99ns, serde ~330–395ns) + bare engine overhead **~2.9µs/request** (`kgateway-core/benches/hotpath.rs`).
- [x] **Pipeline overhead by layer** (`kgateway-plugins/benches/pipeline.rs`): bare 3.1µs → full observability (logging+governance) **3.5µs** → +content-capture 4.3µs → +redaction ~15µs (both bench bodies match).
- [x] **Redaction optimized** with a `RegexSet` prefilter: secret-free bodies (the common case) now cost **~0.3µs** instead of a full per-pattern scan. Data-driven: an alternation-regex variant was measured (6.4µs vs 4.2µs on matching bodies) and rejected; shipped `RegexSet`-prefilter + fast-failing per-pattern scans. Decomposed cost (no-match 0.3µs / mask 4.2µs / +crypto 6.3µs) documented in `docs/15-performance.md`.
- [x] HTTP end-to-end load test (`ab`, release, threaded mock): **p50/p95 = 1ms**, ~3.8k req/s (mock-bound, not gateway-bound). Gateway single-core overhead ceiling ≈ 285k req/s.
- [x] Honest caveats documented (mock is the bottleneck; micro-bench is the trustworthy overhead figure).

**Result:** full-observability overhead ~3.5µs/request — comfortably inside a typical ~11–59µs range for gateways of this class.

---

## M14 — Solidify / release pass ✅ DONE

Consolidating the M10–M13 work into something shippable.
- [x] **`kgateway-server` is now a library** (`lib.rs`) + a thin binary, so integration tests drive the REAL router/state (the old e2e reconstructed a fake one).
- [x] **Real-router integration tests** (`tests/api_e2e.rs`): chat→capture+redaction, reveal RBAC (viewer 403 / admin restore), 401/403 rejection, analytics endpoints — all through `app::build_router`.
- [x] **Dockerfile** (multi-stage, non-root, slim runtime, no libpq/libsqlite) + `.dockerignore` + `docker-compose.yml` (SQLite default, opt-in Postgres). The "docker-first" promise now has real files.
- [x] Helm `values.yaml` updated for the new config (api_tokens/roles, content_logging, redaction, log_retention_days); `helm lint` clean. `docs/06-deployment.md` reconciled with the real files.
- [x] **Config-reference doc** ([`16-configuration.md`](./16-configuration.md)) covers every field, incl. the later governance additions (`denied_models`, `max_cost_per_period`/`max_cost_period_secs`, shared cross-replica counters) and `content_logging.capture_streaming`. Getting-started ([`08`](./08-getting-started.md)) refreshed; `CHANGELOG.md` kept current per milestone.
- [x] **Docker image build verified** — multi-stage build succeeds → **98.5 MB** slim non-root image; runtime smoke test boots the binary and `GET /health` returns `{"status":"ok"}`.

---

## M15 — OTLP / OpenTelemetry export ✅ DONE

Export traces + metrics over OTLP so KGateway plugs into Grafana/Tempo, Jaeger, Datadog. Design in `docs/17-otlp-plan.md`.
- [x] OTel SDK (0.27) with **OTLP-over-HTTP** transport (reqwest client; no gRPC/tonic). `otlp` config block (endpoint/service_name/traces/metrics), off by default.
- [x] `OtelObserver` (`RequestObserver`) in the server crate — a span per request (start/end from `ctx.started_at`, attributes provider/model/status/tokens/stream/cache_hit/vkey/error) + metrics (request counter, latency histogram, token counter). Providers held in `AppState`, flushed on graceful shutdown (multi-thread-safe; the current-thread deadlock is a test-only artifact, handled).
- [x] **Live-verified**: a mock OTLP/HTTP collector received `POST /v1/traces` + `POST /v1/metrics` after chats + graceful shutdown. Unit tests cover disabled/enabled init paths.
- [x] **W3C `traceparent` context propagation**: inbound trace context is parsed and the gateway span nests under the caller's distributed trace (parsed in handlers → `Ctx.ext` → `build_with_context`). Live-verified: the exported span's protobuf carries the propagated trace-id. Parser unit-tested (valid + malformed/zero/version cases).

**Out of scope (later):** per-step span events, gRPC transport, OTLP logs signal.

---

## M16 — CI pipeline ✅ DONE

GitHub Actions (`.github/workflows/ci.yml`) enforcing on every push/PR what was previously checked by hand.
- [x] **Rust** job: `cargo fmt --all --check` + `clippy --all-targets -D warnings` + `test --workspace` (with `Swatinem/rust-cache`). Reformatted the whole workspace so the fmt gate is green.
- [x] **MSRV guard** job (rust 1.88): builds the workspace on the declared MSRV — catches the exact dependency-MSRV drift that broke the Docker build this session.
- [x] **UI** job: pnpm `install --frozen-lockfile` + `lint` + `build` (verified locally to pass).
- [x] **Docker** job: buildx image build (no push) with GHA layer cache.
- [x] Reconciled a duplicate Dockerfile/compose (removed the stale `docker/` scaffold; standardized on the validated repo-root `Dockerfile` + `docker-compose.yml`); fixed stale `docker/` path references in README/getting-started/roadmap.

---

## M17 — Semantic cache upgrade ✅ DONE

Hardened the cache design (see the M5 note above).
- [x] Persistent `PgVectorStore` (Postgres + pgvector, `<=>` cosine distance) — cache survives restarts and is shared across replicas; auto-selected when `database` is Postgres.
- [x] Two-tier lookup: O(1) exact-match by request hash *before* embedding, then scoped semantic search.
- [x] Params/model scoping so a cached response is never served across different params/tools.

---

## M18 — Feature-parity audit + batch ✅ DONE

Systematic gap analysis against a mature reference gateway → ranked gap list in [`docs/18-parity-audit.md`](./18-parity-audit.md); high-value / low-risk findings closed this pass.
- [x] **(A) Full request-param fidelity** — `ChatRequest` models ~18 previously-dropped OpenAI params + an `extra` flatten passthrough for arbitrary extra params; cache scopes on the whole serialized request minus messages.
- [x] **(C) Exponential backoff + jitter** between key-retry attempts (200 ms→3 s cap, ±20%).
- [x] **(G) Key rotation on auth failure** — per-key 401/402/403 rotates to the next eligible key (dead-key vs used-key split) instead of failing the provider.
- [x] **(E) Cost budgets** — virtual-key `max_cost_per_period` USD budget (tumbling window, priced from the static table).
- [x] **(H) Present-but-unknown virtual key → 401** even in open mode (only a truly-absent key is anonymous).
- [x] **(I) Model deny-lists** — virtual-key `denied_models` wins over the allow-list.
- [x] **(O) More OpenAI-compatible providers** — Fireworks, Parasail.

**Follow-on backlog (tracked in the audit doc):** ~~streaming resilience~~ (done in M19), shared cross-key governance, error-taxonomy fidelity, stream tool-calls, token-throughput limits, Responses/Batch/Files APIs, Vertex.

---

## M19 — Streaming resilience ✅ DONE

Brought streamed chat completions up to the same reliability bar as the non-streaming path (audit findings **B/M/D**).
- [x] **(B) Stream-open failover + first-chunk peek** — `chat_stream` now opens through the same provider-failover + key-rotation logic as `chat` (`open_stream_with_failover` / `open_stream_one`). The first chunk is peeked before any bytes reach the client, so an error at stream-open *or* on the first chunk rotates keys / fails over to the next provider transparently; once the first chunk is delivered, the provider is committed.
- [x] **(M) Idle timeout + permit release** — an inter-chunk idle timeout (incl. time-to-first-chunk, `STREAM_IDLE_TIMEOUT` = 60s) aborts a hung upstream, surfaces a terminal error chunk, and releases the per-provider concurrency permit so a stalled stream can't pin capacity.
- [x] **(D) Stream usage capture** — token usage from the final stream chunk is recorded on stream end and counts against governance budgets. Unified the capture and non-capture paths onto a single deferred audit guard that emits exactly once (on completion or early disconnect).
- [x] Tests: open-error failover, first-chunk-error failover, key rotation at open, non-retryable open error (no failover), usage recorded on completion, idle-timeout abort (deterministic via `tokio` paused time). `cargo test/clippy/fmt` green.

**Still deferred for streams:** `post_llm` (needs full stream accumulation) and streamed tool-call delta assembly.

---

## M20 — Shared governance counters ✅ DONE

Made per-virtual-key limits correct under horizontal scaling (audit finding **F**).
- [x] **`GovernanceStore` trait** in `kgateway-store` — abstracts the three counters (fixed-window requests, cumulative tokens, per-period cost). `InMemoryGovernanceStore` (default, single-node) + `SqlGovernanceStore` (Postgres, atomic `INSERT … ON CONFLICT DO UPDATE … RETURNING`).
- [x] **`GovernancePlugin` reworked** — configs are static; mutable counters go through the store (async). New `with_store` constructor; `new` keeps the in-process default.
- [x] **Server wiring** — `build_governance_store` selects the shared Postgres store when `database` is Postgres (reusing that connection), else in-process. Connect/migrate failure logs a warning and falls back rather than failing startup; a counter-store error at request time **fails open**.
- [x] **Windows are tumbling** — a single cheap atomic op, trading the in-process sliding window's precision for cross-replica correctness (documented).
- [x] Tests: in-memory window/token/cost accounting, a two-replica shared-budget test (one store, two plugin instances → single combined budget), and a live-Postgres integration test gated on `KGATEWAY_TEST_PG`. `cargo test/clippy/fmt` green.

---

## M21 — Streamed tool calls ✅ DONE

Function-/tool-calling now works over SSE (audit finding **L**).
- [x] **`Delta` models `tool_calls`** — new `ToolCallDelta` / `FunctionCallDelta` types; streamed fragments survive the parse→reserialize round-trip instead of being dropped. Fixes all OpenAI-compatible providers at once (shared SSE parser).
- [x] **`ToolCallAccumulator`** — reassembles fragments (first-seen id/name/type, `arguments` concatenated, keyed by `index`, parallel tool calls supported) into complete `ToolCall`s.
- [x] **Capture wiring** — the streaming content-capture guard reassembles tool calls so a tool-call response (no text deltas) is recorded in the audit log as the assembled call.
- [x] Tests: wire round-trip, single + parallel accumulation, plain-content no-op, OpenAI SSE forwarding, and end-to-end capture of an assembled streamed tool call. `cargo test/clippy/fmt` green.

**Deferred:** native tool-call streaming for the Gemini/Bedrock/Cohere adapters (their non-OpenAI SSE event shapes need per-adapter mapping); `post_llm` over accumulated streams. *(The **Anthropic** adapter shipped its mapping — `content_block_start` → id/name, `input_json_delta` → argument fragments — verified in M24.)*

---

## M22 — Aggregated model listing + z.ai GLM + coding-CLI verification ✅ DONE

- [x] **`GET /v1/models`** — OpenAI-compatible aggregated model list. Fans out concurrently to
  every configured provider's official list-models API (OpenAI-compat `GET {base}/models`,
  Anthropic `GET {base}/v1/models`) and returns the union with routable `provider/model` ids.
  Best-effort: erroring / unlistable (bedrock, azure, gemini, cohere) / keyless providers are
  skipped; 10s per-fetch timeout. New `kgateway-providers::model_listing` module (fetchers +
  parsers), `model_list_target` wire/base inference mirroring `build_engine`.
- [x] **z.ai GLM in `openai_compat::KNOWN`** — `zai` (pay-as-you-go, `/api/paas/v4`) and
  `zai-coding` (GLM Coding Plan, `/api/coding/paas/v4`); the Coding Plan's Anthropic wire stays
  available via `kind: "anthropic"`. Explicit `kind` still beats name inference.
- [x] **Coding-CLI verification** — Claude Code, OMP CLI, and Pi CLI each ran end-to-end through
  the gateway against the live GLM Coding Plan (`zai/glm-5.2`, streaming + tools), requests
  confirmed in the audit log. Pi CLI setup documented in docs 08 §3d.
- [x] Tests: list-body parsers, wire-inference table, wiremock e2e (aggregation + failure
  skipping). `cargo test/clippy/fmt` green (190 tests).

---

## M23 — Provider expansion: Moonshot (Kimi) + MiniMax 🟡 PREPARED (pending real keys)

- [x] `moonshot` (`https://api.moonshot.ai/v1`) and `minimax` (`https://api.minimax.io/v1`)
  added to `openai_compat::KNOWN` + unit tests + `config.example.json` entries
  (`${MOONSHOT_API_KEY}` / `${MINIMAX_API_KEY}`).
- [x] Official docs cross-checked (base URLs, Bearer auth, model families: kimi-k3 /
  kimi-k2.x / moonshot-v1-\*; MiniMax-M2 → M3). Both vendors' Anthropic-compatible
  `…/anthropic` endpoints and `GET /models` list endpoints probe-confirmed live (401-gated).
- [x] Keyless gateway verification: providers register by bare name, requests reach the real
  upstreams, upstream auth errors come back scrubbed (`upstream provider error`, HTTP 401),
  `/v1/models` skips them gracefully.
- [x] **Verification-status table** added to [03-providers.md](./03-providers.md#verification-status)
  marking live-tested vs prepared vs unit-tested-only providers.
- [ ] **Pending:** live chat/stream/tool verification once real keys are available — then flip
  the table rows to ✅.

---

## M24 — Model-list caching, picker, vkey hardening ✅ DONE

- [x] **`/v1/models` TTL cache** (5 min) keyed by a **provider-set fingerprint** (names, kinds,
  base URLs, key *ids* — never key values), so a config change or SIGHUP reload invalidates it
  immediately instead of serving stale inventory for the rest of the window.
- [x] **`/v1/models` is vkey-gated in strict mode** — it exposes provider + model inventory, so
  it now requires a known virtual key whenever `virtual_keys` are configured (previously the
  only anonymous data-plane route).
- [x] **Playground model picker** fed by the aggregated listing (`getModels()` in `ui/lib/api.ts`),
  merged with configured providers + recent-traffic pairs. No admin token needed for the
  listing source, so the picker is populated even for non-admin dashboard users.
- [x] **Anthropic streamed tool-calls verified** — the adapter's `content_block_start` /
  `input_json_delta` → `ToolCallDelta` mapping (present since M21 but untested) now has a
  reassembly test, and was confirmed live end-to-end through the gateway against GLM-5.2.
- [x] Governance audit: strict mode confirmed live on **both** ingresses
  (`/v1/chat/completions` and `/v1/messages`) — anonymous and wrong-key both `401`, hot-reloaded
  via SIGHUP with no restart.

---

## M25 — Per-request call tracing ✅ DONE

**Goal:** answer "where did this request's time actually go?" — which one `latency_ms`
number cannot.

- [x] **`kgateway_core::trace`** — `Span { name, category, start_us, dur_us, depth, outcome,
  detail }`, eight `SpanCategory` bands, and a `SpanCollector` behind a mutex (most of the
  pipeline holds `&Ctx`, not `&mut Ctx`) inside an `Arc` (a streamed response outlives the
  borrow). Microsecond precision so a sub-millisecond cache hit doesn't render as `0`.
- [x] **Engine instrumentation** — observer checks, per-plugin `pre_llm`, every dispatch
  attempt (with its upstream status as an outcome chip), backoff sleeps, contended semaphore
  waits (floored at 200 µs so uncontended permits don't add noise rows), time-to-first-token,
  and stream-body transfer.
- [x] **Persistence** — nullable `spans` column on SQLite + Postgres with in-place
  migrations; detail-read-only, mirroring the captured-body contract (list, `query`, and the
  SSE live tail all strip it).
- [x] **API** — `GET /api/logs/{id}` returns spans as a real JSON array rather than
  JSON-inside-a-string; a corrupt trace is dropped rather than breaking the read.
- [x] **Dashboard** — `TraceWaterfall` in a full-screen request dialog whose open state lives
  in the URL (`/logs?request=<id>`), so a trace is linkable and the browser back button
  closes it. Three columns (stage | timeline | duration) over a shared ruler, built from the
  existing bar idiom and theme tokens — no new palette, no charting dependency.
- [x] Tests: failover records one span per attempt with its status; a cache short-circuit
  records a hit and **no** upstream attempt; a streamed request's trace survives into its
  deferred audit record; spans round-trip on detail but are stripped from lists; the API
  returns an array. `cargo test/clippy/fmt` + `pnpm lint/build` green (203 tests).

**Caught during the build:** the deferred stream-capture guard rebuilt a fresh `Ctx` to emit
its audit record after the borrow ended, so *every streamed request* logged an empty trace
while unary requests traced correctly. Sharing the collector by `Arc` fixed it; there is now
a regression test pinning it.

**Deferred:** per-chunk stream timing (would show mid-stream stalls, but multiplies row
size); emitting these as OTLP child spans so the waterfall and Jaeger/Grafana agree.

---

## M26 — Generated API reference ✅ DONE

**Goal:** one place that tells you every endpoint and how to call it — that can't go stale.

- [x] **`api_catalog`** — one entry per endpoint: method, path, auth tier, summary, parameters,
  runnable example, sample response. The single source of truth for every doc surface.
- [x] **Drift test** — parses `.route(...)` out of `app.rs` and asserts it matches the catalog
  both ways. An undocumented route, or a documented route that no longer exists, fails the
  gate. This is the whole reason the reference stays true; a hand-kept one is wrong in a month.
- [x] **Four rendered surfaces:** `/openapi.json` (OpenAPI 3.1), `/llms.txt` (index, following
  the convention agents are trained on), `/llms-full.txt` (inlined), `/docs/{slug}.md`
  (per-endpoint Markdown). Unauthenticated — no secrets, and discovery shouldn't need a token.
- [x] **Dashboard `/docs`** — renders the gateway's own spec: grouped by auth tier, cURL /
  Python / JavaScript tabs derived from the one stored example, copy-page-as-Markdown.
- [x] Tests: OpenAPI shape and coverage, two methods on one path, security only where required,
  llms.txt convention, **every index link resolves to a real endpoint**, Markdown structure,
  base-URL substitution, size ceiling on llms-full.txt. 225 tests green.

**Deferred:** the interactive "Try it" playground (needs a request builder, auth handling, and
CORS thought — the existing Playground page already sends real chats, so the gap is small).

---

## 🎉 M0–M9 complete

KGateway is a working, tested Rust + Next.js LLM gateway: **13 providers**, multimodal (chat/embeddings/images/audio/rerank), failover + load-balancing + per-provider isolation, a capability-segmented plugin pipeline, governance (virtual keys / budgets / rate limits), SQLite **and** Postgres persistence, semantic cache, Prometheus metrics, agentic MCP tool-calling, a live Next.js dashboard, Docker + Helm deployment, and ~2.8 µs per-request overhead — every milestone runtime-verified. Remaining items are explicit follow-ons (transport-heavy connectors: Bedrock/Vertex/Azure; real MCP transport via `rmcp`; OTLP export; live-config write APIs), and the architecture is ready for each.

---

## Dependency graph

```
M0 ─► M1 ─► M2 ─► M3 ─► M4 ─► M5 ─► M6 ─► M7
                    └────────► M8 (UI, parallelizable from M4)
                                          └► M9
```

## How agents are used per milestone

Each milestone is delegated as: **build agent → test agent (or same agent writes tests) → code-review agent (`code-reviewer`) → main session verifies `cargo test`/`clippy` green**. Independent modules within a milestone (e.g. separate providers in M2/M7) fan out to parallel agents in isolated worktrees to avoid file conflicts.
