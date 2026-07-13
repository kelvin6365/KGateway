# 04 — Plugin & hook system

A capability-segmented plugin system — small role traits composing a base, rather than one fat interface.

## Capability-segmented interfaces

Not one fat plugin interface. Small role traits composing a base:

| Role | KGateway trait | Layer | Hooks |
|---|---|---|---|
| Base | `Plugin` | all | `name`, `cleanup` |
| HTTP transport | `HttpPlugin` | HTTP edge | `http_pre`, `http_post`, `stream_chunk` |
| LLM | `LlmPlugin` | core | `pre_request`, `pre_llm`, `post_llm` |
| MCP | `McpPlugin` | MCP | connect / tool hooks |
| Config marshaller | `ConfigPlugin` | infra | config transform |
| Observability | `ObsPlugin` | infra | span/metric emission |

A plugin implements only the traits it needs. Registration is per-trait: `Vec<Arc<dyn LlmPlugin>>`, `Vec<Arc<dyn HttpPlugin>>`, etc.

## Two layers, on purpose

- **HTTP-transport layer** (`HttpPlugin`) works on **serde-serializable** `HttpRequestParts` / `HttpResponse`. Kept serializable so the same plugin can later run as a **WASM module** (`wasmtime`) — this split between native and WASM plugin hosts is the whole reason the boundary stays serializable. Maps to `tower::Layer`s in axum.
- **Core LLM layer** (`LlmPlugin`) works on the rich typed `ChatRequest` / `ChatResponse`. Only runs when going through the engine (also when embedded as a library).

## Two-phase pre-hooks (critical detail)

From the real code:

- `pre_request` — runs **ONCE** per top-level request. Its mutations are **committed** and observed by every fallback attempt, the provider call, and all later plugins. This is the canonical place to decide provider/model/fallbacks. **Errors are non-blocking** (logged as warning; cannot abort).
- `pre_llm` — runs **PER attempt**. Can **short-circuit** (cache hit, auth reject). Errors non-blocking.
- `post_llm` — runs in **reverse (LIFO)** order relative to `pre_llm`, giving wrapping semantics (first-registered = outermost).

```rust
pub enum PreOutcome {
    Continue(ChatRequest),      // proceed (possibly mutated)
    ShortCircuit(ChatResponse), // skip provider + remaining plugins, return this
}
```

**Rejection/gating must be a `ShortCircuit`, never an `Err`.** `Err` from a hook is logged and the pipeline continues — by design, so a buggy plugin can't take down traffic.

## Pipeline execution

```rust
// pre: forward order, thread the (maybe-mutated) request
for p in &llm_plugins {
    match p.pre_llm(ctx, req).await {
        Ok(PreOutcome::Continue(r)) => req = r,
        Ok(PreOutcome::ShortCircuit(resp)) => return finish_post(resp), // still run post in LIFO
        Err(e) => warn!(?e, plugin = p.name(), "pre_llm error (non-blocking)"),
    }
}
let resp = provider_call(req).await;
// post: reverse order (LIFO)
let mut resp = resp;
for p in llm_plugins.iter().rev() {
    resp = p.post_llm(ctx, resp).await.unwrap_or_else(|e| { warn!(...); resp_fallback });
}
```

## Built-in plugins

| Plugin | Purpose | Milestone |
|---|---|---|
| `logging` | request/response audit → store | M3 |
| `telemetry` | Prometheus metrics | M5 |
| `otel` | OpenTelemetry tracing | M5 |
| `governance` | budgets, rate limits, RBAC (short-circuit on breach) | M4 |
| `semanticcache` | vector-similarity response cache | M5 |
| `mocker` | canned responses for tests/dev | M3 |
| `maxim` / observability | external observability export | later |

## Custom & WASM plugins

- **Native:** implement the traits in `kgateway-plugins` (or an external crate) and register at startup.
- **WASM (M9, optional):** host with `wasmtime`; the serde-serializable HTTP-layer boundary is the contract. This is why `HttpPlugin` types must stay `Serialize + Deserialize`.
