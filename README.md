<div align="center">

# KGateway

**One OpenAI-compatible API in front of 25 LLM providers.**

Failover · load balancing · semantic cache · budgets & rate limits · PII redaction · tracing · dashboard

Built in Rust. **~3.5 µs** of overhead per request.

[![CI](https://github.com/kelvin6365/KGateway/actions/workflows/ci.yml/badge.svg)](https://github.com/kelvin6365/KGateway/actions/workflows/ci.yml)
[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](./LICENSE)
[![MSRV: 1.88](https://img.shields.io/badge/MSRV-1.88-orange.svg)](#local-development)

[Quick start](#quick-start) · [Features](#features) · [Local development](#local-development) · [Docs](docs/README.md)

</div>

---

![KGateway dashboard](docs/images/dashboard.png)

<details>
<summary><b>The whole dashboard</b> — every page, captured from a live gateway carrying real traffic</summary>

<br>

Each shot below is a running instance, not a mockup: a coding agent working through this
repository via the gateway, plus a deliberately dead provider to prove failover.

---

**See what's happening**

### Per-request tracing — `/logs/{id}`

Every request records a stage-by-stage waterfall. Here a **streamed** call to a self-hosted node
fails in 32 ms, fails over to a second provider, and still delivers a first token 2.18 s in — the
client never sees the failure, only a normal `200`.

![Request trace waterfall](docs/images/request-trace.png)

### Logs — `/logs`

One row per call: provider, model, virtual key, status, latency, tokens in/out, cost. Filter on
any of them, search request ids and errors, or tail live traffic over SSE.

![Request logs](docs/images/logs.png)

### Analytics — `/logs` → Analytics

Latency, cost and token *distribution* — not just averages — plus rankings by count, cost, tokens
or errors.

![Analytics](docs/images/analytics.png)

### Sessions — `/sessions`

Calls carrying an `x-session-id` (or an OpenAI `user`) are grouped into runs, so a coding agent's
whole task shows up as one row with its own spend, tokens, and duration — live, while it runs.

![Sessions](docs/images/sessions.png)

### Session journey — `/sessions/{id}`

The full run: every call, where the spend went by model, the success/cache/error split, and the
provider → model → outcome flow.

![Session journey](docs/images/session-journey.png)

---

**Control what goes through**

### Providers — `/providers`

Connect a provider from the catalog — changes persist to `config.json` and hot-reload without a
restart.

![Providers](docs/images/providers.png)

### Virtual keys — `/virtual-keys`

Issue a key with a model allow/deny-list, a rate limit, a token budget and a USD cost ceiling.
Your upstream provider keys never leave the gateway.

![Virtual keys](docs/images/virtual-keys.png)

### Plugins — `/plugins`

The request pipeline and which stages are actually on. Redaction you *think* is enabled is worse
than none, so this page reports state rather than letting you toggle it.

![Plugins](docs/images/plugins.png)

### Semantic cache — `/cache`

Hit rate and stored entries for the two-tier cache. Shown here unconfigured — it needs an
`embedding_provider` in the config.

![Cache](docs/images/cache.png)

### MCP tools — `/mcp`

Tools discovered from MCP servers and injected into requests, with the gateway executing the
calls and feeding results back. Not enabled in this instance.

![MCP tools](docs/images/mcp.png)

---

**Use it**

### API reference — `/docs`

Every endpoint, generated from the server's own route table, with runnable cURL / Python /
JavaScript examples. A drift test fails the build if this page and the router ever disagree.

![API reference](docs/images/api-docs.png)

### Playground — `/playground`

Multi-turn, streaming, against any configured `provider/model`.

![Playground](docs/images/playground.png)

### Settings — `/settings`

Read-only view of the live configuration — version, storage, auth mode, timeouts, providers,
feature flags — plus what each credential type can see.

![Settings](docs/images/settings.png)

</details>

## Why KGateway?

- **One API, every provider.** Point any OpenAI SDK at KGateway and switch between 25 providers
  by changing a `"provider/model"` string — no per-vendor client code.
- **Requests don't drop.** Provider failover + weighted key rotation + retry with backoff, on
  unary **and** streaming (a first-chunk peek fails over before the client sees a byte).
- **Spend stays capped.** Virtual keys with model allow-lists, rate limits, token budgets, and
  USD cost budgets — enforced across replicas via a shared Postgres counter store.
- **Repeat prompts are free.** A two-tier semantic cache (exact hash, then embedding similarity)
  answers near-duplicate prompts without touching a provider.
- **You can see everything.** Per-request waterfall traces, filterable audit logs with live SSE
  tail, analytics, Prometheus `/metrics`, OTLP export — and a full Next.js dashboard.
- **Agent runs are one row, not fifty.** Calls are grouped into **sessions**, so a coding agent's
  whole task shows its own spend, tokens, and outcome while it's still running.
- **It's fast.** The full production pipeline (logging + governance) adds **~3.5 µs** per
  request ([benchmarks](docs/15-performance.md)).

## Quick start

### Option A — one command (generates a config from your env)

```bash
git clone https://github.com/kelvin6365/KGateway.git && cd KGateway
OPENAI_API_KEY=sk-... ./scripts/start.sh
```

Writes a `config.json` from whatever keys are in your environment, builds, asks whether to start
the dashboard alongside, and runs both (`KGATEWAY_START_UI=1|0` answers that prompt
non-interactively).

### Option B — from source (Rust 1.88+)

```bash
git clone https://github.com/kelvin6365/KGateway.git && cd KGateway

cat > config.json <<'JSON'
{
  "port": 8080,
  "database": "sqlite://./kgateway.db?mode=rwc",
  "providers": {
    "openai": { "keys": [{ "id": "default", "value": "${OPENAI_API_KEY}", "weight": 1 }] }
  }
}
JSON

export OPENAI_API_KEY=sk-...
cargo run -p kgateway-server -- --config config.json
# → kgateway listening on 0.0.0.0:8080
```

### Option C — Docker (nothing to install but Docker)

Same `config.json` as above, but point the database at the container's data volume
(`"database": "sqlite:///data/kgateway.db?mode=rwc"`), then:

```bash
OPENAI_API_KEY=sk-... docker compose up --build
```

> **On `config.example.json`:** it's a *reference* showing every field, not a starter config.
> It declares an `admin_token` and a `virtual_keys` entry — so copying it as-is turns on
> strict mode (every request needs `Authorization: Bearer vk_team_alpha`) and, if
> `KGATEWAY_ADMIN_TOKEN` is unset, locks the control plane fail-closed. Start from the minimal
> config above and add fields from the example as you need them.

### Send your first request

Everything is OpenAI-compatible. Models are addressed as `provider/model`:

```bash
curl http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"openai/gpt-4o","messages":[{"role":"user","content":"hi"}]}'
```

Or use any OpenAI SDK unchanged:

```python
from openai import OpenAI

client = OpenAI(base_url="http://localhost:8080/v1", api_key="unused")

resp = client.chat.completions.create(
    model="openai/gpt-4o",          # switch providers by changing the prefix:
    # model="anthropic/claude-3-5-sonnet",  "groq/llama-3.1-70b",  "ollama/llama3", ...
    messages=[{"role": "user", "content": "hi"}],
)
```

Streaming (`"stream": true`), embeddings, images, audio, and rerank work the same way. Add a
failover chain per request with `"fallbacks": [{"provider": "anthropic", "model": "claude-3-5-sonnet"}]`.

### Open the dashboard

```bash
cd ui
pnpm install
NEXT_PUBLIC_KGATEWAY_URL=http://localhost:8080 pnpm dev
# → http://localhost:3000
```

Playground, live logs, analytics, provider management, cache, MCP tools, and generated API docs.

### Use it with coding agents

KGateway also exposes an **Anthropic-compatible `/v1/messages`** ingress (streaming + tool use),
so Anthropic-protocol clients such as **Claude Code** route through the gateway to *any*
provider — with governance, logging, failover, and caching applied:

```bash
export ANTHROPIC_BASE_URL=http://localhost:8080   # base URL — Claude Code appends /v1/messages
export ANTHROPIC_AUTH_TOKEN=local                 # any token; a virtual key if governance is on
export ANTHROPIC_MODEL="zai/glm-4.6"              # provider/model picks the route
claude
```

Setup guides for Claude Code, the OMP CLI, and the Pi CLI — including the common traps — are in
[`docs/08-getting-started.md`](docs/08-getting-started.md).

## Features

| Area | Capabilities |
|---|---|
| **API** | OpenAI-compatible `/v1/chat/completions` (JSON + SSE), `/v1/embeddings`, `/v1/images/generations`, `/v1/audio/speech`, `/v1/audio/transcriptions`, `/v1/rerank`, aggregated `/v1/models`, plus **Anthropic-compatible `/v1/messages`** ingress. Full request-param fidelity (`seed`, `response_format`, penalties, tool-choice, …) and an `extra` passthrough so no client field is dropped. |
| **Providers (25)** | **Native (6):** OpenAI, Anthropic, Cohere, Amazon Bedrock, Google Gemini, Azure OpenAI. **OpenAI-compatible (19):** Groq, OpenRouter, xAI, DeepSeek, Cerebras, Perplexity, Together, Fireworks, Parasail, Mistral, Nebius, HuggingFace, z.ai GLM (`zai` pay-as-you-go + `zai-coding` Coding Plan), Moonshot (Kimi), MiniMax, Ollama, vLLM, SGLang. Any other OpenAI/Anthropic/Bedrock/Gemini/Azure-wire endpoint registers under a custom name via `kind`. See the [verification-status table](docs/03-providers.md#verification-status). |
| **Routing** | Primary + `fallbacks[]` provider failover, weighted key selection, per-key retry with backoff + jitter, per-provider concurrency isolation, dead-key vs used-key rotation — on unary **and** streaming. |
| **Governance** | Virtual keys: model allow/deny-lists, request rate limits, token budgets, per-period USD cost budgets. In-process counters by default, **shared Postgres** for horizontal scaling. |
| **Caching** | Two-tier semantic cache (exact-hash tier + embedding similarity), params/model-scoped. In-memory or persistent **pgvector** (survives restart, shared across replicas). |
| **Security** | Reversible AES-256-GCM redaction of captured bodies, RBAC (viewer/operator/admin) with fail-closed tokens, audited reveal. |
| **Observability** | Request audit log (filters + pagination + SSE tail), **per-request call tracing** (stage-by-stage waterfall incl. failed retries and time-to-first-token), **session grouping** (agent runs rolled up by `x-session-id` with per-run spend + a call-by-call journey), connected-client detection, analytics, opt-in content capture, Prometheus `/metrics`, **OTLP** traces + metrics with W3C `traceparent` propagation. |
| **MCP** | Agentic tool-calling over in-process + stdio MCP servers: discover → inject → execute → re-prompt. |
| **Docs for agents** | `/openapi.json`, `/llms.txt`, `/llms-full.txt`, and per-endpoint Markdown served straight off the gateway — generated from the route table and pinned to it by a test. |
| **Persistence** | SQLite (default) and Postgres behind store traits; in-memory fallback. |
| **Deploy** | Docker (single container + SQLite) or Helm (SQLite/Postgres, HPA, Ingress). |

## Performance

Rust core, measured with `cargo bench` against an instant mock provider (KGateway's own
overhead, no network). Full detail in [`docs/15-performance.md`](docs/15-performance.md).

| Path | Overhead |
|---|---|
| Bare engine (`chat` pipeline) | **~2.9 µs** |
| **Full production observability** (logging + governance) | **~3.5 µs / request** |
| + request/response content capture | ~4.3 µs |
| Redaction — no secrets (`RegexSet` prefilter miss) | ~0.30 µs |
| Weighted key selection (8 keys) | ~99 ns |

## Local development

### Prerequisites

- **Rust 1.88+** (MSRV; edition 2021)
- **Node 22+** and **pnpm** — only for the dashboard
- Optional: **Docker** for containerized runs, **Postgres** for the shared-counter / pgvector paths

### Run the backend

```bash
# config.json is gitignored; keys come from ${ENV}, never hard-coded.
# Start minimal (see Quick start) and add fields from config.example.json as needed.
export OPENAI_API_KEY=sk-...
cargo run -p kgateway-server -- --config config.json
```

Config hot-reloads without a restart: edit `config.json` and `kill -HUP $(pgrep -f kgateway-server)`.

### Run the dashboard

```bash
cd ui
pnpm install
NEXT_PUBLIC_KGATEWAY_URL=http://localhost:8080 pnpm dev   # http://localhost:3000
```

### Test & lint (the quality gate)

Every change should leave this green — it's what CI runs:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                 # 285 tests; no live DB or API keys required
# if ui/ changed:
pnpm --dir ui lint && pnpm --dir ui build
```

Postgres integration tests are env-gated (`KGATEWAY_TEST_PG`, `KGATEWAY_TEST_PGVECTOR`) and skip
when unset. Benchmarks: `cargo bench -p kgateway-core`.

### Workspace layout

Dependency direction: `server → plugins / providers / store → core`. Nothing depends on `server`.

| Crate | Role |
|---|---|
| `kgateway-core` | The engine: schemas, `Provider`/`LlmPlugin`/`RequestObserver` traits, routing + failover, streaming, plugin pipeline. No HTTP dependency — embeddable. |
| `kgateway-providers` | Provider connectors: 6 native + the OpenAI-compatible factory (19 vendors). |
| `kgateway-plugins` | Built-in plugins: logging, governance, semantic cache, redaction, pricing. |
| `kgateway-store` | Persistence behind `LogStore` / `VectorStore` / `GovernanceStore` traits — in-memory, SQLite, Postgres. |
| `kgateway-server` | axum HTTP gateway + control plane (the binary). |
| `ui/` | Next.js 15 (App Router) + Tailwind + shadcn/ui dashboard. |

### Adding things

- **New OpenAI-compatible provider** — one `(name, base_url)` entry in `openai_compat::KNOWN`.
- **New native provider** — implement the `Provider` trait in `kgateway-providers`, register in
  `app.rs`, add SSE-parse + error-mapping tests.
- **New plugin** — implement `LlmPlugin` or `RequestObserver` in `kgateway-plugins`, wire in `app.rs`.
- **New endpoint** — register in `app.rs` **and** `api_catalog::ENDPOINTS`; a drift test fails if
  the two disagree.

Architecture deep-dives live in [`docs/`](docs/README.md) — start with
[`docs/01-architecture.md`](docs/01-architecture.md).

## Deploy

```bash
# Docker — single container + SQLite volume
OPENAI_API_KEY=sk-... docker compose up --build

# Kubernetes — SQLite (single replica + PVC)
helm install kg charts/kgateway --set secretEnv.OPENAI_API_KEY=sk-...

# Kubernetes — Postgres (multi-replica + HPA)
helm install kg charts/kgateway \
  --set database.mode=postgres \
  --set database.url='postgres://user:pass@pg:5432/kgateway' \
  --set replicaCount=3 --set autoscaling.enabled=true \
  --set secretEnv.OPENAI_API_KEY=sk-...
```

See [`docs/06-deployment.md`](docs/06-deployment.md).

## Documentation

| | |
|---|---|
| [Getting started](docs/08-getting-started.md) | 5-minute guide: run, first request, dashboard, Claude Code / OMP / Pi setup, troubleshooting |
| [Configuration reference](docs/16-configuration.md) | Every config field, type, and default |
| [Architecture](docs/01-architecture.md) | Engine, traits, request flow, streaming |
| [Providers](docs/03-providers.md) | All 25 providers + live-verification status |
| [Security](docs/09-security.md) | Redaction, RBAC, key handling |
| [Performance](docs/15-performance.md) | Benchmark methodology + results |
| [Roadmap](docs/02-roadmap.md) | Milestone history and what's next |

## Contributing

Contributions are welcome — bug reports, provider connectors, plugins, docs.

1. Fork and branch.
2. Ship tests with the code (table-driven `#[tokio::test]`, matching the crate's existing style).
3. Run the [quality gate](#test--lint-the-quality-gate) — CI enforces fmt, clippy `-D warnings`,
   tests, an MSRV (1.88) build, the UI build, and the Docker build.
4. Never commit a real API key. Configs reference secrets as `${ENV}` only.

By contributing, you agree to the [CLA](./COMMERCIAL_LICENSE.md#contributor-license-agreement-cla).

## License

**Dual-licensed** — choose what fits your use case:

| | AGPL-3.0 (Open Source) | Commercial License |
|---|---|---|
| Self-host, modify, contribute | ✅ Free | ✅ Free |
| Offer as a managed/SaaS service | ✅ Must open-source changes | ✅ No sharing required |
| Embed in closed-source product | ❌ | ✅ |
| SSO, SLA, indemnification | — | ✅ |

- **Open source:** [AGPL-3.0](./LICENSE) — free for self-hosting, modification, and
  contribution. If you offer KGateway as a network service, you must open-source your
  modifications.
- **Commercial:** for closed-source use, managed services, or enterprise features, contact
  **kelvin.kwong@2rocksstudio.hk**. See [`COMMERCIAL_LICENSE.md`](./COMMERCIAL_LICENSE.md).
