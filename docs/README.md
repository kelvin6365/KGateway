# KGateway

A high-performance, open-source **AI/LLM gateway** — a Rust backend + Next.js frontend.

One OpenAI-compatible API in front of every major LLM provider, with failover, load balancing, semantic caching, governance (virtual keys / budgets / rate limits), MCP tool-calling, and full observability.

## Why this exists

KGateway implements a proven gateway architecture in **Rust** for:

- **Lower, more predictable latency** — no GC pauses, zero-cost abstractions, `tokio` async.
- **Memory safety without a runtime** — ownership replaces the manual object-pooling and GC-fighting a garbage-collected runtime forces.
- **A modern, typed UI** — Next.js + Tailwind + shadcn/ui.

## Stack

| Layer | Reference (Go) | KGateway (this project) |
|---|---|---|
| HTTP server | fasthttp | `axum` on `hyper`/`tokio` |
| Provider HTTP client | fasthttp | `reqwest` (pooled per provider) |
| JSON | sonic | `serde_json` → `simd-json` in hot paths |
| Streaming | `chan chan` | `tokio` streams / `async-stream`, axum SSE |
| Config schema | hand-written JSON Schema | `serde` + `schemars` (generated) |
| Persistence | pluggable stores | `sqlx` — SQLite default, Postgres option |
| Vector store | pluggable | `pgvector` (Postgres) / `sqlite-vec` |
| MCP | custom + Starlark | `rmcp` (official Rust MCP SDK) |
| Observability | Prometheus + OTel | `tracing` + `opentelemetry` + `metrics` |
| UI | React + Vite + TanStack + RTK Query | Next.js (App Router) + Tailwind + shadcn/ui + TanStack Query |
| Plugins | native `.so` + WASM | native (trait objects) + WASM via `wasmtime` (later phase) |

## Documents in this folder

| Doc | What it covers |
|---|---|
| [08-getting-started.md](./08-getting-started.md) | **Start here** — run the gateway, first request, dashboard, Docker/Helm |
| [01-architecture.md](./01-architecture.md) | Crate layout, request flow, the core traits |
| [02-roadmap.md](./02-roadmap.md) | Phased milestones M0–M9 (all done), deliverables, follow-ons |
| [03-providers.md](./03-providers.md) | Connectors, the capability-trait strategy, port order |
| [04-plugins.md](./04-plugins.md) | Plugin/hook system, the two-layer model, short-circuit semantics |
| [05-frontend.md](./05-frontend.md) | Next.js dashboard plan, pages, API contract |
| [06-deployment.md](./06-deployment.md) | Docker, Helm, config model, benchmarks |
| [07-quality.md](./07-quality.md) | Per-part testing + code-review process |
| [09-security.md](./09-security.md) | Security review findings + resolution status |

## Guiding principles

1. **Avoid a 100+ method `Provider` god-interface.** Split into a slim core trait + opt-in capability traits. Unsupported ops shouldn't compile, not error at runtime. See [03-providers.md](./03-providers.md).
2. **Use a capability-segmented plugin system** (`BasePlugin` + `HTTPTransportPlugin` + `LLMPlugin` + ...). It's the best-designed part of the architecture. See [04-plugins.md](./04-plugins.md).
3. **Two-phase pre-hooks:** a once-per-request routing phase (`PreRequest`) and a per-attempt phase (`PreLLM`). This is what makes failover + plugins compose.
4. **Short-circuit is a typed value, not an error.** Errors are non-blocking (logged); rejection/cache-hit is an explicit short-circuit response.
5. **Per-provider isolation.** Each provider gets its own queue + concurrency cap so one bad provider can't cascade.
6. **`core` is a standalone library.** The HTTP server is a thin binder on top, so KGateway is embeddable as a Rust crate too.

## The reference implementation

A mature reference gateway serves as the source of truth for wire formats, provider quirks, and edge cases — but re-implement idiomatically, don't transliterate Go.
