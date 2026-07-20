//! The API catalog — one entry per endpoint the gateway serves.
//!
//! This is the single source of truth behind every documentation artifact:
//! `/openapi.json`, `/llms.txt`, `/llms-full.txt`, `/docs/{slug}.md`, and the
//! dashboard's `/docs` page. They are all rendered from here, so they cannot
//! disagree with each other.
//!
//! **They also cannot disagree with the router.** `drift_tests` below parses the
//! `.route(...)` table out of `app.rs` and asserts it matches this catalog exactly,
//! so adding an endpoint without documenting it fails `cargo test`. A hand-kept API
//! reference is wrong within a month, and a wrong reference is worse than none.

/// Who may call an endpoint. These four tiers are the thing people get wrong about
/// this gateway, so every rendering surfaces them per endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// Data plane. Open by default; requires a virtual key once `virtual_keys` are
    /// configured (strict mode).
    DataPlane,
    /// Open regardless of configuration — liveness and the docs artifacts themselves.
    Public,
    /// Control-plane read: `logs:view` (viewer role and above).
    LogsView,
    /// Control-plane write: `config:write` (operator and above).
    ConfigWrite,
    /// Un-redacting captured content: `logs:reveal` (admin only).
    LogsReveal,
}

impl Auth {
    pub fn label(self) -> &'static str {
        match self {
            Auth::DataPlane => "virtual key (strict mode only)",
            Auth::Public => "none",
            Auth::LogsView => "logs:view",
            Auth::ConfigWrite => "config:write",
            Auth::LogsReveal => "logs:reveal",
        }
    }

    /// Section heading used to group endpoints in every rendering.
    pub fn group(self) -> &'static str {
        match self {
            Auth::DataPlane | Auth::Public => "Data plane",
            Auth::LogsView => "Control plane (read)",
            Auth::ConfigWrite => "Control plane (write)",
            Auth::LogsReveal => "Redaction",
        }
    }
}

/// One documented parameter — a body field, path segment, or query string entry.
pub struct Param {
    pub name: &'static str,
    pub location: &'static str,
    pub ty: &'static str,
    pub required: bool,
    pub description: &'static str,
}

/// One endpoint.
pub struct Endpoint {
    pub method: &'static str,
    pub path: &'static str,
    pub auth: Auth,
    /// Short imperative summary, one line.
    pub summary: &'static str,
    /// Longer prose. Explains the things people get wrong, not what the name says.
    pub description: &'static str,
    pub params: &'static [Param],
    /// A runnable curl against a local gateway — real model ids, not placeholders.
    pub example: &'static str,
    /// Trimmed example response, or empty when the shape is obvious.
    pub response: &'static str,
}

impl Endpoint {
    /// Stable slug for `/docs/{slug}.md` and page anchors: `POST /v1/chat/completions`
    /// → `post-v1-chat-completions`.
    pub fn slug(&self) -> String {
        let mut s = String::with_capacity(self.path.len() + 8);
        s.push_str(&self.method.to_lowercase());
        for ch in self.path.chars() {
            match ch {
                'a'..='z' | 'A'..='Z' | '0'..='9' => s.push(ch.to_ascii_lowercase()),
                _ => {
                    if !s.ends_with('-') {
                        s.push('-');
                    }
                }
            }
        }
        s.trim_end_matches('-').to_string()
    }
}

const MODEL_PARAM: Param = Param {
    name: "model",
    location: "body",
    ty: "string",
    required: true,
    description: "Route as `provider/model`, e.g. `zai/glm-5.2`. An unprefixed model routes to the `openai` provider.",
};

pub const ENDPOINTS: &[Endpoint] = &[
    // ---- Data plane ----
    Endpoint {
        method: "POST",
        path: "/v1/chat/completions",
        auth: Auth::DataPlane,
        summary: "Chat completion (OpenAI-compatible)",
        description: "The main data-plane endpoint. Point any OpenAI SDK at the gateway and route to any \
configured provider with the `provider/model` convention. Set `stream: true` for SSE; the gateway \
injects `stream_options.include_usage` so token usage still arrives. Add `fallbacks[]` to fail over \
to other providers on retryable errors.",
        params: &[
            MODEL_PARAM,
            Param { name: "messages", location: "body", ty: "array", required: true,
                description: "Chat turns. A `system` message is extracted automatically for Anthropic-protocol providers, which take it as a top-level field." },
            Param { name: "stream", location: "body", ty: "boolean", required: false,
                description: "Stream the response as SSE. Failover happens before the first chunk reaches you, so a retry is invisible." },
            Param { name: "fallbacks", location: "body", ty: "array", required: false,
                description: "Provider failover chain, e.g. `[{\"provider\":\"openai\",\"model\":\"gpt-4o\"}]`. Tried on retryable errors only. Capped at 5." },
            Param { name: "tools", location: "body", ty: "array", required: false,
                description: "Function/tool definitions. Streamed tool-call fragments are reassembled by the gateway." },
            Param { name: "temperature", location: "body", ty: "number", required: false,
                description: "Sampling temperature. Also part of the semantic cache's scope key, so a different value is a different cache entry." },
        ],
        example: r#"curl http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "zai/glm-5.2",
    "messages": [{"role": "user", "content": "Say hi"}],
    "fallbacks": [{"provider": "openai", "model": "gpt-4o"}]
  }'"#,
        response: r#"{
  "id": "chatcmpl-…",
  "object": "chat.completion",
  "model": "glm-5.2",
  "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hi there"}, "finish_reason": "stop"}],
  "usage": {"prompt_tokens": 8, "completion_tokens": 3, "total_tokens": 11}
}"#,
    },
    Endpoint {
        method: "POST",
        path: "/v1/messages",
        auth: Auth::DataPlane,
        summary: "Anthropic Messages ingress",
        description: "Accepts inbound Anthropic-protocol requests and routes them to any provider, so \
Claude Code, the OMP CLI, the Pi CLI, and the Anthropic SDKs can all run through the gateway with \
governance, logging, failover, and caching applied. Streaming and tool use are translated in both \
directions. Two traps: `ANTHROPIC_BASE_URL` is the bare origin (clients append `/v1/messages` \
themselves), and the model needs the `provider/` prefix or it routes to the default provider.",
        params: &[
            MODEL_PARAM,
            Param { name: "messages", location: "body", ty: "array", required: true,
                description: "Anthropic-shaped turns. `tool_use` / `tool_result` blocks are supported." },
            Param { name: "max_tokens", location: "body", ty: "integer", required: true,
                description: "Required by the Anthropic protocol. Defaults to 4096 when routed to a provider that doesn't require it." },
            Param { name: "system", location: "body", ty: "string", required: false,
                description: "Top-level system prompt, per the Anthropic wire format." },
            Param { name: "stream", location: "body", ty: "boolean", required: false,
                description: "Emit Anthropic SSE events (`message_start`, `content_block_delta`, …)." },
        ],
        example: r#"# Point Claude Code at the gateway
export ANTHROPIC_BASE_URL=http://localhost:8080
export ANTHROPIC_AUTH_TOKEN=local
export ANTHROPIC_MODEL="zai/glm-5.2"
claude

# …or call it directly
curl http://localhost:8080/v1/messages \
  -H 'content-type: application/json' \
  -H 'anthropic-version: 2023-06-01' \
  -d '{"model":"zai/glm-5.2","max_tokens":64,"messages":[{"role":"user","content":"Say hi"}]}'"#,
        response: r#"{
  "id": "msg_…",
  "type": "message",
  "role": "assistant",
  "model": "glm-5.2",
  "content": [{"type": "text", "text": "Hi there"}],
  "stop_reason": "end_turn",
  "usage": {"input_tokens": 8, "output_tokens": 3}
}"#,
    },
    Endpoint {
        method: "GET",
        path: "/v1/models",
        auth: Auth::DataPlane,
        summary: "List every routable model",
        description: "Fans out to each configured provider's official list-models API and returns the \
union with `provider/model`-prefixed ids — every id is directly routable back through the gateway. \
Best-effort: a provider that errors, has no listable endpoint (bedrock, azure, gemini, cohere), or \
whose `${ENV}` key is unset is skipped rather than failing the response. Cached 5 minutes, \
invalidated when the provider set changes.",
        params: &[],
        example: "curl http://localhost:8080/v1/models",
        response: r#"{
  "object": "list",
  "data": [
    {"id": "zai/glm-5.2", "object": "model", "created": 1781625600, "owned_by": "zai"}
  ]
}"#,
    },
    Endpoint {
        method: "POST",
        path: "/v1/embeddings",
        auth: Auth::DataPlane,
        summary: "Create embeddings",
        description: "OpenAI-compatible embeddings. Dispatched only to providers that implement the \
capability; others return `operation not supported` before any upstream call is made.",
        params: &[
            MODEL_PARAM,
            Param { name: "input", location: "body", ty: "string | array", required: true,
                description: "Text (or array of texts) to embed." },
        ],
        example: r#"curl http://localhost:8080/v1/embeddings \
  -H 'content-type: application/json' \
  -d '{"model":"openai/text-embedding-3-small","input":["hello"]}'"#,
        response: "",
    },
    Endpoint {
        method: "POST",
        path: "/v1/images/generations",
        auth: Auth::DataPlane,
        summary: "Generate images",
        description: "OpenAI-compatible image generation, for providers that implement the capability.",
        params: &[
            MODEL_PARAM,
            Param { name: "prompt", location: "body", ty: "string", required: true, description: "What to draw." },
        ],
        example: r#"curl http://localhost:8080/v1/images/generations \
  -H 'content-type: application/json' \
  -d '{"model":"openai/dall-e-3","prompt":"a red bicycle"}'"#,
        response: "",
    },
    Endpoint {
        method: "POST",
        path: "/v1/audio/speech",
        auth: Auth::DataPlane,
        summary: "Text to speech",
        description: "Returns raw audio bytes with the upstream's content type, not JSON.",
        params: &[
            MODEL_PARAM,
            Param { name: "input", location: "body", ty: "string", required: true, description: "Text to speak." },
            Param { name: "voice", location: "body", ty: "string", required: false, description: "Provider-specific voice id." },
        ],
        example: r#"curl http://localhost:8080/v1/audio/speech \
  -H 'content-type: application/json' \
  -d '{"model":"openai/tts-1","input":"hello","voice":"alloy"}' --output speech.mp3"#,
        response: "",
    },
    Endpoint {
        method: "POST",
        path: "/v1/audio/transcriptions",
        auth: Auth::DataPlane,
        summary: "Transcribe audio",
        description: "Multipart upload, unlike every other endpoint here.",
        params: &[
            Param { name: "file", location: "form", ty: "binary", required: true, description: "Audio file to transcribe." },
            Param { name: "model", location: "form", ty: "string", required: true, description: "`provider/model`, e.g. `openai/whisper-1`." },
        ],
        example: r#"curl http://localhost:8080/v1/audio/transcriptions \
  -F file=@audio.mp3 -F model=openai/whisper-1"#,
        response: "",
    },
    Endpoint {
        method: "POST",
        path: "/v1/rerank",
        auth: Auth::DataPlane,
        summary: "Rerank documents",
        description: "Cohere-style reranking. Only providers implementing the capability accept it — \
OpenAI does not.",
        params: &[
            MODEL_PARAM,
            Param { name: "query", location: "body", ty: "string", required: true, description: "The search query." },
            Param { name: "documents", location: "body", ty: "array", required: true, description: "Candidate documents to score." },
        ],
        example: r#"curl http://localhost:8080/v1/rerank \
  -H 'content-type: application/json' \
  -d '{"model":"cohere/rerank-v3.5","query":"cats","documents":["dogs","cats are great"]}'"#,
        response: "",
    },
    Endpoint {
        method: "GET",
        path: "/health",
        auth: Auth::Public,
        summary: "Liveness probe",
        description: "Always open, even in strict mode. Suitable for a Kubernetes liveness probe.",
        params: &[],
        example: "curl http://localhost:8080/health",
        response: r#"{"status": "ok"}"#,
    },
    // ---- Docs artifacts (public, so an agent can discover the API) ----
    Endpoint {
        method: "GET",
        path: "/openapi.json",
        auth: Auth::Public,
        summary: "OpenAPI 3.1 specification",
        description: "The whole API as a standard spec — import it into Postman, Insomnia, or a client \
generator. Rendered from the same catalog as this page, so it can never drift from the router.",
        params: &[],
        example: "curl http://localhost:8080/openapi.json",
        response: "",
    },
    Endpoint {
        method: "GET",
        path: "/llms.txt",
        auth: Auth::Public,
        summary: "Documentation index for AI agents",
        description: "An llms.txt index: one line per endpoint linking to its Markdown page. Point a \
coding agent at this and it can discover the whole API. Use `/llms-full.txt` when you want every \
page inlined in a single fetch.",
        params: &[],
        example: "curl http://localhost:8080/llms.txt",
        response: "",
    },
    Endpoint {
        method: "GET",
        path: "/llms-full.txt",
        auth: Auth::Public,
        summary: "Full documentation as one file",
        description: "Every endpoint's Markdown concatenated, for pasting into a model's context in one \
go rather than following links.",
        params: &[],
        example: "curl http://localhost:8080/llms-full.txt",
        response: "",
    },
    Endpoint {
        method: "GET",
        path: "/docs/{file}",
        auth: Auth::Public,
        summary: "One endpoint as Markdown",
        description: "The Markdown twin of a single endpoint's documentation — what the `llms.txt` \
index links to. The slug is the method and path lowercased and hyphenated.",
        params: &[Param {
            name: "file",
            location: "path",
            ty: "string",
            required: true,
            description: "`<slug>.md`, e.g. `post-v1-chat-completions.md`. The slug is the method and path lowercased and hyphenated.",
        }],
        example: "curl http://localhost:8080/docs/post-v1-chat-completions.md",
        response: "",
    },
    // ---- Control plane: read ----
    Endpoint {
        method: "GET",
        path: "/api/logs",
        auth: Auth::LogsView,
        summary: "Query the request audit log",
        description: "Filtered, sorted, paginated. Deliberately lean: captured bodies and trace spans \
are omitted here and fetched per-request from `/api/logs/{id}`.",
        params: &[
            Param { name: "limit", location: "query", ty: "integer", required: false, description: "Page size, capped at 200. Default 50." },
            Param { name: "offset", location: "query", ty: "integer", required: false, description: "Rows to skip." },
            Param { name: "sort_by", location: "query", ty: "string", required: false, description: "`created_at` | `latency` | `tokens` | `cost`." },
            Param { name: "order", location: "query", ty: "string", required: false, description: "`asc` | `desc` (default)." },
            Param { name: "provider", location: "query", ty: "string", required: false, description: "Exact provider match." },
            Param { name: "model", location: "query", ty: "string", required: false, description: "Exact model match." },
            Param { name: "status", location: "query", ty: "integer", required: false, description: "Exact HTTP status." },
            Param { name: "virtual_key", location: "query", ty: "string", required: false, description: "Exact virtual-key match." },
            Param { name: "cache_hit", location: "query", ty: "boolean", required: false, description: "Only hits, or only misses." },
            Param { name: "since_ms", location: "query", ty: "integer", required: false, description: "Unix ms lower bound on `created_at`." },
            Param { name: "search", location: "query", ty: "string", required: false, description: "Case-insensitive substring over request id / provider / model." },
        ],
        example: "curl -H 'authorization: Bearer $KG_ADMIN' \\\n  'http://localhost:8080/api/logs?limit=20&status=200'",
        response: "",
    },
    Endpoint {
        method: "GET",
        path: "/api/logs/{id}",
        auth: Auth::LogsView,
        summary: "One request in full, with its trace",
        description: "The only endpoint that returns captured request/response bodies and the per-stage \
**trace spans** behind the call waterfall — list and live-tail responses omit both. Spans arrive as a \
JSON array and carry no request content or upstream error text, only stage names, timings, and \
gateway-authored outcomes.",
        params: &[Param { name: "id", location: "path", ty: "string", required: true, description: "The request id." }],
        example: "curl -H 'authorization: Bearer $KG_ADMIN' \\\n  http://localhost:8080/api/logs/$REQUEST_ID",
        response: r#"{
  "request_id": "…", "provider": "zai", "model": "glm-5.2", "status": 200, "latency_ms": 2887,
  "spans": [
    {"name": "attempt · zai key=coding-plan", "category": "network", "start_us": 157, "dur_us": 2886000, "depth": 1}
  ]
}"#,
    },
    Endpoint {
        method: "GET",
        path: "/api/logs/stream",
        auth: Auth::LogsView,
        summary: "Live-tail the audit log (SSE)",
        description: "Server-sent events, one per completed request. Authenticates via a `?token=` query \
parameter rather than a header, because browser `EventSource` cannot send one. Payloads follow the \
lean list contract — no bodies, no spans.",
        params: &[Param { name: "token", location: "query", ty: "string", required: true, description: "Control-plane token, since EventSource can't set headers." }],
        example: "curl -N 'http://localhost:8080/api/logs/stream?token=$KG_ADMIN'",
        response: "",
    },
    Endpoint {
        method: "GET",
        path: "/api/logs/stats",
        auth: Auth::LogsView,
        summary: "Aggregate stats over a filter",
        description: "Totals, success/error counts, average latency, tokens, cost, cache hits. Takes the \
same filter parameters as `/api/logs`.",
        params: &[],
        example: "curl -H 'authorization: Bearer $KG_ADMIN' http://localhost:8080/api/logs/stats",
        response: "",
    },
    Endpoint {
        method: "GET",
        path: "/api/logs/histogram",
        auth: Auth::LogsView,
        summary: "Distribution of latency, cost, or tokens",
        description: "Bucketed distribution for the analytics charts.",
        params: &[
            Param { name: "metric", location: "query", ty: "string", required: false, description: "`latency` (default) | `cost` | `tokens`." },
            Param { name: "buckets", location: "query", ty: "integer", required: false, description: "Bucket count, default 20." },
        ],
        example: "curl -H 'authorization: Bearer $KG_ADMIN' \\\n  'http://localhost:8080/api/logs/histogram?metric=latency'",
        response: "",
    },
    Endpoint {
        method: "GET",
        path: "/api/logs/timeseries",
        auth: Auth::LogsView,
        summary: "Requests and errors over time",
        description: "Bucketed counts for the requests-over-time chart.",
        params: &[Param { name: "bucket_ms", location: "query", ty: "integer", required: false, description: "Bucket width in ms, default 60000." }],
        example: "curl -H 'authorization: Bearer $KG_ADMIN' http://localhost:8080/api/logs/timeseries",
        response: "",
    },
    Endpoint {
        method: "GET",
        path: "/api/logs/rankings",
        auth: Auth::LogsView,
        summary: "Top models, providers, or virtual keys",
        description: "Leaderboard over a chosen dimension and metric.",
        params: &[
            Param { name: "by", location: "query", ty: "string", required: false, description: "`model` (default) | `provider` | `virtual_key`." },
            Param { name: "metric", location: "query", ty: "string", required: false, description: "`count` | `cost` | `tokens` | `errors`." },
            Param { name: "limit", location: "query", ty: "integer", required: false, description: "How many rows." },
        ],
        example: "curl -H 'authorization: Bearer $KG_ADMIN' \\\n  'http://localhost:8080/api/logs/rankings?by=model&metric=cost'",
        response: "",
    },
    Endpoint {
        method: "GET",
        path: "/api/logs/filterdata",
        auth: Auth::LogsView,
        summary: "Distinct values for the filter controls",
        description: "The providers, models, and virtual keys actually present in the log, so the \
dashboard's filter dropdowns offer real options.",
        params: &[],
        example: "curl -H 'authorization: Bearer $KG_ADMIN' http://localhost:8080/api/logs/filterdata",
        response: "",
    },
    Endpoint {
        method: "GET",
        path: "/api/logs/dropped",
        auth: Auth::LogsView,
        summary: "Count of audit rows dropped under load",
        description: "The async batch writer drops rows rather than blocking requests when its channel \
is full. A non-zero count here means the log is incomplete.",
        params: &[],
        example: "curl -H 'authorization: Bearer $KG_ADMIN' http://localhost:8080/api/logs/dropped",
        response: "",
    },
    Endpoint {
        method: "GET",
        path: "/api/providers",
        auth: Auth::LogsView,
        summary: "Configured providers and their capabilities",
        description: "Which providers are registered and which capabilities (chat, embeddings, images, \
audio, rerank) each one implements.",
        params: &[],
        example: "curl -H 'authorization: Bearer $KG_ADMIN' http://localhost:8080/api/providers",
        response: "",
    },
    Endpoint {
        method: "GET",
        path: "/api/config/providers",
        auth: Auth::LogsView,
        summary: "Provider configuration",
        description: "The provider block from `config.json`, with key values replaced by `<redacted>`.",
        params: &[],
        example: "curl -H 'authorization: Bearer $KG_ADMIN' http://localhost:8080/api/config/providers",
        response: "",
    },
    Endpoint {
        method: "GET",
        path: "/api/config/virtual-keys",
        auth: Auth::LogsView,
        summary: "Virtual-key configuration",
        description: "Configured virtual keys with their model allow/deny-lists, rate limits, and budgets.",
        params: &[],
        example: "curl -H 'authorization: Bearer $KG_ADMIN' http://localhost:8080/api/config/virtual-keys",
        response: "",
    },
    Endpoint {
        method: "GET",
        path: "/api/mcp/tools",
        auth: Auth::LogsView,
        summary: "Discovered MCP tools",
        description: "Tools exposed by the configured MCP servers, as injected into agentic requests.",
        params: &[],
        example: "curl -H 'authorization: Bearer $KG_ADMIN' http://localhost:8080/api/mcp/tools",
        response: "",
    },
    Endpoint {
        method: "GET",
        path: "/api/status",
        auth: Auth::LogsView,
        summary: "Runtime and feature summary",
        description: "Which plugins are active, the database mode, cache settings — the non-secret \
runtime picture behind the dashboard's settings pages.",
        params: &[],
        example: "curl -H 'authorization: Bearer $KG_ADMIN' http://localhost:8080/api/status",
        response: "",
    },
    Endpoint {
        method: "GET",
        path: "/api/whoami",
        auth: Auth::LogsView,
        summary: "The caller's role and permissions",
        description: "Lets a client show or hide controls it isn't allowed to use, rather than \
discovering it by getting a 403.",
        params: &[],
        example: "curl -H 'authorization: Bearer $KG_ADMIN' http://localhost:8080/api/whoami",
        response: r#"{"role": "admin", "permissions": ["logs:view", "config:write", "logs:reveal"]}"#,
    },
    Endpoint {
        method: "GET",
        path: "/metrics",
        auth: Auth::LogsView,
        summary: "Prometheus metrics",
        description: "Prometheus text exposition: request counts by status, latency, tokens, cost.",
        params: &[],
        example: "curl -H 'authorization: Bearer $KG_ADMIN' http://localhost:8080/metrics",
        response: "",
    },
    // ---- Control plane: write ----
    Endpoint {
        method: "PUT",
        path: "/api/config/providers/{name}",
        auth: Auth::ConfigWrite,
        summary: "Create or update a provider",
        description: "Writes the provider into `config.json` and rebuilds the engine without a restart. \
`${ENV}` references are preserved on disk, so no resolved secret is ever persisted.",
        params: &[
            Param { name: "name", location: "path", ty: "string", required: true, description: "The route prefix, e.g. `zai-coding`." },
            Param { name: "kind", location: "body", ty: "string", required: false, description: "`openai` | `anthropic` | `bedrock` | `gemini` | `azure`. Inferred from the name when omitted." },
            Param { name: "base_url", location: "body", ty: "string", required: false, description: "Override the vendor default. Required for unknown provider names." },
            Param { name: "keys", location: "body", ty: "array", required: true, description: "Weighted API keys; values may be `${ENV}` references." },
        ],
        example: r#"curl -X PUT -H 'authorization: Bearer $KG_ADMIN' \
  -H 'content-type: application/json' \
  http://localhost:8080/api/config/providers/moonshot \
  -d '{"keys":[{"id":"default","value":"${MOONSHOT_API_KEY}","weight":1}]}'"#,
        response: "",
    },
    Endpoint {
        method: "DELETE",
        path: "/api/config/providers/{name}",
        auth: Auth::ConfigWrite,
        summary: "Remove a provider",
        description: "Removes it from `config.json` and rebuilds the engine. In-flight requests finish.",
        params: &[Param { name: "name", location: "path", ty: "string", required: true, description: "Provider to remove." }],
        example: "curl -X DELETE -H 'authorization: Bearer $KG_ADMIN' \\\n  http://localhost:8080/api/config/providers/moonshot",
        response: "",
    },
    Endpoint {
        method: "PUT",
        path: "/api/config/virtual-keys/{id}",
        auth: Auth::ConfigWrite,
        summary: "Create or update a virtual key",
        description: "Adding the first virtual key flips the data plane into **strict mode**: every \
request must then present a known key, including `/v1/models`.",
        params: &[
            Param { name: "id", location: "path", ty: "string", required: true, description: "The bearer token clients will send." },
            Param { name: "allowed_models", location: "body", ty: "array", required: false, description: "Allow-list; empty means all models." },
            Param { name: "denied_models", location: "body", ty: "array", required: false, description: "Deny-list; a match always wins over the allow-list." },
            Param { name: "max_requests_per_min", location: "body", ty: "integer", required: false, description: "Fixed-window rate limit; trips 429." },
            Param { name: "max_total_tokens", location: "body", ty: "integer", required: false, description: "Cumulative token budget." },
            Param { name: "max_cost_per_period", location: "body", ty: "number", required: false, description: "USD budget per rolling period, from the estimated price table." },
        ],
        example: r#"curl -X PUT -H 'authorization: Bearer $KG_ADMIN' \
  -H 'content-type: application/json' \
  http://localhost:8080/api/config/virtual-keys/vk_team_alpha \
  -d '{"name":"Team Alpha","max_requests_per_min":60}'"#,
        response: "",
    },
    Endpoint {
        method: "DELETE",
        path: "/api/config/virtual-keys/{id}",
        auth: Auth::ConfigWrite,
        summary: "Remove a virtual key",
        description: "Removing the last one returns the data plane to open mode.",
        params: &[Param { name: "id", location: "path", ty: "string", required: true, description: "Virtual key to remove." }],
        example: "curl -X DELETE -H 'authorization: Bearer $KG_ADMIN' \\\n  http://localhost:8080/api/config/virtual-keys/vk_team_alpha",
        response: "",
    },
    // ---- Redaction ----
    Endpoint {
        method: "GET",
        path: "/api/logs/{id}/reveal",
        auth: Auth::LogsReveal,
        summary: "Un-redact a captured request",
        description: "Decrypts the AES-GCM reversible redaction mapping and returns the original \
request/response bodies. Admin-only and audited — the reveal itself is logged with the token's name.",
        params: &[Param { name: "id", location: "path", ty: "string", required: true, description: "The request id." }],
        example: "curl -H 'authorization: Bearer $KG_ADMIN' \\\n  http://localhost:8080/api/logs/$REQUEST_ID/reveal",
        response: "",
    },
];

#[cfg(test)]
mod drift_tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Pull `(METHOD, path)` pairs out of the router source.
    ///
    /// Reading the source is blunt, but it is the only way to enumerate an axum
    /// `Router` — and bluntness is fine for a guard whose whole job is to fail loudly
    /// when someone adds an endpoint and forgets to document it.
    fn routes_declared_in_app_rs() -> BTreeSet<(String, String)> {
        let src = include_str!("app.rs");
        // Only the router section; `app.rs` mentions paths in comments and tests too.
        let body = src
            .split_once("pub fn build_router")
            .expect("build_router exists")
            .1;

        let mut out = BTreeSet::new();
        for (i, _) in body.match_indices(".route(") {
            let rest = &body[i..];
            let Some(open) = rest.find('"') else { continue };
            let Some(close) = rest[open + 1..].find('"') else {
                continue;
            };
            let path = &rest[open + 1..open + 1 + close];
            // The handler list runs to the closing paren of this .route(...) call.
            let seg_end = rest.find("\n        .").unwrap_or(rest.len().min(400));
            let seg = &rest[..seg_end];
            for (verb, method) in [
                ("get(", "GET"),
                ("post(", "POST"),
                ("put(", "PUT"),
                ("delete(", "DELETE"),
            ] {
                if seg.contains(verb) {
                    out.insert((method.to_string(), path.to_string()));
                }
            }
        }
        assert!(
            out.len() > 20,
            "route extraction found only {} routes — the parser has drifted from app.rs",
            out.len()
        );
        out
    }

    fn catalog_pairs() -> BTreeSet<(String, String)> {
        ENDPOINTS
            .iter()
            .map(|e| (e.method.to_string(), e.path.to_string()))
            .collect()
    }

    #[test]
    fn every_route_is_documented() {
        let routes = routes_declared_in_app_rs();
        let documented = catalog_pairs();
        let missing: Vec<_> = routes.difference(&documented).collect();
        assert!(
            missing.is_empty(),
            "these routes are registered but missing from api_catalog.rs — document them \
             (the API reference, openapi.json and llms.txt are generated from it): {missing:#?}"
        );
    }

    #[test]
    fn every_documented_endpoint_exists() {
        let routes = routes_declared_in_app_rs();
        let documented = catalog_pairs();
        let phantom: Vec<_> = documented.difference(&routes).collect();
        assert!(
            phantom.is_empty(),
            "these endpoints are documented but no longer registered in app.rs — remove or \
             fix them, the docs would be lying: {phantom:#?}"
        );
    }

    #[test]
    fn slugs_are_unique_and_url_safe() {
        let mut seen = BTreeSet::new();
        for e in ENDPOINTS {
            let slug = e.slug();
            assert!(
                slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "slug must be URL-safe: {slug}"
            );
            assert!(seen.insert(slug.clone()), "duplicate slug: {slug}");
        }
    }

    #[test]
    fn slug_shape_is_stable() {
        // Slugs appear in llms.txt links and page anchors; changing the scheme breaks
        // every link an agent has already seen.
        let chat = ENDPOINTS
            .iter()
            .find(|e| e.path == "/v1/chat/completions")
            .unwrap();
        assert_eq!(chat.slug(), "post-v1-chat-completions");
        let detail = ENDPOINTS
            .iter()
            .find(|e| e.path == "/api/logs/{id}")
            .unwrap();
        assert_eq!(detail.slug(), "get-api-logs-id");
    }

    #[test]
    fn every_endpoint_is_actually_documented() {
        // Guard against an entry added as a stub to silence the drift test.
        for e in ENDPOINTS {
            assert!(
                !e.summary.is_empty(),
                "{} {} has no summary",
                e.method,
                e.path
            );
            assert!(
                e.description.len() > 40,
                "{} {} needs a real description, not a stub",
                e.method,
                e.path
            );
            assert!(
                !e.example.is_empty(),
                "{} {} has no example",
                e.method,
                e.path
            );
        }
    }
}
