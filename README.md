# KGateway

A high-performance, open-source **AI/LLM gateway** built with Rust + Next.js.

One OpenAI-compatible API in front of every major LLM provider — with failover, load
balancing, semantic caching, governance, redaction, MCP tool-calling, and full observability.

> **Status:** production-capable and continuously verified. **25 providers**, multimodal
> (chat / embeddings / images / audio / rerank), resilient routing (failover + weighted key
> selection + per-provider isolation) on **both** unary and streaming paths, a plugin pipeline,
> governance with **shared cross-replica counters**, SQLite **and** Postgres persistence, a
> two-tier semantic cache, reversible PII redaction + RBAC, Prometheus `/metrics` + OTLP export,
> agentic MCP tool-calling, and a live Next.js dashboard — at **~3.5 µs** per-request overhead.
> ~164 tests, green under `clippy -D warnings` + `fmt`. See [`docs/02-roadmap.md`](docs/02-roadmap.md).

## What you can do

- **Talk to any provider through one API.** Point any OpenAI SDK at KGateway and route to 25
  providers by a `"provider/model"` string — no per-vendor client code.
- **Never drop a request.** Provider failover, weighted API-key selection, and key-level retry
  with exponential backoff + jitter — applied to **streaming** responses too (first-chunk peek
  fails over before the client sees a byte; idle-timeout aborts a hung upstream).
- **Cap spend and abuse.** Per-virtual-key model allow/deny-lists, request rate limits, token
  budgets, and USD **cost budgets** — enforced correctly **across replicas** via a shared
  Postgres counter store.
- **Cut cost and latency.** A two-tier semantic cache (O(1) exact-match before an embedding
  similarity search) serves repeat/near-repeat prompts without hitting the provider.
- **Stay compliant.** Reversible, AES-256-GCM-encrypted redaction of secrets/PII in captured
  bodies, RBAC-gated control plane, and audited reveal.
- **See everything.** Request audit log with filters/pagination/live SSE tail, analytics
  (histograms, time-series, top-N rankings), Prometheus metrics, and OTLP traces + metrics.
- **Run tools.** Agentic MCP tool-calling: discover → inject → execute → re-prompt.
- **Operate from a UI.** A Next.js dashboard for playground, logs, analytics, providers, cache,
  plugins, MCP, and settings.

## What's inside

| Area | Capabilities |
|---|---|
| **API** | OpenAI-compatible `/v1/chat/completions` (JSON + SSE), `/v1/embeddings`, `/v1/images/generations`, `/v1/audio/speech`, `/v1/audio/transcriptions`, `/v1/rerank`, and an aggregated `/v1/models` (fans out to every configured provider's official list-models API, returns routable `provider/model` ids). **Anthropic-compatible `/v1/messages`** ingress too (streaming + tool use) — point **Claude Code**, the **OMP CLI**, the **Pi CLI**, or the Anthropic SDKs at the gateway. Full request-param fidelity (`seed`, `response_format`, penalties, tool-choice, …) plus an `extra` passthrough so no client field is dropped. |
| **Providers (25)** | **Native:** OpenAI, Anthropic, Cohere, Amazon Bedrock, Google Gemini, Azure OpenAI. **OpenAI-compatible:** Groq, OpenRouter, xAI, DeepSeek, Cerebras, Perplexity, Together, Fireworks, Parasail, Mistral, Nebius, HuggingFace, z.ai GLM (`zai` pay-as-you-go + `zai-coding` Coding Plan), Moonshot (Kimi), MiniMax, Ollama, vLLM, SGLang. See the [verification-status table](docs/03-providers.md#verification-status) for which are live-tested vs prepared. |
| **Routing** | Primary + `fallbacks[]` provider failover, weighted key selection, per-key retry with backoff + jitter, per-provider `Semaphore` concurrency isolation, dead-key vs used-key rotation. Works on unary **and** streaming. |
| **Governance** | Virtual keys: model allow/deny-lists, request rate limits, token budgets, per-period USD cost budgets. Counters behind a `GovernanceStore` — in-process by default, **shared Postgres** for horizontal scaling. |
| **Caching** | Two-tier semantic cache (exact-hash tier + embedding similarity), params/model-scoped. In-memory or persistent **pgvector** (survives restart, shared across replicas). |
| **Security** | Reversible AES-256-GCM redaction of captured bodies, RBAC (viewer/operator/admin) with fail-closed tokens, audited reveal. |
| **Observability** | Request audit log (`/api/logs`, filters + pagination + SSE tail), analytics endpoints, opt-in request/response content capture (async batch writer), Prometheus `/metrics`, **OTLP** traces + metrics with W3C `traceparent` propagation. |
| **Plugins** | Capability-segmented pipeline (`pre_request` / `pre_llm` / `post_llm` + request observers) running on every capability. |
| **MCP** | Agentic tool-calling over in-process + stdio MCP servers. |
| **Persistence** | SQLite (default) and Postgres behind store traits; in-memory fallback. |
| **Deploy** | Docker (single container + SQLite) or Helm (SQLite/Postgres, HPA, Ingress). CI: fmt + clippy + tests + MSRV guard + UI build + Docker build. |

## Performance

Rust core, measured via `cargo bench` against an instant mock provider (KGateway's own
overhead, no network). Full detail in [`docs/15-performance.md`](docs/15-performance.md).

| Path | Overhead |
|---|---|
| Bare engine (`chat` pipeline) | **~2.9 µs** |
| **Full production observability** (logging + governance) | **~3.5 µs / request** |
| + request/response content capture | ~4.3 µs |
| Redaction — no secrets (`RegexSet` prefilter miss) | ~0.30 µs |
| Weighted key selection (8 keys) | ~99 ns |

The full observability path sits comfortably inside a typical **~11–59 µs** mean-overhead range
for comparable gateways.

## Quick start (dev)

```bash
# 1. build
cargo build --workspace

# 2. configure (copy the example and set your key)
cp config.example.json config.json
export OPENAI_API_KEY=sk-...

# 3. run
cargo run -p kgateway-server -- --config config.json

# 4. call it (OpenAI-compatible)
curl -X POST http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"openai/gpt-4o","messages":[{"role":"user","content":"hi"}]}'

# streaming (SSE):
curl -N -X POST http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"openai/gpt-4o","messages":[{"role":"user","content":"hi"}],"stream":true}'
```

Model routing convention: `"provider/model"` (e.g. `openai/gpt-4o`, `groq/llama-3.1-70b`).
Add a fallback chain with `"fallbacks":[{"provider":"anthropic","model":"claude-3-5-sonnet"}]`.

## Docker

```bash
docker compose up --build
```

## Workspace

| Crate | Role |
|---|---|
| `kgateway-core` | engine: schemas, `Provider`/`Plugin` traits, routing, streaming, pipeline (no HTTP dep — embeddable) |
| `kgateway-providers` | connectors: OpenAI, Anthropic, Cohere, Bedrock, Gemini, Azure + the OpenAI-compatible factory |
| `kgateway-plugins` | built-in plugins: logging, governance, semantic cache, redaction, pricing |
| `kgateway-store` | persistence behind store traits (SQLite / Postgres / in-memory: logs, vectors, governance counters) |
| `kgateway-server` | axum HTTP gateway + control plane (the binary) |
| `ui/` | Next.js + Tailwind + shadcn/ui dashboard |

## Configuration highlights

- `providers` — per-provider keys (with `${ENV}` interpolation), weights, model filters, base-URL overrides
- `virtual_keys` — `allowed_models` / `denied_models`, `max_requests_per_min`, `max_total_tokens`, `max_cost_per_period`
- `database` — SQLite or Postgres URL (Postgres unlocks persistent cache + shared governance counters)
- `semantic_cache`, `redaction`, `api_tokens` (RBAC), `otlp`, `mcp`, `content_logging`, `cors_allow_origins`

## Documentation

Full architecture, roadmap, and design rationale live in [`docs/`](docs/). Start with
[`docs/README.md`](docs/README.md).

## Development

```bash
cargo test --workspace          # ~164 unit + integration tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

## License

Apache-2.0.
