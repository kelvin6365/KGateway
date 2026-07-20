# 01 — Architecture

## Crate layout (Cargo workspace)

```
kgateway/
├── Cargo.toml                 # workspace manifest
├── crates/
│   ├── kgateway-core/         # engine: schemas, traits, routing, plugin pipeline
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── schema/        # core request/response types (serde types)
│   │       ├── provider.rs    # Provider trait + capability traits
│   │       ├── plugin.rs      # plugin traits (Base/Llm/Http/Mcp/Observability)
│   │       ├── engine.rs      # Kgateway struct: queue, dispatch, fallback
│   │       ├── router.rs      # provider/model/key selection, load balancing
│   │       ├── keyselect.rs   # weighted-random key selection
│   │       ├── context.rs     # request-scoped Ctx (incl. trace span collector)
│   │       ├── trace.rs       # per-request Span/SpanCategory — the call waterfall
│   │       └── error.rs       # KgError (structured provider/status error)
│   ├── kgateway-providers/    # provider implementations
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── openai.rs      # reference implementation
│   │       ├── openai_compat.rs # wrapper: groq, ollama, zai, moonshot, minimax, ...
│   │       ├── model_listing.rs # upstream list-models fetchers (aggregated /v1/models)
│   │       └── anthropic.rs
│   ├── kgateway-plugins/      # built-in plugins (logging, telemetry, cache, governance)
│   ├── kgateway-store/        # persistence: store traits + sqlite/postgres impls
│   └── kgateway-server/       # axum HTTP transport (the binary)
│       └── src/
│           ├── main.rs
│           ├── app.rs         # router, middleware/tower layers
│           ├── handlers/      # /v1/chat/completions, /v1/embeddings, config APIs...
│           ├── api_catalog.rs # every endpoint documented; drift-tested against app.rs
│           ├── api_docs.rs    # renders openapi.json / llms.txt / per-endpoint .md
│           └── integrations/  # SDK-compat: openai, anthropic, ... request adapters
├── ui/                        # Next.js dashboard
├── docker/                    # Dockerfile(s), compose
└── docs/
```

**Why this split**: a clean separation of engine (`core`), persistence (`kgateway-store`), transport (`kgateway-server`), providers, and plugins. `kgateway-core` has **no HTTP/axum dependency** — it's a pure library, so the gateway is embeddable as a crate, not just runnable as a server.

## Request flow

The request pipeline:

```
HTTP request (axum handler)
 └─ integration adapter        # normalize OpenAI/Anthropic/... wire form → internal ChatRequest
    └─ HTTP-layer plugins       # tower layers: HttpTransportPreHook (serde-safe, WASM-capable)
       └─ engine.chat(req)
          ├─ PreRequestHook     # ONCE: routing/model/fallback decisions, committed mutations
          ├─ for each attempt (primary then fallbacks):
          │   ├─ PreLLMHook     # PER-ATTEMPT: auth, rate-limit, cache-lookup → may short-circuit
          │   ├─ router: pick provider + key (weighted-random ~O(1))
          │   ├─ provider queue (mpsc + Semaphore, isolated per provider)
          │   ├─ provider.chat() / chat_stream()   # reqwest, pooled client
          │   └─ PostLLMHook    # reverse/LIFO order: logging, telemetry, cache-write
          └─ return response / stream
       └─ HttpTransportPostHook / StreamChunkHook
    └─ integration adapter       # internal → client wire form
 └─ serialize → client (JSON or SSE)
```

## Core traits

### Provider — slim core + capability traits

We deliberately avoid a single god-interface with 100+ methods that every provider must satisfy. Instead:

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn key(&self) -> ProviderKey;
    async fn chat(&self, ctx: &Ctx, key: &ApiKey, req: ChatRequest)
        -> Result<ChatResponse, KgError>;
    async fn chat_stream(&self, ctx: &Ctx, key: &ApiKey, req: ChatRequest)
        -> Result<ChunkStream, KgError>;
}

// Opt-in capabilities — a provider implements only what it supports.
#[async_trait] pub trait Embeddings   { async fn embed(&self, ..) -> ..; }
#[async_trait] pub trait Images       { async fn image_generate(&self, ..) -> ..; /* edit, variation */ }
#[async_trait] pub trait Audio        { async fn speech(&self, ..) -> ..; async fn transcribe(&self, ..) -> ..; }
#[async_trait] pub trait Rerank       { async fn rerank(&self, ..) -> ..; }
#[async_trait] pub trait Batch        { /* create/list/retrieve/cancel/delete/results */ }
#[async_trait] pub trait Files        { /* upload/list/retrieve/delete/content */ }
#[async_trait] pub trait Responses    { /* OpenAI Responses API */ }
```

Capability discovery uses a registry that records which traits each provider satisfies (see `router.rs`), so the engine returns a clean `Unsupported` before dispatch.

### Plugin — capability-segmented

Small role interfaces composing a base:

```rust
pub trait Plugin: Send + Sync {           // BasePlugin
    fn name(&self) -> &str;
    async fn cleanup(&self) -> Result<(), KgError> { Ok(()) }
}

#[async_trait]
pub trait LlmPlugin: Plugin {             // LLMPlugin
    // ONCE per request; mutations committed & seen by all fallback attempts. Error = non-blocking (logged).
    async fn pre_request(&self, ctx: &Ctx, req: &mut ChatRequest) -> Result<(), KgError> { Ok(()) }
    // PER attempt; may short-circuit. Error = non-blocking.
    async fn pre_llm(&self, ctx: &Ctx, req: ChatRequest) -> Result<PreOutcome, KgError>
        { Ok(PreOutcome::Continue(req)) }
    // Reverse (LIFO) order relative to pre_llm.
    async fn post_llm(&self, ctx: &Ctx, resp: Result<ChatResponse, KgError>) -> Result<ChatResponse, KgError>;
}

pub enum PreOutcome { Continue(ChatRequest), ShortCircuit(ChatResponse) }

#[async_trait]
pub trait HttpPlugin: Plugin {            // HTTPTransportPlugin — serde-safe (WASM-capable)
    async fn http_pre(&self, ctx: &Ctx, req: &mut HttpRequestParts) -> Result<Option<HttpResponse>, KgError> { Ok(None) }
    async fn http_post(&self, ctx: &Ctx, req: &HttpRequestParts, resp: &mut HttpResponse) -> Result<(), KgError> { Ok(()) }
    async fn stream_chunk(&self, ctx: &Ctx, chunk: StreamChunk) -> Result<Option<StreamChunk>, KgError> { Ok(Some(chunk)) }
}
```

Registered as `Vec<Arc<dyn LlmPlugin>>`. Pre-hooks iterate forward; post-hooks iterate `.rev()` (LIFO wrapping). HTTP-layer plugins map to `tower::Layer`s.

### Context

Rather than a mutable ambient context with reserved keys, we use an owned, request-scoped struct threaded explicitly:

```rust
pub struct Ctx {
    pub request_id: RequestId,
    pub virtual_key: Option<VirtualKeyId>,
    pub attempt: u32,
    pub started_at: Instant,
    ext: HashMap<TypeId, Box<dyn Any + Send + Sync>>, // typed extensions, like axum::Extensions
}
```

No `RwLock`-on-context dance needed — ownership + `&mut` gives safe mutation.

### Errors

`KgError` is a structured enum carrying provider, status, retryable flag, and upstream body. The router uses `KgError::is_retryable()` to drive fallback.

## Concurrency & isolation (provider isolation)

Each provider owns:
- a `tokio::sync::mpsc` request channel (or a `Semaphore` bounding in-flight calls),
- a dedicated worker set,
- its own `reqwest::Client` with a tuned connection pool.

One provider saturating or failing cannot starve others. Backpressure is explicit via bounded channels.

## What we deliberately DON'T adopt

| Go-gateway pattern | Why skip in Rust |
|---|---|
| `sync.Pool` object pooling everywhere | Ownership removes GC pressure; use `bytes::BytesMut` reuse only where profiling demands. |
| `sonic` custom JSON | `serde_json` is fine; `simd-json` for hot paths later. |
| fasthttp | `hyper`/`axum` is already zero-alloc-ish and idiomatic. |
| Manual context mutability via RWMutex | Owned `Ctx` + `&mut`. |
| Starlark code-mode sandbox (initially) | Defer; revisit in MCP phase with a Rust sandbox (`wasmtime`/`rquickjs`). |
