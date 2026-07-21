//! The engine: orchestrates the request pipeline (plugins → routing → provider call).
//!
//! M1: single provider, plugin pipeline wired (pre/post + short-circuit).
//! M2: provider-level failover over `req.fallbacks`, key-level retry with weighted
//! selection, per-provider concurrency isolation.

use crate::context::{Ctx, RequestId};
use crate::error::{KgError, KgErrorKind};
use crate::keyselect;
use crate::mcp::McpClient;
use crate::observer::{CallRecord, RequestObserver};
use crate::plugin::{LlmPlugin, PreOutcome};
use crate::provider::{
    ApiKey, ChunkStream, EmbeddingRequest, EmbeddingResponse, ImageGenerationRequest,
    ImageResponse, ProviderKey, RerankRequest, RerankResponse, SpeechRequest, SpeechResponse,
    TranscriptionRequest, TranscriptionResponse,
};
use crate::router::{ProviderEntry, Registry};
use crate::schema::{
    ChatRequest, ChatResponse, Delta, Message, Role, StreamChoice, StreamChunk, ToolCallAccumulator,
};
use crate::trace::SpanCategory;
use futures::StreamExt;
use rand::rngs::StdRng;
use rand::SeedableRng;
use std::sync::Arc;

/// Max distinct keys to try within a single provider before giving up on it.
const MAX_KEYS_PER_PROVIDER: usize = 3;

/// Default cap on agentic tool-execution rounds (safety against infinite tool loops).
pub const DEFAULT_MAX_TOOL_ROUNDS: usize = 8;

/// Max client-supplied fallback targets honored per request (DoS guard — an unbounded
/// chain would fan one request into many outbound calls against the operator's keys).
pub const MAX_FALLBACKS: usize = 5;

/// Idle (inter-chunk) timeout for a streamed response, including time-to-first-chunk. If
/// the upstream sends nothing for this long, the stream is aborted — releasing the
/// per-provider concurrency permit so a hung upstream can't pin capacity indefinitely.
/// Generous enough to tolerate real time-to-first-token latency on large prompts.
const STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Below this, a concurrency permit was effectively uncontended — recording it would
/// add a noise row to every trace without telling the operator anything.
const SEMAPHORE_WAIT_SPAN_FLOOR: std::time::Duration = std::time::Duration::from_micros(200);

/// Describe a failed upstream attempt for the trace **in our own words**.
///
/// Deliberately does NOT copy `KgError::message`: that holds the raw upstream error
/// body, which can echo the user's prompt back (a provider rejecting content quotes
/// it), and spans are persisted unconditionally — unlike captured bodies, which are
/// opt-in twice and pass through the redactor. Copying it would smuggle request
/// content into the audit row with content capture switched off, and unbounded in
/// size. The raw detail stays in the row's `error_message`, which is the sanctioned
/// server-side field for it.
fn attempt_failure_detail(e: &KgError) -> String {
    let cause = match e.kind {
        KgErrorKind::RateLimit => "Rate limited by the provider.",
        KgErrorKind::Auth => "The provider rejected this key.",
        KgErrorKind::Network => "Network error reaching the provider.",
        KgErrorKind::BadRequest => "The provider rejected the request as invalid.",
        KgErrorKind::Provider => "The provider returned an error.",
        KgErrorKind::Unsupported => "The provider does not support this operation.",
        KgErrorKind::Internal => "Gateway error while dispatching.",
    };
    let next = if e.is_retryable() {
        " Retryable — failing over."
    } else if e.is_key_rotatable() {
        " Rotating to another key."
    } else {
        " Not retryable — giving up."
    };
    format!("{cause}{next} See this request's error detail for the upstream text.")
}

/// Which colour band a plugin's span belongs to. Cache plugins get their own band
/// because "did the cache save us a call?" is the question traces are read for.
fn plugin_category(plugin_name: &str) -> SpanCategory {
    if plugin_name.contains("cache") {
        SpanCategory::Cache
    } else {
        SpanCategory::Gateway
    }
}

/// Record one plugin's `pre_llm` stage. Shared by `chat` and `chat_stream` so the two
/// paths can't drift into describing the same pipeline differently.
fn record_pre_llm_span(
    ctx: &Ctx,
    plugin_name: &str,
    at: std::time::Instant,
    outcome: &Result<PreOutcome, KgError>,
) {
    let (chip, detail) = match outcome {
        Ok(PreOutcome::ShortCircuit(_)) => (
            Some("hit".to_string()),
            Some("Served here — no upstream call was made.".to_string()),
        ),
        Ok(PreOutcome::Reject(e)) => (Some("rejected".to_string()), Some(e.message.clone())),
        Err(e) => (
            Some("error".to_string()),
            // Plugin errors are non-blocking by design; say so, or the trace looks broken.
            Some(format!("{} (non-blocking — request continued)", e.message)),
        ),
        Ok(PreOutcome::Continue(_)) => (None, None),
    };
    ctx.span_at(
        at,
        at.elapsed(),
        format!("{plugin_name}.pre_llm"),
        plugin_category(plugin_name),
        1,
        chip,
        detail,
    );
}

/// Exponential backoff with ±20% jitter between retry attempts. Base doubles per attempt,
/// capped; jitter de-synchronizes retries from many concurrent requests (thundering herd).
fn backoff_delay(attempt: u32, rng: &mut StdRng) -> std::time::Duration {
    use rand::Rng;
    const INITIAL_MS: u64 = 200;
    const MAX_MS: u64 = 3000;
    let base = INITIAL_MS.saturating_mul(1u64 << attempt.min(5));
    let capped = base.min(MAX_MS);
    let jitter = 0.8 + rng.gen::<f64>() * 0.4;
    std::time::Duration::from_millis((capped as f64 * jitter) as u64)
}

/// Content-capture policy (M10 Phase 2). Off by default; when `enabled`, the engine
/// serializes request/response payloads into the `CallRecord` (truncated to
/// `max_body_bytes`; `0` disables truncation and captures bodies in full).
/// See `docs/12-content-capture-plan.md`.
#[derive(Debug, Clone)]
pub struct ContentCapture {
    pub enabled: bool,
    /// Per-body truncation budget in bytes. `0` means unbounded (capture in full).
    pub max_body_bytes: usize,
    /// Capture the assembled response of streamed chat (tee + accumulate). When false,
    /// streamed requests capture the request body only.
    pub capture_streaming: bool,
}

impl Default for ContentCapture {
    fn default() -> Self {
        Self {
            enabled: false,
            max_body_bytes: 16 * 1024,
            capture_streaming: false,
        }
    }
}

/// Truncate a UTF-8 string to at most `max` bytes (on a char boundary), appending a
/// marker when truncation occurred. Used to bound captured payload size.
/// `max == 0` means unbounded: the string is returned untouched, no marker.
fn truncate_utf8(mut s: String, max: usize) -> String {
    if max == 0 {
        return s;
    }
    if s.len() <= max {
        return s;
    }
    const MARKER: &str = "…[truncated]";
    // When the budget can't even fit the marker, hard-truncate to a char boundary with
    // no marker so the result never exceeds `max`.
    if max <= MARKER.len() {
        let mut cut = max;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        return s;
    }
    // Reserve room for the marker, then back up to a char boundary.
    let mut cut = max - MARKER.len();
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push_str(MARKER);
    s
}

/// Emits the deferred audit record for a streamed chat when content capture accumulates
/// the response. Living inside the returned response-body stream, it fires on `Drop` —
/// whether the stream is fully consumed OR the client disconnects early — so a cancelled
/// stream is still logged (governance + audit), closing the "lost record on disconnect"
/// gap. Response text is accumulated up to `max_bytes`.
/// Deferred audit guard for a streamed response. Emits exactly one `CallRecord` when the
/// stream ends — on normal completion AND on early client disconnect (Drop) — so a stream
/// is always logged once, with the real end status and token usage. Body accumulation is
/// opt-in (`capture_body`); with it off the guard still records status + tokens.
struct StreamCaptureGuard {
    observers: Vec<Arc<dyn RequestObserver>>,
    request_id: RequestId,
    virtual_key: Option<String>,
    started_at: std::time::Instant,
    model_full: String,
    req_body: Option<String>,
    /// Whether to accumulate response text into `acc` (content capture enabled).
    capture_body: bool,
    acc: String,
    /// Reassembles streamed tool-call fragments so a tool-call response is captured too
    /// (not just plain content). Only fed when `capture_body`.
    tool_calls: ToolCallAccumulator,
    max_bytes: usize,
    /// Usage from the stream's final usage-bearing chunk (needs `stream_options.include_usage`).
    prompt_tokens: u32,
    completion_tokens: u32,
    /// The finish reason from the stream (e.g. "stop", "length", "tool_calls"), last-seen wins.
    stop_reason: Option<String>,
    /// Final status for the record — 200 unless the stream errored mid-flight.
    status: u16,
    /// Upstream error detail if the stream errored.
    error_message: Option<String>,
    /// Shared handle to the request's trace spans. The guard emits the audit record
    /// after the borrow of `Ctx` is gone, so it holds the collector itself rather than
    /// a snapshot — spans recorded late (mid-stream) still land on the record.
    spans: Arc<crate::trace::SpanCollector>,
    /// When the first chunk reached the client, so the guard can split the stream into
    /// time-to-first-token and body-transfer time on the trace.
    first_chunk_at: Option<std::time::Instant>,
    emitted: bool,
}

impl StreamCaptureGuard {
    /// Append a response delta, clamped to the remaining byte budget on a char boundary
    /// (so a single large delta can't overshoot `max_bytes`). No-op when body capture is off.
    /// `max_bytes == 0` means unbounded: every delta is accumulated in full, so the whole
    /// streamed completion is held in memory per active stream until the record is emitted.
    fn push_delta(&mut self, text: &str) {
        if !self.capture_body {
            return;
        }
        if self.max_bytes == 0 {
            self.acc.push_str(text);
            return;
        }
        if self.acc.len() >= self.max_bytes {
            return;
        }
        let remaining = self.max_bytes - self.acc.len();
        if text.len() <= remaining {
            self.acc.push_str(text);
        } else {
            let mut cut = remaining;
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            self.acc.push_str(&text[..cut]);
        }
    }

    /// Merge a chunk's streamed tool-call fragments (no-op when body capture is off or the
    /// delta carries no tool calls).
    fn push_tool_calls(&mut self, delta: &Delta) {
        if self.capture_body && !delta.tool_calls.is_empty() {
            self.tool_calls.push(delta);
        }
    }

    /// Record the finish reason seen on a chunk's choice (last non-null wins). OpenAI sends it
    /// on the second-to-last chunk, before the usage-only final chunk.
    fn note_stop_reason(&mut self, choice: &StreamChoice) {
        if let Some(fr) = &choice.finish_reason {
            self.stop_reason = Some(fr.clone());
        }
    }

    /// Record token usage seen on a chunk. Providers emit usage on (or near) the final
    /// chunk, so the last-seen value is authoritative — later chunks overwrite earlier ones.
    fn note_usage(&mut self, usage: &crate::schema::Usage) {
        self.prompt_tokens = usage.prompt_tokens;
        self.completion_tokens = usage.completion_tokens;
    }

    /// Stamp the first chunk's arrival. Called for every chunk; only the first matters.
    fn note_chunk(&mut self) {
        self.first_chunk_at
            .get_or_insert_with(std::time::Instant::now);
    }

    /// Note an error observed while consuming the stream, so the deferred record reflects
    /// the real outcome instead of a blanket 200.
    fn note_error(&mut self, e: &KgError) {
        self.status = e.http_status();
        self.error_message = Some(e.message.clone());
    }
}

impl Drop for StreamCaptureGuard {
    fn drop(&mut self) {
        if self.emitted {
            return;
        }
        self.emitted = true;
        let observers = std::mem::take(&mut self.observers);
        if observers.is_empty() {
            return;
        }
        // `on_response` is async but `drop` is not, so emit on a detached task. Guard the
        // spawn: if the guard is dropped outside a Tokio runtime (e.g. a unit test drops
        // the stream after `block_on` returns, or during runtime teardown), `tokio::spawn`
        // would PANIC — and a panic in `drop` during unwinding aborts the process. When no
        // runtime is present we log and drop the record instead.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("stream capture record dropped: no Tokio runtime in drop context");
            return;
        };
        let request_id = self.request_id;
        let virtual_key = self.virtual_key.take();
        let started_at = self.started_at;
        let spans = self.spans.clone();
        // Split the stream into TTFT (already recorded at dispatch) and body transfer, so
        // "the model thought for 4 s" reads differently from "the body took 4 s".
        if let Some(first) = self.first_chunk_at {
            spans.record_detailed(
                self.started_at,
                first,
                first.elapsed(),
                "stream.body",
                crate::trace::SpanCategory::Network,
                1,
                None,
                Some("First chunk to end of stream.".into()),
            );
        }
        let mut rec = cap_record(
            &self.model_full,
            self.status,
            self.prompt_tokens,
            self.completion_tokens,
        );
        rec.stream = true;
        rec.stop_reason = self.stop_reason.take();
        rec.request_body = self.req_body.take();
        // Only attach a response body when content capture was on; otherwise leave it None
        // (a status/token-only record) rather than an empty string. A tool-call response has
        // no text deltas, so fall back to the assembled tool calls as JSON.
        if self.capture_body {
            let text = std::mem::take(&mut self.acc);
            let body = if !text.is_empty() {
                text
            } else {
                let calls = std::mem::take(&mut self.tool_calls).finish();
                if calls.is_empty() {
                    String::new()
                } else {
                    serde_json::to_string(&calls).unwrap_or_default()
                }
            };
            rec.response_body = Some(truncate_utf8(body, self.max_bytes));
        }
        rec.error_message = self.error_message.take();
        handle.spawn(async move {
            let mut c = Ctx::new();
            c.request_id = request_id;
            c.virtual_key = virtual_key;
            c.started_at = started_at;
            c.spans = spans;
            for o in &observers {
                o.on_response(&c, &rec).await;
            }
        });
    }
}

/// The KGateway engine. Holds the provider registry, plugin chain, request observers,
/// and MCP clients.
pub struct Kgateway {
    registry: Registry,
    llm_plugins: Vec<Arc<dyn LlmPlugin>>,
    observers: Vec<Arc<dyn RequestObserver>>,
    mcp_clients: Vec<Arc<dyn McpClient>>,
    content_capture: ContentCapture,
}

impl Kgateway {
    pub fn new(registry: Registry) -> Self {
        Self {
            registry,
            llm_plugins: Vec::new(),
            observers: Vec::new(),
            mcp_clients: Vec::new(),
            content_capture: ContentCapture::default(),
        }
    }

    /// Enable request/response content capture (M10 Phase 2). Off by default.
    pub fn with_content_capture(mut self, capture: ContentCapture) -> Self {
        self.content_capture = capture;
        self
    }

    /// Serialize a payload to (truncated) JSON when capture is enabled, else `None`.
    /// The `serde_json` work is skipped entirely when disabled (zero hot-path cost).
    fn capture<T: serde::Serialize>(&self, value: &T) -> Option<String> {
        if !self.content_capture.enabled {
            return None;
        }
        serde_json::to_string(value)
            .ok()
            .map(|s| truncate_utf8(s, self.content_capture.max_body_bytes))
    }

    /// Whether streamed responses should be accumulated for capture.
    fn capture_streaming(&self) -> bool {
        self.content_capture.enabled && self.content_capture.capture_streaming
    }

    pub fn with_plugin(mut self, plugin: Arc<dyn LlmPlugin>) -> Self {
        self.llm_plugins.push(plugin);
        self
    }

    /// Register a request observer (governance, audit logging, telemetry). Observers run
    /// on EVERY capability method, unlike `LlmPlugin`s which only run on chat.
    pub fn with_observer(mut self, observer: Arc<dyn RequestObserver>) -> Self {
        self.observers.push(observer);
        self
    }

    /// Run all observers' pre-flight checks. Returns the first rejection.
    async fn observe_check(&self, ctx: &Ctx, model: &str) -> Result<(), KgError> {
        for o in &self.observers {
            let at = std::time::Instant::now();
            let outcome = o.on_request(ctx, model).await;
            // Named for the hook, not for "checking" — the logging observer admits every
            // request and checks nothing, so `logging.check` read as nonsense. Named per
            // observer so a slow governance store is distinguishable from a slow exporter.
            ctx.span_at(
                at,
                at.elapsed(),
                format!("{}.on_request", o.name()),
                SpanCategory::Policy,
                1,
                outcome.as_ref().err().map(|_| "rejected".to_string()),
                outcome.as_ref().err().map(|e| e.message.clone()),
            );
            outcome?;
        }
        Ok(())
    }

    /// Record the outcome with all observers (audit, token accounting). Never fails.
    async fn observe_record(&self, ctx: &Ctx, record: CallRecord) {
        for o in &self.observers {
            o.on_response(ctx, &record).await;
        }
    }

    /// Register an MCP tool server. Its tools are injected into agentic requests and its
    /// tool calls are executed by [`Kgateway::chat_agentic`].
    pub fn with_mcp(mut self, client: Arc<dyn McpClient>) -> Self {
        self.mcp_clients.push(client);
        self
    }

    pub fn has_mcp(&self) -> bool {
        !self.mcp_clients.is_empty()
    }

    /// All tools exposed by the registered MCP clients (control-plane / diagnostics).
    pub async fn list_mcp_tools(&self) -> Vec<crate::schema::Tool> {
        let mut all = Vec::new();
        for client in &self.mcp_clients {
            all.extend(client.list_tools().await.unwrap_or_default());
        }
        all
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Non-streaming chat completion through the full pipeline.
    pub async fn chat(&self, ctx: &mut Ctx, mut req: ChatRequest) -> Result<ChatResponse, KgError> {
        // pre_request: ONCE, committed mutations (routing decisions). Non-blocking errors.
        for p in &self.llm_plugins {
            if let Err(e) = p.pre_request(ctx, &mut req).await {
                tracing::warn!(plugin = p.name(), error = %e, "pre_request error (non-blocking)");
            }
        }

        // Observer pre-flight (governance/auth) — runs BEFORE the cache so a cache hit
        // can't be served to an unauthorized/over-budget key. A rejection is recorded
        // (audit) then returned.
        if let Err(e) = self.observe_check(ctx, &req.model).await {
            let mut rec = chat_record(&req.model, &Err(e.clone()));
            rec.request_body = self.capture(&req);
            self.observe_record(ctx, rec).await;
            return self.run_post(ctx, Err(e)).await;
        }

        // pre_llm: forward order, may short-circuit. Errors are NON-BLOCKING (per
        // docs/04-plugins.md): a plugin returning Err is logged and the pipeline
        // continues with the request unchanged — a buggy plugin must not fail traffic.
        // `pre_llm` consumes the request by value, so we snapshot it before each call
        // to restore on error.
        for p in &self.llm_plugins {
            let snapshot = req.clone();
            let at = std::time::Instant::now();
            let outcome = p.pre_llm(ctx, req).await;
            record_pre_llm_span(ctx, p.name(), at, &outcome);
            match outcome {
                Ok(PreOutcome::Continue(r)) => req = r,
                Ok(PreOutcome::ShortCircuit(resp)) => {
                    // A pre_llm short-circuit means the semantic cache served this.
                    let mut rec = chat_record(&snapshot.model, &Ok(resp.clone()));
                    rec.cache_hit = true;
                    rec.request_body = self.capture(&snapshot);
                    rec.response_body = self.capture(&resp);
                    self.observe_record(ctx, rec).await;
                    return self.run_post(ctx, Ok(resp)).await;
                }
                Ok(PreOutcome::Reject(e)) => {
                    let mut rec = chat_record(&snapshot.model, &Err(e.clone()));
                    rec.request_body = self.capture(&snapshot);
                    self.observe_record(ctx, rec).await;
                    return self.run_post(ctx, Err(e)).await;
                }
                Err(e) => {
                    tracing::warn!(plugin = p.name(), error = %e, "pre_llm error (non-blocking)");
                    req = snapshot;
                }
            }
        }

        let req_body = self.capture(&req);
        let result = self.dispatch_with_fallback(ctx, &req).await;
        let mut rec = chat_record(&req.model, &result);
        rec.request_body = req_body;
        if let Ok(r) = &result {
            rec.response_body = self.capture(r);
        }
        self.observe_record(ctx, rec).await;
        self.run_post(ctx, result).await
    }

    /// Agentic chat: inject the registered MCP tools, then run the LLM → execute
    /// tool-calls → re-prompt loop until the model returns a final answer (no tool
    /// calls) or `max_rounds` is reached. Each LLM turn goes through the full `chat`
    /// pipeline (plugins, failover, isolation). With no MCP clients, this is a single
    /// `chat` call.
    pub async fn chat_agentic(
        &self,
        ctx: &mut Ctx,
        mut req: ChatRequest,
        max_rounds: usize,
    ) -> Result<ChatResponse, KgError> {
        // Inject MCP tools (merged with any tools the caller already supplied).
        for client in &self.mcp_clients {
            match client.list_tools().await {
                Ok(tools) => req.tools.extend(tools),
                Err(e) => tracing::warn!(mcp = client.name(), error = %e, "list_tools failed"),
            }
        }

        let mut last = self.chat(ctx, req.clone()).await?;
        for _ in 0..max_rounds {
            let msg = match last.choices.first() {
                Some(c) => &c.message,
                None => return Ok(last),
            };
            if msg.tool_calls.is_empty() {
                return Ok(last); // final answer
            }

            // Append the assistant's tool-call message, then a tool-result message per call.
            req.messages.push(msg.clone());
            let calls = msg.tool_calls.clone();
            for tc in &calls {
                let tool_at = std::time::Instant::now();
                let outcome = self
                    .execute_tool(&tc.function.name, &tc.function.arguments)
                    .await;
                ctx.span_at(
                    tool_at,
                    tool_at.elapsed(),
                    format!("mcp.{}", tc.function.name),
                    SpanCategory::Tools,
                    1,
                    outcome.as_ref().err().map(|_| "error".to_string()),
                    None,
                );
                let result = outcome.unwrap_or_else(|e| format!("tool error: {e}"));
                req.messages.push(Message {
                    role: Role::Tool,
                    content: Some(result.into()),
                    name: Some(tc.function.name.clone()),
                    tool_calls: vec![],
                    tool_call_id: Some(tc.id.clone()),
                });
            }

            // Each round writes its own audit row. Without a fresh collector every row
            // would carry all previous rounds' spans — quadratic storage, and the
            // dashboard's attempt count would read a 5-round tool loop as 5 failovers.
            ctx.spans = std::sync::Arc::new(crate::trace::SpanCollector::new());
            last = self.chat(ctx, req.clone()).await?;
        }
        // Hit the round cap with tool calls still pending — return the last response.
        tracing::warn!(max_rounds, "agentic loop hit round cap");
        Ok(last)
    }

    /// Route a tool call to the first MCP client that owns it.
    async fn execute_tool(&self, name: &str, arguments: &str) -> Result<String, KgError> {
        for client in &self.mcp_clients {
            if client.has_tool(name).await {
                return client.call_tool(name, arguments).await;
            }
        }
        Err(KgError::internal(format!(
            "no MCP client owns tool: {name}"
        )))
    }

    /// Provider-level failover: try the primary (`req.model`) then each `req.fallbacks`
    /// entry. Move to the next provider only when the error is retryable; a
    /// non-retryable error (e.g. 400) stops immediately. `ctx.attempt` counts provider
    /// attempts. Runs exactly once per top-level request (post-hooks run afterwards).
    async fn dispatch_with_fallback(
        &self,
        ctx: &mut Ctx,
        req: &ChatRequest,
    ) -> Result<ChatResponse, KgError> {
        // Cap the client-supplied fallback chain: an unbounded array pointed at a
        // degraded provider would drive one client request into many real outbound
        // calls against the operator's keys. Excess entries are ignored.
        let fallbacks = &req.fallbacks[..req.fallbacks.len().min(MAX_FALLBACKS)];
        let mut targets: Vec<String> = Vec::with_capacity(1 + fallbacks.len());
        targets.push(req.model.clone());
        for fb in fallbacks {
            targets.push(format!("{}/{}", fb.provider, fb.model));
        }

        let mut rng = StdRng::from_entropy();
        let mut last_err = KgError::internal("no provider attempted");

        for target in targets {
            ctx.attempt += 1;
            let mut attempt_req = req.clone();
            attempt_req.model = target;
            match self.dispatch_one(ctx, &attempt_req, &mut rng).await {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    let retryable = e.is_retryable();
                    last_err = e;
                    if !retryable {
                        break;
                    }
                }
            }
        }
        Err(last_err)
    }

    /// Dispatch to a single provider with key-level retry. Picks eligible keys by
    /// weighted-random selection and tries up to `MAX_KEYS_PER_PROVIDER` distinct keys,
    /// advancing only on retryable errors. Each call holds a per-provider concurrency
    /// permit (isolation).
    async fn dispatch_one(
        &self,
        ctx: &Ctx,
        req: &ChatRequest,
        rng: &mut StdRng,
    ) -> Result<ChatResponse, KgError> {
        let provider_key = ProviderKey::new(req.model_provider());
        let entry = self.registry.get(&provider_key).ok_or_else(|| {
            KgError::new(
                KgErrorKind::BadRequest,
                format!("unknown provider: {provider_key}"),
            )
        })?;

        let mut remaining = keyselect::eligible_keys(&entry.keys, req.model_id());
        if remaining.is_empty() {
            return Err(KgError::new(
                KgErrorKind::Auth,
                format!("no eligible API keys for {provider_key}/{}", req.model_id()),
            ));
        }

        let max_keys = remaining.len().min(MAX_KEYS_PER_PROVIDER);
        let mut last_err = KgError::internal("no key attempted");

        for attempt in 0..max_keys {
            let Some(chosen) = keyselect::weighted_pick(&remaining, rng) else {
                break;
            };
            // Backoff before every attempt after the first — a 429 especially means "slow
            // down", so hammering the next key immediately is counterproductive.
            if attempt > 0 {
                let at = std::time::Instant::now();
                tokio::time::sleep(backoff_delay(attempt as u32, rng)).await;
                ctx.span_at(
                    at,
                    at.elapsed(),
                    "backoff + jitter",
                    SpanCategory::Wait,
                    1,
                    None,
                    Some("Exponential backoff before retrying on the next key.".into()),
                );
            }
            let at = std::time::Instant::now();
            let call = self.call_provider(entry, chosen, ctx, req.clone()).await;
            // One span per attempt, named by provider + key, so a failover chain reads as
            // the sequence of tries it actually was rather than a single opaque total.
            ctx.span_at(
                at,
                at.elapsed(),
                format!("attempt · {provider_key} key={}", chosen.id),
                if call.is_ok() {
                    SpanCategory::Network
                } else {
                    SpanCategory::Failed
                },
                1,
                call.as_ref().err().map(|e| {
                    e.status
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "error".into())
                }),
                call.as_ref().err().map(attempt_failure_detail),
            );
            match call {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    // Rotate to another key on per-key failures (incl. 401/402/403), not just
                    // transient 5xx/429. A single revoked/over-billing key shouldn't abort the
                    // whole provider while sibling keys are valid.
                    let rotatable = e.is_key_rotatable();
                    last_err = e;
                    if !rotatable {
                        return Err(last_err);
                    }
                    // Retry a different key (without replacement).
                    remaining.retain(|k| !std::ptr::eq(*k, chosen));
                }
            }
        }
        Err(last_err)
    }

    /// Acquire the provider's isolation permit for the duration of the call.
    async fn call_provider(
        &self,
        entry: &ProviderEntry,
        key: &ApiKey,
        ctx: &Ctx,
        req: ChatRequest,
    ) -> Result<ChatResponse, KgError> {
        let queued_at = std::time::Instant::now();
        let _permit = entry
            .concurrency
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| KgError::internal("provider concurrency semaphore closed"))?;
        // Only worth a row when the request actually waited — an uncontended permit is
        // sub-microsecond noise that would clutter every trace.
        let queued = queued_at.elapsed();
        if queued >= SEMAPHORE_WAIT_SPAN_FLOOR {
            ctx.span_at(
                queued_at,
                queued,
                "semaphore.acquire",
                SpanCategory::Wait,
                2,
                Some("queued".into()),
                Some("Waited for a per-provider concurrency permit.".into()),
            );
        }
        // No child span for the HTTP call itself: it lands within microseconds of its
        // parent `attempt` span, so the extra row costs a line and says nothing. Nesting
        // is reserved for children that genuinely differ — the semaphore wait above, and
        // the TTFT / stream-body split on the streaming path.
        entry.provider.chat(ctx, key, req).await
    }

    /// Resolve a `provider/model` string to its registered entry, a weighted-selected
    /// key, and the bare model id. Shared by all capability dispatch methods.
    fn resolve<'a>(
        &'a self,
        model: &str,
    ) -> Result<(&'a ProviderEntry, &'a ApiKey, String), KgError> {
        let (provider_name, model_id) = match model.split_once('/') {
            Some((p, m)) => (p.to_string(), m.to_string()),
            None => ("openai".to_string(), model.to_string()),
        };
        let provider_key = ProviderKey::new(&provider_name);
        let entry = self.registry.get(&provider_key).ok_or_else(|| {
            KgError::new(
                KgErrorKind::BadRequest,
                format!("unknown provider: {provider_key}"),
            )
        })?;
        let eligible = keyselect::eligible_keys(&entry.keys, &model_id);
        let mut rng = StdRng::from_entropy();
        let key = keyselect::weighted_pick(&eligible, &mut rng).ok_or_else(|| {
            KgError::new(
                KgErrorKind::Auth,
                format!("no eligible API keys for {provider_key}/{model_id}"),
            )
        })?;
        Ok((entry, key, model_id))
    }

    /// Embeddings request. Runs observers (governance/audit), routes by `provider/model`,
    /// checks the `Embeddings` capability, selects a weighted key, holds an isolation permit.
    pub async fn embed(
        &self,
        ctx: &Ctx,
        mut req: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, KgError> {
        let model_full = req.model.clone();
        if let Err(e) = self.observe_check(ctx, &model_full).await {
            self.observe_record(ctx, cap_record(&model_full, e.http_status(), 0, 0))
                .await;
            return Err(e);
        }
        let (entry, key, model_id) = self.resolve(&req.model)?;
        let cap = entry.provider.as_embeddings().ok_or_else(|| {
            KgError::unsupported(format!("embeddings for {}", entry.provider.key()))
        })?;
        req.model = model_id;
        let _permit = self.permit(entry).await?;
        // Capture matrix: embeddings log the input text only — the response is a vector,
        // which is huge and opaque, so it is never captured.
        let req_body = self.capture(&req);
        let result = cap.embed(ctx, key, req).await;
        let (status, pt, ct) = match &result {
            Ok(r) => (200, r.usage.prompt_tokens, r.usage.completion_tokens),
            Err(e) => (e.http_status(), 0, 0),
        };
        let mut rec = cap_record(&model_full, status, pt, ct);
        rec.request_body = req_body;
        self.observe_record(ctx, rec).await;
        result
    }

    /// Image generation. Routes to the provider's `Images` capability (observed).
    pub async fn image_generate(
        &self,
        ctx: &Ctx,
        mut req: ImageGenerationRequest,
    ) -> Result<ImageResponse, KgError> {
        let model_full = req.model.clone();
        if let Err(e) = self.observe_check(ctx, &model_full).await {
            self.observe_record(ctx, cap_record(&model_full, e.http_status(), 0, 0))
                .await;
            return Err(e);
        }
        let (entry, key, model_id) = self.resolve(&req.model)?;
        let cap = entry
            .provider
            .as_images()
            .ok_or_else(|| KgError::unsupported(format!("images for {}", entry.provider.key())))?;
        req.model = model_id;
        let _permit = self.permit(entry).await?;
        // Capture matrix: images log the prompt/params only — the response can be a
        // base64 blob, which is never captured.
        let req_body = self.capture(&req);
        let result = cap.image_generate(ctx, key, req).await;
        let status = result
            .as_ref()
            .map(|_| 200)
            .unwrap_or_else(|e| e.http_status());
        let mut rec = cap_record(&model_full, status, 0, 0);
        rec.request_body = req_body;
        self.observe_record(ctx, rec).await;
        result
    }

    /// Text-to-speech. Routes to the provider's `Audio` capability (observed).
    pub async fn speech(
        &self,
        ctx: &Ctx,
        mut req: SpeechRequest,
    ) -> Result<SpeechResponse, KgError> {
        let model_full = req.model.clone();
        if let Err(e) = self.observe_check(ctx, &model_full).await {
            self.observe_record(ctx, cap_record(&model_full, e.http_status(), 0, 0))
                .await;
            return Err(e);
        }
        let (entry, key, model_id) = self.resolve(&req.model)?;
        let cap = entry
            .provider
            .as_audio()
            .ok_or_else(|| KgError::unsupported(format!("audio for {}", entry.provider.key())))?;
        req.model = model_id;
        let _permit = self.permit(entry).await?;
        // Capture matrix: speech logs the input text/params only — the response is binary
        // audio, never captured.
        let req_body = self.capture(&req);
        let result = cap.speech(ctx, key, req).await;
        let status = result
            .as_ref()
            .map(|_| 200)
            .unwrap_or_else(|e| e.http_status());
        let mut rec = cap_record(&model_full, status, 0, 0);
        rec.request_body = req_body;
        self.observe_record(ctx, rec).await;
        result
    }

    /// Speech-to-text transcription. Routes to the provider's `Audio` capability (observed).
    pub async fn transcribe(
        &self,
        ctx: &Ctx,
        mut req: TranscriptionRequest,
    ) -> Result<TranscriptionResponse, KgError> {
        let model_full = req.model.clone();
        if let Err(e) = self.observe_check(ctx, &model_full).await {
            self.observe_record(ctx, cap_record(&model_full, e.http_status(), 0, 0))
                .await;
            return Err(e);
        }
        let (entry, key, model_id) = self.resolve(&req.model)?;
        let cap = entry
            .provider
            .as_audio()
            .ok_or_else(|| KgError::unsupported(format!("audio for {}", entry.provider.key())))?;
        req.model = model_id;
        let _permit = self.permit(entry).await?;
        let result = cap.transcribe(ctx, key, req).await;
        let status = result
            .as_ref()
            .map(|_| 200)
            .unwrap_or_else(|e| e.http_status());
        // Capture matrix: transcription logs the response text only — the request is
        // binary audio, never captured.
        let mut rec = cap_record(&model_full, status, 0, 0);
        if let Ok(r) = &result {
            rec.response_body = self.capture(r);
        }
        self.observe_record(ctx, rec).await;
        result
    }

    /// Document reranking. Routes to the provider's `Rerank` capability (observed).
    pub async fn rerank(
        &self,
        ctx: &Ctx,
        mut req: RerankRequest,
    ) -> Result<RerankResponse, KgError> {
        let model_full = req.model.clone();
        if let Err(e) = self.observe_check(ctx, &model_full).await {
            self.observe_record(ctx, cap_record(&model_full, e.http_status(), 0, 0))
                .await;
            return Err(e);
        }
        let (entry, key, model_id) = self.resolve(&req.model)?;
        let cap = entry
            .provider
            .as_rerank()
            .ok_or_else(|| KgError::unsupported(format!("rerank for {}", entry.provider.key())))?;
        req.model = model_id;
        let _permit = self.permit(entry).await?;
        // Capture matrix: rerank logs both the query+documents and the ranked scores
        // (both compact).
        let req_body = self.capture(&req);
        let result = cap.rerank(ctx, key, req).await;
        let status = result
            .as_ref()
            .map(|_| 200)
            .unwrap_or_else(|e| e.http_status());
        let mut rec = cap_record(&model_full, status, 0, 0);
        rec.request_body = req_body;
        if let Ok(r) = &result {
            rec.response_body = self.capture(r);
        }
        self.observe_record(ctx, rec).await;
        result
    }

    /// Acquire a provider's isolation permit.
    async fn permit(
        &self,
        entry: &ProviderEntry,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, KgError> {
        entry
            .concurrency
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| KgError::internal("provider concurrency semaphore closed"))
    }

    /// Streaming chat completion. Runs `pre_request` + `pre_llm` (so governance/cache
    /// short-circuits apply to streams too — closing the `"stream": true` bypass),
    /// selects a weighted key, and holds a per-provider concurrency permit for the
    /// lifetime of the stream (isolation).
    ///
    /// A `pre_llm` short-circuit `ChatResponse` is converted into a single-chunk stream.
    ///
    /// Resilience: the stream is opened with the SAME failover + key-rotation logic as
    /// `chat` — the first chunk is peeked before any bytes reach the client, so an error
    /// surfaced at stream-open (or on the first chunk) rotates keys / fails over to the
    /// next provider transparently. Once the first chunk is delivered, the provider is
    /// committed. An idle (inter-chunk) timeout aborts a hung upstream and releases its
    /// concurrency permit. Token usage from the final chunk is recorded on stream end.
    ///
    /// Not yet applied to streams: `post_llm` (needs full stream accumulation).
    /// Stream-chunk hooks land at the HTTP layer. Tracked in docs/02-roadmap.md.
    pub async fn chat_stream(
        &self,
        ctx: &mut Ctx,
        mut req: ChatRequest,
    ) -> Result<ChunkStream, KgError> {
        for p in &self.llm_plugins {
            if let Err(e) = p.pre_request(ctx, &mut req).await {
                tracing::warn!(plugin = p.name(), error = %e, "pre_request error (non-blocking)");
            }
        }

        // Observer pre-flight (governance/auth) — streams are enforced too.
        if let Err(e) = self.observe_check(ctx, &req.model).await {
            let mut rec = chat_record(&req.model, &Err(e.clone()));
            rec.stream = true;
            rec.request_body = self.capture(&req);
            self.observe_record(ctx, rec).await;
            return Err(e);
        }

        // pre_llm: same non-blocking semantics as `chat`. A short-circuit response is
        // emitted as a one-shot stream so streaming clients can't bypass the hook.
        for p in &self.llm_plugins {
            let snapshot = req.clone();
            let at = std::time::Instant::now();
            let outcome = p.pre_llm(ctx, req).await;
            record_pre_llm_span(ctx, p.name(), at, &outcome);
            match outcome {
                Ok(PreOutcome::Continue(r)) => req = r,
                Ok(PreOutcome::ShortCircuit(resp)) => {
                    // A streaming cache hit is still audited + governed (mirrors `chat`),
                    // so `stream: true` can't bypass on_response accounting.
                    let mut rec = chat_record(&snapshot.model, &Ok(resp.clone()));
                    rec.stream = true;
                    rec.cache_hit = true;
                    rec.request_body = self.capture(&snapshot);
                    rec.response_body = self.capture(&resp);
                    self.observe_record(ctx, rec).await;
                    return Ok(response_to_stream(resp));
                }
                Ok(PreOutcome::Reject(e)) => {
                    let mut rec = chat_record(&snapshot.model, &Err(e.clone()));
                    rec.stream = true;
                    rec.request_body = self.capture(&snapshot);
                    self.observe_record(ctx, rec).await;
                    return Err(e);
                }
                Err(e) => {
                    tracing::warn!(plugin = p.name(), error = %e, "pre_llm error (non-blocking)");
                    req = snapshot;
                }
            }
        }

        // Ask the provider to emit token usage on the stream. OpenAI-compatible providers only
        // send a final usage chunk when `stream_options.include_usage` is set; without this the
        // gateway records 0 tokens (and 0 cost) for every stream. Native adapters that build
        // their own body ignore the field, so injecting it is safe. A client-supplied value is
        // respected.
        if req.stream_options.is_none() {
            req.stream_options = Some(serde_json::json!({ "include_usage": true }));
        }

        // Capture the request body before `req` is consumed by the opener (if content
        // capture is enabled; `None` otherwise).
        let req_body = self.capture(&req);

        // Open the stream with provider failover + key rotation + first-chunk peek. On
        // success we hold the winning provider's permit and know the actual target used.
        let (permit, inner, model_full) = self.open_stream_with_failover(ctx, &req).await?;

        // A single deferred audit guard covers BOTH the capture and non-capture paths: it
        // emits exactly one record on stream end (completion OR early disconnect), with the
        // real end status and token usage. Body text is accumulated only when capture is on.
        let guard = StreamCaptureGuard {
            observers: self.observers.clone(),
            request_id: ctx.request_id,
            virtual_key: ctx.virtual_key.clone(),
            started_at: ctx.started_at,
            model_full,
            req_body,
            capture_body: self.capture_streaming(),
            acc: String::new(),
            tool_calls: ToolCallAccumulator::new(),
            max_bytes: self.content_capture.max_body_bytes,
            prompt_tokens: 0,
            completion_tokens: 0,
            stop_reason: None,
            status: 200,
            error_message: None,
            spans: ctx.spans.clone(),
            first_chunk_at: None,
            emitted: false,
        };

        let guarded = async_stream::stream! {
            let _permit = permit; // held for the stream's lifetime; released on Drop
            let mut guard = guard; // moved in; drops (and emits) when the generator ends
            futures::pin_mut!(inner);
            loop {
                // Idle-timeout each poll so a hung upstream can't pin the permit forever.
                match tokio::time::timeout(STREAM_IDLE_TIMEOUT, inner.next()).await {
                    Ok(Some(item)) => {
                        match &item {
                            Ok(chunk) => {
                                guard.note_chunk();
                                if let Some(u) = &chunk.usage {
                                    guard.note_usage(u);
                                }
                                if let Some(ch) = chunk.choices.first() {
                                    if let Some(c) = ch.delta.content.as_deref() {
                                        guard.push_delta(c);
                                    }
                                    guard.push_tool_calls(&ch.delta);
                                    guard.note_stop_reason(ch);
                                }
                            }
                            Err(e) => guard.note_error(e),
                        }
                        yield item;
                    }
                    Ok(None) => break, // stream finished normally
                    Err(_elapsed) => {
                        let e = KgError::new(
                            KgErrorKind::Network,
                            "stream idle timeout: upstream stopped sending",
                        );
                        guard.note_error(&e);
                        yield Err(e);
                        break;
                    }
                }
            }
            drop(guard);
        };
        Ok(guarded.boxed())
    }

    /// Open a streamed chat with provider-level failover, mirroring `dispatch_with_fallback`.
    /// Tries the primary (`req.model`) then each `req.fallbacks` entry, advancing only on a
    /// retryable error. Returns the held concurrency permit, the (first-chunk-prepended)
    /// stream, and the `provider/model` target that actually won (for the audit record).
    async fn open_stream_with_failover(
        &self,
        ctx: &mut Ctx,
        req: &ChatRequest,
    ) -> Result<(tokio::sync::OwnedSemaphorePermit, ChunkStream, String), KgError> {
        let fallbacks = &req.fallbacks[..req.fallbacks.len().min(MAX_FALLBACKS)];
        let mut targets: Vec<String> = Vec::with_capacity(1 + fallbacks.len());
        targets.push(req.model.clone());
        for fb in fallbacks {
            targets.push(format!("{}/{}", fb.provider, fb.model));
        }

        let mut rng = StdRng::from_entropy();
        let mut last_err = KgError::internal("no provider attempted");

        for target in targets {
            ctx.attempt += 1;
            let mut attempt_req = req.clone();
            attempt_req.model = target.clone();
            match self.open_stream_one(ctx, &attempt_req, &mut rng).await {
                Ok((permit, stream)) => return Ok((permit, stream, target)),
                Err(e) => {
                    let retryable = e.is_retryable();
                    last_err = e;
                    if !retryable {
                        break;
                    }
                }
            }
        }
        Err(last_err)
    }

    /// Open a stream against a single provider with key-level retry, mirroring `dispatch_one`.
    /// Acquires the isolation permit, opens the stream, then PEEKS the first chunk: an error
    /// at open OR on the first chunk rotates to another key (per-key failures) before any
    /// bytes reach the client. On success the peeked chunk is prepended so the caller sees a
    /// complete stream. The permit is released on every failed attempt and held on success.
    async fn open_stream_one(
        &self,
        ctx: &Ctx,
        req: &ChatRequest,
        rng: &mut StdRng,
    ) -> Result<(tokio::sync::OwnedSemaphorePermit, ChunkStream), KgError> {
        let provider_key = ProviderKey::new(req.model_provider());
        let entry = self.registry.get(&provider_key).ok_or_else(|| {
            KgError::new(
                KgErrorKind::BadRequest,
                format!("unknown provider: {provider_key}"),
            )
        })?;

        let mut remaining = keyselect::eligible_keys(&entry.keys, req.model_id());
        if remaining.is_empty() {
            return Err(KgError::new(
                KgErrorKind::Auth,
                format!("no eligible API keys for {provider_key}/{}", req.model_id()),
            ));
        }

        let max_keys = remaining.len().min(MAX_KEYS_PER_PROVIDER);
        let mut last_err = KgError::internal("no key attempted");

        for attempt in 0..max_keys {
            let Some(chosen) = keyselect::weighted_pick(&remaining, rng) else {
                break;
            };
            if attempt > 0 {
                let at = std::time::Instant::now();
                tokio::time::sleep(backoff_delay(attempt as u32, rng)).await;
                ctx.span_at(
                    at,
                    at.elapsed(),
                    "backoff + jitter",
                    SpanCategory::Wait,
                    1,
                    None,
                    Some("Exponential backoff before retrying on the next key.".into()),
                );
            }
            // Acquire the permit before opening; hold it on success, drop on failure so a
            // failed attempt never leaks a permit.
            let queued_at = std::time::Instant::now();
            let permit = match entry.concurrency.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => return Err(KgError::internal("provider concurrency semaphore closed")),
            };
            let queued = queued_at.elapsed();
            if queued >= SEMAPHORE_WAIT_SPAN_FLOOR {
                ctx.span_at(
                    queued_at,
                    queued,
                    "semaphore.acquire",
                    SpanCategory::Wait,
                    2,
                    Some("queued".into()),
                    Some("Waited for a per-provider concurrency permit.".into()),
                );
            }
            // Stamped after the permit: queue time is its own `semaphore.acquire` span, and
            // folding it into TTFT would report the gateway's own backpressure as the
            // model being slow to answer.
            let attempt_at = std::time::Instant::now();
            let opened = entry.provider.chat_stream(ctx, chosen, req.clone()).await;
            let mut stream = match opened {
                Ok(s) => s,
                Err(e) => {
                    drop(permit);
                    ctx.span_at(
                        attempt_at,
                        attempt_at.elapsed(),
                        format!("attempt · {provider_key} key={}", chosen.id),
                        SpanCategory::Failed,
                        1,
                        Some(
                            e.status
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| "error".into()),
                        ),
                        Some(attempt_failure_detail(&e)),
                    );
                    let rotatable = e.is_key_rotatable();
                    last_err = e;
                    if !rotatable {
                        return Err(last_err);
                    }
                    remaining.retain(|k| !std::ptr::eq(*k, chosen));
                    continue;
                }
            };
            // Peek the first chunk (bounded) — some providers return 200 then emit an error
            // event; catching it here lets us fail over before the client sees anything.
            match tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next()).await {
                Ok(Some(Ok(first))) => {
                    // Time to first token: the number that decides whether a stream *feels*
                    // fast, and the one a single total-latency figure hides completely.
                    ctx.span_at(
                        attempt_at,
                        attempt_at.elapsed(),
                        format!("stream.ttft · {provider_key} key={}", chosen.id),
                        SpanCategory::Network,
                        1,
                        Some("first token".into()),
                        Some("Open + time to first chunk. Failover happens before this point, so the client never sees a retry.".into()),
                    );
                    let rest = futures::stream::once(async move { Ok(first) })
                        .chain(stream)
                        .boxed();
                    return Ok((permit, rest));
                }
                Ok(None) => {
                    // Empty stream: a degenerate success. Return it rather than retrying, so
                    // one client request never double-dispatches against the operator's keys.
                    return Ok((permit, futures::stream::empty().boxed()));
                }
                Ok(Some(Err(e))) => {
                    drop(permit);
                    // A 200 followed by an in-band error event — the exact case the
                    // first-chunk peek exists to catch. Without a span the failed try is
                    // invisible and its cost reads as the next attempt being slow.
                    ctx.span_at(
                        attempt_at,
                        attempt_at.elapsed(),
                        format!("attempt · {provider_key} key={}", chosen.id),
                        SpanCategory::Failed,
                        1,
                        Some(
                            e.status
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| "stream error".into()),
                        ),
                        Some(attempt_failure_detail(&e)),
                    );
                    let rotatable = e.is_key_rotatable();
                    last_err = e;
                    if !rotatable {
                        return Err(last_err);
                    }
                    remaining.retain(|k| !std::ptr::eq(*k, chosen));
                }
                Err(_elapsed) => {
                    drop(permit);
                    ctx.span_at(
                        attempt_at,
                        attempt_at.elapsed(),
                        format!("attempt · {provider_key} key={}", chosen.id),
                        SpanCategory::Failed,
                        1,
                        Some("timeout".into()),
                        Some(
                            "No first chunk before the idle timeout. Retryable — failing over."
                                .into(),
                        ),
                    );
                    // No first chunk in time — treat as a retryable network error.
                    last_err = KgError::new(
                        KgErrorKind::Network,
                        "stream idle timeout awaiting first chunk",
                    );
                    remaining.retain(|k| !std::ptr::eq(*k, chosen));
                }
            }
        }
        Err(last_err)
    }

    /// post_llm: REVERSE (LIFO) order. Errors are NON-BLOCKING: a plugin that returns
    /// Err is logged and the pipeline keeps the PRIOR result — a failing telemetry/
    /// logging plugin must not turn a good upstream response into a 500. Plugins that
    /// intend to *replace* the result (e.g. a content filter) must return `Ok(...)`
    /// with the transformed response; `Err` is reserved for plugin-internal failure.
    /// `post_llm` consumes the result by value, so we snapshot it before each call.
    async fn run_post(
        &self,
        ctx: &Ctx,
        mut result: Result<ChatResponse, KgError>,
    ) -> Result<ChatResponse, KgError> {
        for p in self.llm_plugins.iter().rev() {
            let snapshot = result.clone();
            match p.post_llm(ctx, result).await {
                Ok(resp) => result = Ok(resp),
                Err(e) => {
                    tracing::warn!(plugin = p.name(), error = %e, "post_llm error (non-blocking)");
                    result = snapshot;
                }
            }
        }
        result
    }
}

/// Split a `provider/model` string into (provider, bare model). Defaults provider to
/// "openai" when no prefix is present.
fn split_model(model_full: &str) -> (String, String) {
    match model_full.split_once('/') {
        Some((p, m)) => (p.to_string(), m.to_string()),
        None => ("openai".to_string(), model_full.to_string()),
    }
}

/// Build a [`CallRecord`] for a chat outcome (tokens + stop_reason from the response;
/// error detail for the audit log). The caller sets `stream`/`cache_hit`.
fn chat_record(model_full: &str, result: &Result<ChatResponse, KgError>) -> CallRecord {
    let (provider, model) = split_model(model_full);
    match result {
        Ok(r) => CallRecord {
            provider,
            model,
            status: 200,
            prompt_tokens: r.usage.prompt_tokens,
            completion_tokens: r.usage.completion_tokens,
            stop_reason: r.choices.first().and_then(|c| c.finish_reason.clone()),
            ..Default::default()
        },
        Err(e) => CallRecord {
            provider: e.provider.clone().unwrap_or(provider),
            model,
            status: e.http_status(),
            error_message: Some(e.message.clone()),
            ..Default::default()
        },
    }
}

/// Build a [`CallRecord`] for a capability (non-chat) outcome with explicit status/tokens.
fn cap_record(
    model_full: &str,
    status: u16,
    prompt_tokens: u32,
    completion_tokens: u32,
) -> CallRecord {
    let (provider, model) = split_model(model_full);
    CallRecord {
        provider,
        model,
        status,
        prompt_tokens,
        completion_tokens,
        ..Default::default()
    }
}

/// Convert a (short-circuited) `ChatResponse` into a one-shot stream: a single chunk
/// carrying the response content, then end-of-stream. Lets `pre_llm` short-circuits
/// (e.g. a cache hit) satisfy streaming requests.
fn response_to_stream(resp: ChatResponse) -> ChunkStream {
    let chunk = StreamChunk {
        id: resp.id,
        object: "chat.completion.chunk".to_string(),
        model: resp.model,
        choices: resp
            .choices
            .into_iter()
            .map(|c| StreamChoice {
                index: c.index,
                delta: Delta {
                    role: Some(c.message.role),
                    content: c.message.content.as_ref().and_then(|mc| mc.to_text()),
                    ..Default::default()
                },
                finish_reason: c.finish_reason.or(Some("stop".to_string())),
            })
            .collect(),
        usage: Some(resp.usage),
    };
    futures::stream::once(async move { Ok(chunk) }).boxed()
}

impl ChatRequest {
    /// Extract the provider portion of the model string.
    ///
    /// Convention: `"provider/model"` (e.g. `"openai/gpt-4o"`). If no slash is present,
    /// defaults to `"openai"`. M2 may add explicit `provider` routing overrides.
    pub fn model_provider(&self) -> String {
        match self.model.split_once('/') {
            Some((provider, _)) => provider.to_string(),
            None => "openai".to_string(),
        }
    }

    /// The model id with any `provider/` prefix stripped, for sending upstream.
    pub fn model_id(&self) -> &str {
        match self.model.split_once('/') {
            Some((_, model)) => model,
            None => &self.model,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::Plugin;
    use crate::provider::{ApiKey, Embeddings, Provider};
    use crate::schema::{
        ChatResponse, Choice, FunctionCallDelta, Message, Role, ToolCallDelta, Usage,
    };
    use async_trait::async_trait;

    #[test]
    fn truncate_utf8_leaves_short_strings_untouched() {
        assert_eq!(truncate_utf8("hello".to_string(), 16), "hello");
    }

    #[test]
    fn truncate_utf8_appends_marker_when_over_limit() {
        let out = truncate_utf8("x".repeat(100), 20);
        assert!(out.len() <= 20, "len was {}", out.len());
        assert!(out.ends_with("…[truncated]"));
    }

    #[test]
    fn truncate_utf8_respects_char_boundaries() {
        // Multibyte chars: cutting must not split a codepoint (would panic / be invalid).
        let s = "é".repeat(50); // each 'é' is 2 bytes
        let out = truncate_utf8(s, 25);
        assert!(out.len() <= 25);
        assert!(out.ends_with("…[truncated]"));
        // Still valid UTF-8 (guaranteed by String, but assert it's non-empty prefix).
        assert!(out.starts_with('é'));
    }

    #[test]
    fn capture_disabled_yields_none() {
        let eng = Kgateway::new(Registry::new());
        assert!(eng.capture(&serde_json::json!({"a": 1})).is_none());
    }

    #[test]
    fn capture_enabled_serializes_and_truncates() {
        let eng = Kgateway::new(Registry::new()).with_content_capture(ContentCapture {
            enabled: true,
            max_body_bytes: 20,
            capture_streaming: false,
        });
        let out = eng
            .capture(&serde_json::json!({"k": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}))
            .unwrap();
        assert!(out.len() <= 20, "len was {}", out.len());
        assert!(out.ends_with("…[truncated]"));
    }

    #[test]
    fn truncate_utf8_tiny_budget_never_exceeds_max() {
        // Budget smaller than the marker: hard-truncate, no marker, still within budget.
        let out = truncate_utf8("abcdefghij".to_string(), 4);
        assert!(out.len() <= 4, "len was {}", out.len());
    }

    #[test]
    fn truncate_utf8_zero_budget_is_unbounded() {
        // 0 is the "no cap" sentinel: the string comes back whole, no marker.
        let s = "x".repeat(64 * 1024);
        let out = truncate_utf8(s.clone(), 0);
        assert_eq!(out, s);
        assert!(!out.ends_with("…[truncated]"));
    }

    #[test]
    fn capture_enabled_zero_budget_keeps_full_body() {
        let eng = Kgateway::new(Registry::new()).with_content_capture(ContentCapture {
            enabled: true,
            max_body_bytes: 0,
            capture_streaming: false,
        });
        // Larger than the 16 KiB default to prove no cap applies anywhere.
        let big = "a".repeat(32 * 1024);
        let out = eng.capture(&serde_json::json!({ "k": big })).unwrap();
        assert!(out.len() > 32 * 1024, "len was {}", out.len());
        assert!(out.contains(&big));
        assert!(!out.ends_with("…[truncated]"));
    }

    // A provider that always returns a fixed successful response.
    struct OkProvider;

    #[async_trait]
    impl Provider for OkProvider {
        fn key(&self) -> ProviderKey {
            ProviderKey::new("openai")
        }
        async fn chat(
            &self,
            _ctx: &Ctx,
            _key: &ApiKey,
            _req: ChatRequest,
        ) -> Result<ChatResponse, KgError> {
            Ok(ChatResponse {
                id: "ok".into(),
                object: "chat.completion".into(),
                model: "m".into(),
                choices: vec![Choice {
                    index: 0,
                    message: Message {
                        role: Role::Assistant,
                        content: Some("real answer".into()),
                        name: None,
                        tool_calls: vec![],
                        tool_call_id: None,
                    },
                    finish_reason: Some("stop".into()),
                }],
                usage: Usage::default(),
            })
        }
        async fn chat_stream(
            &self,
            _ctx: &Ctx,
            _key: &ApiKey,
            _req: ChatRequest,
        ) -> Result<crate::provider::ChunkStream, KgError> {
            Err(KgError::internal("not used in test"))
        }
    }

    // A plugin whose pre_llm and post_llm both error.
    struct ExplodingPlugin;
    impl Plugin for ExplodingPlugin {
        fn name(&self) -> &str {
            "exploding"
        }
    }
    #[async_trait]
    impl LlmPlugin for ExplodingPlugin {
        async fn pre_llm(&self, _ctx: &Ctx, _req: ChatRequest) -> Result<PreOutcome, KgError> {
            Err(KgError::internal("pre boom"))
        }
        async fn post_llm(
            &self,
            _ctx: &Ctx,
            _resp: Result<ChatResponse, KgError>,
        ) -> Result<ChatResponse, KgError> {
            Err(KgError::internal("post boom"))
        }
    }

    fn registry_with_ok_provider() -> Registry {
        let mut r = Registry::new();
        r.register(
            Arc::new(OkProvider),
            vec![ApiKey {
                id: "k".into(),
                value: "v".into(),
                weight: 1,
                models: vec![],
            }],
        );
        r
    }

    fn req() -> ChatRequest {
        ChatRequest {
            model: "openai/m".into(),
            messages: vec![Message::user("hi")],
            ..Default::default()
        }
    }

    // Regression for the HIGH review findings: a plugin whose pre_llm AND post_llm error
    // must NOT fail the request — errors are non-blocking, the real response survives.
    #[tokio::test]
    async fn hook_errors_are_non_blocking() {
        let engine =
            Kgateway::new(registry_with_ok_provider()).with_plugin(Arc::new(ExplodingPlugin));
        let mut ctx = Ctx::new();
        let resp = engine
            .chat(&mut ctx, req())
            .await
            .expect("request should still succeed");
        assert_eq!(resp.choices[0].message.text_content(), Some("real answer"));
    }

    // ---- M2 failover / key-selection tests ----

    use crate::schema::Fallback;
    use std::sync::atomic::{AtomicUsize, Ordering};

    enum Behavior {
        AlwaysOk(&'static str),
        AlwaysErr(u16),
        /// Fail the first `fail` calls with `status` (retryably) then return Ok.
        FailThenOk {
            fail: usize,
            status: u16,
            content: &'static str,
        },
    }

    struct FakeProvider {
        name: String,
        calls: Arc<AtomicUsize>,
        behavior: Behavior,
    }

    impl FakeProvider {
        fn new(name: &str, behavior: Behavior) -> (Arc<Self>, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            let p = Arc::new(Self {
                name: name.into(),
                calls: calls.clone(),
                behavior,
            });
            (p, calls)
        }
    }

    fn ok_response(id: &str, content: &str) -> ChatResponse {
        ChatResponse {
            id: id.into(),
            object: "chat.completion".into(),
            model: "m".into(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: Some(content.into()),
                    name: None,
                    tool_calls: vec![],
                    tool_call_id: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage::default(),
        }
    }

    fn key(id: &str) -> ApiKey {
        ApiKey {
            id: id.into(),
            value: "v".into(),
            weight: 1,
            models: vec![],
        }
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn key(&self) -> ProviderKey {
            ProviderKey::new(&self.name)
        }
        async fn chat(
            &self,
            _ctx: &Ctx,
            _key: &ApiKey,
            _req: ChatRequest,
        ) -> Result<ChatResponse, KgError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.behavior {
                Behavior::AlwaysOk(c) => Ok(ok_response(&self.name, c)),
                Behavior::AlwaysErr(status) => {
                    Err(KgError::provider("fail", *status).with_provider(&self.name))
                }
                Behavior::FailThenOk {
                    fail,
                    status,
                    content,
                } => {
                    if n < *fail {
                        Err(KgError::provider("transient", *status).with_provider(&self.name))
                    } else {
                        Ok(ok_response(&self.name, content))
                    }
                }
            }
        }
        async fn chat_stream(
            &self,
            _ctx: &Ctx,
            _key: &ApiKey,
            _req: ChatRequest,
        ) -> Result<crate::provider::ChunkStream, KgError> {
            Err(KgError::internal("no stream in fake"))
        }
    }

    #[tokio::test]
    async fn fallback_on_retryable_error() {
        let mut registry = Registry::new();
        let (primary, primary_calls) = FakeProvider::new("openai", Behavior::AlwaysErr(503));
        let (fb, _) = FakeProvider::new("anthropic", Behavior::AlwaysOk("from fallback"));
        registry.register(primary, vec![key("k")]);
        registry.register(fb, vec![key("k")]);
        let engine = Kgateway::new(registry);

        let mut r = req(); // model "openai/m"
        r.fallbacks = vec![Fallback {
            provider: "anthropic".into(),
            model: "m".into(),
        }];

        let mut ctx = Ctx::new();
        let resp = engine
            .chat(&mut ctx, r)
            .await
            .expect("fallback should succeed");
        assert_eq!(
            resp.choices[0].message.text_content(),
            Some("from fallback")
        );
        assert_eq!(ctx.attempt, 2, "primary + one fallback = 2 attempts");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn trace_records_each_failover_attempt_with_its_status() {
        // The trace's whole purpose: show that the 503 attempt happened and cost time,
        // instead of reporting one opaque total for a request that looks like a success.
        let mut registry = Registry::new();
        let (primary, _) = FakeProvider::new("openai", Behavior::AlwaysErr(503));
        let (fb, _) = FakeProvider::new("anthropic", Behavior::AlwaysOk("from fallback"));
        registry.register(primary, vec![key("k1")]);
        registry.register(fb, vec![key("k2")]);
        let engine = Kgateway::new(registry);

        let mut r = req();
        r.fallbacks = vec![Fallback {
            provider: "anthropic".into(),
            model: "m".into(),
        }];

        let mut ctx = Ctx::new();
        engine.chat(&mut ctx, r).await.expect("fallback succeeds");

        let spans = ctx.spans.snapshot();
        let attempts: Vec<_> = spans
            .iter()
            .filter(|s| s.name.starts_with("attempt ·"))
            .collect();
        assert_eq!(attempts.len(), 2, "one span per attempt: {spans:#?}");

        assert_eq!(attempts[0].category, SpanCategory::Failed);
        assert_eq!(attempts[0].outcome.as_deref(), Some("503"));
        assert!(attempts[0].name.contains("openai"));
        assert!(
            attempts[0].name.contains("key=k1"),
            "key id identifies which credential failed"
        );

        assert_eq!(attempts[1].category, SpanCategory::Network);
        assert_eq!(
            attempts[1].outcome, None,
            "the successful attempt carries no error chip"
        );
        assert!(attempts[1].name.contains("anthropic"));

        // Timeline order: the failed attempt must precede the fallback that replaced it.
        assert!(attempts[0].start_us <= attempts[1].start_us);
    }

    #[tokio::test]
    async fn trace_never_carries_upstream_error_text() {
        // Spans are persisted unconditionally, unlike captured bodies (opt-in twice and
        // redacted). If an upstream echoes the user's prompt back in its error — providers
        // do this when rejecting content — copying that message into a span would smuggle
        // request content into the audit row with content capture switched off.
        const LEAKY: &str = "rejected: content \"my SSN is 123-45-6789\" violates policy";

        let mut registry = Registry::new();
        struct LeakyProvider;
        #[async_trait]
        impl Provider for LeakyProvider {
            fn key(&self) -> ProviderKey {
                ProviderKey::new("openai")
            }
            async fn chat(
                &self,
                _ctx: &Ctx,
                _key: &ApiKey,
                _req: ChatRequest,
            ) -> Result<ChatResponse, KgError> {
                Err(KgError::provider(LEAKY, 400))
            }
            async fn chat_stream(
                &self,
                _ctx: &Ctx,
                _key: &ApiKey,
                _req: ChatRequest,
            ) -> Result<ChunkStream, KgError> {
                Err(KgError::provider(LEAKY, 400))
            }
        }
        registry.register(Arc::new(LeakyProvider), vec![key("k")]);
        let engine = Kgateway::new(registry);

        let mut ctx = Ctx::new();
        let _ = engine.chat(&mut ctx, req()).await;

        let spans = ctx.spans.snapshot();
        assert!(!spans.is_empty(), "the failed attempt is traced");
        for sp in &spans {
            let blob = format!(
                "{} {} {}",
                sp.name,
                sp.detail.as_deref().unwrap_or(""),
                sp.outcome.as_deref().unwrap_or("")
            );
            assert!(
                !blob.contains("123-45-6789") && !blob.contains("violates policy"),
                "upstream error text leaked into a span: {sp:?}"
            );
        }
        // The status still reaches the operator as a chip, and our own wording explains it.
        let attempt = spans
            .iter()
            .find(|s| s.name.starts_with("attempt ·"))
            .unwrap();
        assert_eq!(attempt.outcome.as_deref(), Some("400"));
        let detail = attempt.detail.as_deref().unwrap();
        assert!(
            detail.contains("provider returned an error") && detail.contains("Not retryable"),
            "detail should be our own words about the failure: {detail}"
        );
    }

    #[tokio::test]
    async fn trace_survives_into_a_streamed_request_s_audit_record() {
        // Regression: the deferred stream guard rebuilds a `Ctx` to emit the audit record
        // after the borrow is gone. It once built a FRESH collector, so every streamed
        // request logged an empty trace while unary requests traced fine.
        use crate::observer::CallRecord;
        use std::sync::Mutex;

        #[derive(Default)]
        struct SpanCapturingObserver {
            seen: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl RequestObserver for SpanCapturingObserver {
            fn name(&self) -> &str {
                "span-capture"
            }
            async fn on_response(&self, ctx: &Ctx, _rec: &CallRecord) {
                *self.seen.lock().unwrap() =
                    ctx.spans.snapshot().into_iter().map(|s| s.name).collect();
            }
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        registry.register(Arc::new(StreamProvider), vec![key("k")]);
        let engine = Kgateway::new(registry)
            .with_observer(Arc::new(SpanCapturingObserver { seen: seen.clone() }));

        let mut ctx = Ctx::new();
        let stream = engine
            .chat_stream(&mut ctx, req())
            .await
            .expect("stream opens");
        // Drain, then drop, so the deferred guard fires.
        let _: Vec<_> = stream.collect().await;
        // The guard emits on a detached task; yield until it lands.
        for _ in 0..50 {
            if !seen.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }

        let names = seen.lock().unwrap().clone();
        assert!(
            names.iter().any(|n| n.starts_with("stream.ttft")),
            "the streamed audit record must carry the trace, incl. time-to-first-token: {names:?}"
        );
    }

    #[tokio::test]
    async fn trace_marks_a_cache_short_circuit_as_a_hit() {
        // A cache hit should read as "served here, no upstream call" — the fast-path trace.
        struct CachePlugin;
        #[async_trait]
        impl Plugin for CachePlugin {
            fn name(&self) -> &str {
                "semantic_cache"
            }
        }
        #[async_trait]
        impl LlmPlugin for CachePlugin {
            async fn pre_llm(&self, _ctx: &Ctx, _req: ChatRequest) -> Result<PreOutcome, KgError> {
                Ok(PreOutcome::ShortCircuit(ok_response(
                    "cached",
                    "from cache",
                )))
            }
        }

        let engine = Kgateway::new(registry_with_ok_provider()).with_plugin(Arc::new(CachePlugin));
        let mut ctx = Ctx::new();
        engine.chat(&mut ctx, req()).await.expect("cache serves it");

        let spans = ctx.spans.snapshot();
        let hit = spans
            .iter()
            .find(|s| s.name == "semantic_cache.pre_llm")
            .expect("cache span recorded");
        assert_eq!(
            hit.category,
            SpanCategory::Cache,
            "cache gets its own colour band"
        );
        assert_eq!(hit.outcome.as_deref(), Some("hit"));
        assert!(
            !spans.iter().any(|s| s.name.starts_with("attempt ·")),
            "a short-circuit must not record an upstream attempt: {spans:#?}"
        );
    }

    #[tokio::test]
    async fn non_retryable_error_stops_failover() {
        let mut registry = Registry::new();
        let (primary, _) = FakeProvider::new("openai", Behavior::AlwaysErr(400));
        let (fb, fb_calls) = FakeProvider::new("anthropic", Behavior::AlwaysOk("nope"));
        registry.register(primary, vec![key("k")]);
        registry.register(fb, vec![key("k")]);
        let engine = Kgateway::new(registry);

        let mut r = req();
        r.fallbacks = vec![Fallback {
            provider: "anthropic".into(),
            model: "m".into(),
        }];

        let mut ctx = Ctx::new();
        let err = engine
            .chat(&mut ctx, r)
            .await
            .expect_err("400 is non-retryable");
        assert_eq!(err.status, Some(400));
        assert_eq!(
            fb_calls.load(Ordering::SeqCst),
            0,
            "fallback must NOT be called on a non-retryable error"
        );
        assert_eq!(ctx.attempt, 1, "only the primary was attempted");
    }

    #[tokio::test]
    async fn key_level_retry_within_provider() {
        let mut registry = Registry::new();
        let (p, calls) = FakeProvider::new(
            "openai",
            Behavior::FailThenOk {
                fail: 1,
                status: 503,
                content: "second key ok",
            },
        );
        // Two keys → the first (retryable) failure retries a second key in the same attempt.
        registry.register(p, vec![key("k1"), key("k2")]);
        let engine = Kgateway::new(registry);

        let mut ctx = Ctx::new();
        let resp = engine
            .chat(&mut ctx, req())
            .await
            .expect("second key succeeds");
        assert_eq!(
            resp.choices[0].message.text_content(),
            Some("second key ok")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2, "should try a second key");
        assert_eq!(
            ctx.attempt, 1,
            "key retry stays within one provider attempt"
        );
    }

    #[tokio::test]
    async fn key_rotation_on_auth_failure() {
        // A 401 on the first key must rotate to the next key, not abort the
        // whole provider — previously 401 was non-retryable and failed immediately.
        let mut registry = Registry::new();
        let (p, calls) = FakeProvider::new(
            "openai",
            Behavior::FailThenOk {
                fail: 1,
                status: 401,
                content: "second key ok",
            },
        );
        registry.register(p, vec![key("k1"), key("k2")]);
        let engine = Kgateway::new(registry);

        let mut ctx = Ctx::new();
        let resp = engine
            .chat(&mut ctx, req())
            .await
            .expect("rotates past the revoked key");
        assert_eq!(
            resp.choices[0].message.text_content(),
            Some("second key ok")
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "401 on key #1 should rotate to key #2"
        );
    }

    // ---- M3: chat_stream pre_llm short-circuit + embeddings ----

    struct ShortCircuitPlugin;
    impl Plugin for ShortCircuitPlugin {
        fn name(&self) -> &str {
            "short-circuit"
        }
    }
    #[async_trait]
    impl LlmPlugin for ShortCircuitPlugin {
        async fn pre_llm(&self, _ctx: &Ctx, _req: ChatRequest) -> Result<PreOutcome, KgError> {
            Ok(PreOutcome::ShortCircuit(ok_response(
                "cache",
                "cached answer",
            )))
        }
    }

    #[tokio::test]
    async fn stream_short_circuit_yields_single_chunk() {
        // A pre_llm short-circuit on a streaming request must still be served (governance
        // / cache can't be bypassed via `stream: true`), as a one-shot stream.
        let engine =
            Kgateway::new(registry_with_ok_provider()).with_plugin(Arc::new(ShortCircuitPlugin));
        let mut ctx = Ctx::new();
        let mut r = req();
        r.stream = Some(true);
        let stream = engine
            .chat_stream(&mut ctx, r)
            .await
            .expect("short-circuit stream");
        let chunks: Vec<_> = stream.collect().await;
        assert_eq!(chunks.len(), 1, "short-circuit yields exactly one chunk");
        let chunk = chunks.into_iter().next().unwrap().unwrap();
        assert_eq!(
            chunk.choices[0].delta.content.as_deref(),
            Some("cached answer")
        );
    }

    // ---- M10 Phase 2: streaming content capture (deferred Drop-guard record) ----

    /// Observer that records every `CallRecord` it sees, for asserting audit emission.
    struct RecordingObserver {
        records: Arc<std::sync::Mutex<Vec<CallRecord>>>,
    }
    #[async_trait]
    impl RequestObserver for RecordingObserver {
        fn name(&self) -> &str {
            "recording"
        }
        async fn on_response(&self, _ctx: &Ctx, record: &CallRecord) {
            self.records.lock().unwrap().push(record.clone());
        }
    }

    /// Provider that streams two content chunks ("STREAM_" then "PART2").
    struct StreamProvider;
    #[async_trait]
    impl Provider for StreamProvider {
        fn key(&self) -> ProviderKey {
            ProviderKey::new("openai")
        }
        async fn chat(
            &self,
            _ctx: &Ctx,
            _key: &ApiKey,
            _req: ChatRequest,
        ) -> Result<ChatResponse, KgError> {
            Err(KgError::internal("chat not used"))
        }
        async fn chat_stream(
            &self,
            _ctx: &Ctx,
            _key: &ApiKey,
            _req: ChatRequest,
        ) -> Result<ChunkStream, KgError> {
            fn chunk(content: &str) -> StreamChunk {
                StreamChunk {
                    id: "c".into(),
                    object: "chat.completion.chunk".into(),
                    model: "m".into(),
                    choices: vec![StreamChoice {
                        index: 0,
                        delta: Delta {
                            role: None,
                            content: Some(content.into()),
                            ..Default::default()
                        },
                        finish_reason: None,
                    }],
                    usage: None,
                }
            }
            let items = vec![Ok(chunk("STREAM_")), Ok(chunk("PART2"))];
            Ok(futures::stream::iter(items).boxed())
        }
    }

    fn capture_engine(
        records: Arc<std::sync::Mutex<Vec<CallRecord>>>,
        max_body_bytes: usize,
    ) -> Kgateway {
        let mut registry = Registry::new();
        registry.register(Arc::new(StreamProvider), vec![key("k")]);
        Kgateway::new(registry)
            .with_observer(Arc::new(RecordingObserver { records }))
            .with_content_capture(ContentCapture {
                enabled: true,
                max_body_bytes,
                capture_streaming: true,
            })
    }

    // Wait (bounded) for the detached emit task spawned by the Drop guard.
    async fn wait_for_record(records: &Arc<std::sync::Mutex<Vec<CallRecord>>>) {
        for _ in 0..100 {
            if !records.lock().unwrap().is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn stream_capture_records_full_response_on_completion() {
        let records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = capture_engine(records.clone(), 1024);
        let mut ctx = Ctx::new();
        let mut r = req();
        r.stream = Some(true);

        let stream = engine.chat_stream(&mut ctx, r).await.expect("stream");
        let collected: Vec<_> = stream.collect().await;
        assert_eq!(collected.len(), 2, "both chunks delivered to the client");

        wait_for_record(&records).await;
        let recs = records.lock().unwrap();
        assert_eq!(recs.len(), 1, "exactly one deferred record");
        assert!(recs[0].stream);
        assert_eq!(recs[0].status, 200);
        assert_eq!(recs[0].response_body.as_deref(), Some("STREAM_PART2"));
    }

    #[tokio::test]
    async fn stream_capture_zero_budget_accumulates_in_full() {
        // max_body_bytes: 0 = unbounded — every delta accumulates, none are clamped.
        let records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = capture_engine(records.clone(), 0);
        let mut ctx = Ctx::new();
        let mut r = req();
        r.stream = Some(true);

        let stream = engine.chat_stream(&mut ctx, r).await.expect("stream");
        let _: Vec<_> = stream.collect().await;

        wait_for_record(&records).await;
        let recs = records.lock().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].response_body.as_deref(), Some("STREAM_PART2"));
    }

    #[tokio::test]
    async fn stream_capture_records_partial_on_early_drop() {
        let records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = capture_engine(records.clone(), 1024);
        let mut ctx = Ctx::new();
        let mut r = req();
        r.stream = Some(true);

        let mut stream = engine.chat_stream(&mut ctx, r).await.expect("stream");
        // Consume only the first chunk, then drop the stream (client disconnect).
        let _first = stream.next().await.expect("first chunk");
        drop(stream);

        wait_for_record(&records).await;
        let recs = records.lock().unwrap();
        assert_eq!(recs.len(), 1, "aborted stream still logged exactly once");
        assert!(recs[0].stream);
        // Only the first chunk's content was accumulated before the disconnect.
        assert_eq!(recs[0].response_body.as_deref(), Some("STREAM_"));
    }

    // ---- Streaming resilience: open failover, key rotation, usage capture, idle timeout ----

    fn content_chunk(content: &str) -> StreamChunk {
        StreamChunk {
            id: "c".into(),
            object: "chat.completion.chunk".into(),
            model: "m".into(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    content: Some(content.into()),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    fn usage_chunk(prompt: u32, completion: u32) -> StreamChunk {
        StreamChunk {
            id: "c".into(),
            object: "chat.completion.chunk".into(),
            model: "m".into(),
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: prompt + completion,
            }),
        }
    }

    enum StreamBehavior {
        /// Stream one content chunk, then an optional usage chunk.
        Ok {
            content: &'static str,
            usage: Option<(u32, u32)>,
        },
        /// `chat_stream` returns Err (fails to open) with `status`.
        OpenErr(u16),
        /// Opens OK, but the first (and only) chunk is `Err(status)`.
        FirstChunkErr(u16),
        /// Fail to OPEN the first `fail` calls with `status`, then stream `content`.
        OpenFailThenOk {
            fail: usize,
            status: u16,
            content: &'static str,
        },
        /// Opens OK, yields one content chunk, then hangs forever (for idle-timeout tests).
        HangAfterFirst(&'static str),
    }

    struct StreamFake {
        name: String,
        calls: Arc<AtomicUsize>,
        behavior: StreamBehavior,
    }

    impl StreamFake {
        fn new(name: &str, behavior: StreamBehavior) -> (Arc<Self>, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            let p = Arc::new(Self {
                name: name.into(),
                calls: calls.clone(),
                behavior,
            });
            (p, calls)
        }
    }

    #[async_trait]
    impl Provider for StreamFake {
        fn key(&self) -> ProviderKey {
            ProviderKey::new(&self.name)
        }
        async fn chat(
            &self,
            _ctx: &Ctx,
            _key: &ApiKey,
            _req: ChatRequest,
        ) -> Result<ChatResponse, KgError> {
            Err(KgError::internal("chat not used in stream test"))
        }
        async fn chat_stream(
            &self,
            _ctx: &Ctx,
            _key: &ApiKey,
            _req: ChatRequest,
        ) -> Result<crate::provider::ChunkStream, KgError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.behavior {
                StreamBehavior::Ok { content, usage } => {
                    let mut items = vec![Ok(content_chunk(content))];
                    if let Some((p, c)) = usage {
                        items.push(Ok(usage_chunk(*p, *c)));
                    }
                    Ok(futures::stream::iter(items).boxed())
                }
                StreamBehavior::OpenErr(status) => {
                    Err(KgError::provider("open fail", *status).with_provider(&self.name))
                }
                StreamBehavior::FirstChunkErr(status) => {
                    let err =
                        KgError::provider("first-chunk fail", *status).with_provider(&self.name);
                    Ok(futures::stream::once(async move { Err(err) }).boxed())
                }
                StreamBehavior::OpenFailThenOk {
                    fail,
                    status,
                    content,
                } => {
                    if n < *fail {
                        Err(KgError::provider("transient open", *status).with_provider(&self.name))
                    } else {
                        Ok(futures::stream::iter(vec![Ok(content_chunk(content))]).boxed())
                    }
                }
                StreamBehavior::HangAfterFirst(content) => {
                    // Copy the 'static str out of the `&self` borrow so the stream is 'static.
                    let content: &'static str = content;
                    let first = futures::stream::once(async move { Ok(content_chunk(content)) });
                    Ok(first.chain(futures::stream::pending()).boxed())
                }
            }
        }
    }

    fn stream_req() -> ChatRequest {
        let mut r = req();
        r.stream = Some(true);
        r
    }

    // Collect just the content deltas from a stream's chunks.
    async fn collect_content(stream: ChunkStream) -> String {
        let chunks: Vec<_> = stream.collect().await;
        let mut out = String::new();
        for chunk in chunks.into_iter().flatten() {
            if let Some(c) = chunk
                .choices
                .first()
                .and_then(|ch| ch.delta.content.as_deref())
            {
                out.push_str(c);
            }
        }
        out
    }

    #[tokio::test]
    async fn stream_open_error_fails_over_to_next_provider() {
        let mut registry = Registry::new();
        let (primary, primary_calls) = StreamFake::new("openai", StreamBehavior::OpenErr(503));
        let (fb, _) = StreamFake::new(
            "anthropic",
            StreamBehavior::Ok {
                content: "from fallback",
                usage: None,
            },
        );
        registry.register(primary, vec![key("k")]);
        registry.register(fb, vec![key("k")]);
        let engine = Kgateway::new(registry);

        let mut r = stream_req();
        r.fallbacks = vec![Fallback {
            provider: "anthropic".into(),
            model: "m".into(),
        }];
        let mut ctx = Ctx::new();
        let stream = engine
            .chat_stream(&mut ctx, r)
            .await
            .expect("failover opens");
        assert_eq!(collect_content(stream).await, "from fallback");
        assert_eq!(ctx.attempt, 2, "primary + one fallback");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stream_first_chunk_error_fails_over() {
        // The primary opens 200 but errors on the first chunk — caught by the peek before
        // any bytes reach the client, so we transparently fail over.
        let mut registry = Registry::new();
        let (primary, primary_calls) =
            StreamFake::new("openai", StreamBehavior::FirstChunkErr(503));
        let (fb, _) = StreamFake::new(
            "anthropic",
            StreamBehavior::Ok {
                content: "recovered",
                usage: None,
            },
        );
        registry.register(primary, vec![key("k")]);
        registry.register(fb, vec![key("k")]);
        let engine = Kgateway::new(registry);

        let mut r = stream_req();
        r.fallbacks = vec![Fallback {
            provider: "anthropic".into(),
            model: "m".into(),
        }];
        let mut ctx = Ctx::new();
        let stream = engine
            .chat_stream(&mut ctx, r)
            .await
            .expect("failover opens");
        assert_eq!(collect_content(stream).await, "recovered");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stream_open_error_rotates_key() {
        // A per-key 401 at stream-open rotates to the next key within the same provider.
        let mut registry = Registry::new();
        let (p, calls) = StreamFake::new(
            "openai",
            StreamBehavior::OpenFailThenOk {
                fail: 1,
                status: 401,
                content: "second key ok",
            },
        );
        registry.register(p, vec![key("k1"), key("k2")]);
        let engine = Kgateway::new(registry);

        let mut ctx = Ctx::new();
        let stream = engine
            .chat_stream(&mut ctx, stream_req())
            .await
            .expect("rotates to a good key");
        assert_eq!(collect_content(stream).await, "second key ok");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "tried a second key");
        assert_eq!(
            ctx.attempt, 1,
            "key rotation stays within one provider attempt"
        );
    }

    #[tokio::test]
    async fn stream_non_retryable_open_error_does_not_fail_over() {
        let mut registry = Registry::new();
        let (primary, _) = StreamFake::new("openai", StreamBehavior::OpenErr(400));
        let (fb, fb_calls) = StreamFake::new(
            "anthropic",
            StreamBehavior::Ok {
                content: "unreached",
                usage: None,
            },
        );
        registry.register(primary, vec![key("k")]);
        registry.register(fb, vec![key("k")]);
        let engine = Kgateway::new(registry);

        let mut r = stream_req();
        r.fallbacks = vec![Fallback {
            provider: "anthropic".into(),
            model: "m".into(),
        }];
        let mut ctx = Ctx::new();
        // `ChunkStream` isn't `Debug`, so match rather than `expect_err`.
        let err = match engine.chat_stream(&mut ctx, r).await {
            Ok(_) => panic!("400 must not open a stream"),
            Err(e) => e,
        };
        assert_eq!(err.status, Some(400));
        assert_eq!(fb_calls.load(Ordering::SeqCst), 0, "no failover on 400");
        assert_eq!(ctx.attempt, 1);
    }

    #[tokio::test]
    async fn stream_usage_recorded_on_completion() {
        // Token usage from the final chunk is accounted on stream end — even with content
        // capture OFF (the deferred guard still records status + tokens).
        let records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        let (p, _) = StreamFake::new(
            "openai",
            StreamBehavior::Ok {
                content: "hello",
                usage: Some((5, 7)),
            },
        );
        registry.register(p, vec![key("k")]);
        let engine = Kgateway::new(registry).with_observer(Arc::new(RecordingObserver {
            records: records.clone(),
        }));

        let mut ctx = Ctx::new();
        let stream = engine
            .chat_stream(&mut ctx, stream_req())
            .await
            .expect("stream opens");
        assert_eq!(collect_content(stream).await, "hello");

        wait_for_record(&records).await;
        let recs = records.lock().unwrap();
        assert_eq!(recs.len(), 1, "exactly one deferred record");
        assert!(recs[0].stream);
        assert_eq!(recs[0].status, 200);
        assert_eq!(recs[0].prompt_tokens, 5);
        assert_eq!(recs[0].completion_tokens, 7);
    }

    #[tokio::test(start_paused = true)]
    async fn stream_idle_timeout_aborts_and_records_error() {
        // The provider yields one chunk then hangs; the inter-chunk idle timeout aborts the
        // stream, surfaces a terminal error chunk, and records the failure. `start_paused`
        // auto-advances virtual time so the 60s timeout fires instantly.
        let records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        let (p, _) = StreamFake::new("openai", StreamBehavior::HangAfterFirst("partial"));
        registry.register(p, vec![key("k")]);
        let engine = Kgateway::new(registry).with_observer(Arc::new(RecordingObserver {
            records: records.clone(),
        }));

        let mut ctx = Ctx::new();
        let stream = engine
            .chat_stream(&mut ctx, stream_req())
            .await
            .expect("stream opens with a first chunk");
        let chunks: Vec<_> = stream.collect().await;
        // First chunk delivered, then a terminal error from the idle timeout.
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].is_ok());
        assert!(chunks[1].is_err());

        wait_for_record(&records).await;
        let recs = records.lock().unwrap();
        assert_eq!(recs.len(), 1);
        assert_ne!(recs[0].status, 200, "recorded as an error, not success");
        assert!(recs[0]
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("idle timeout"));
    }

    /// A tool-call delta chunk in the OpenAI streaming shape.
    fn tool_call_chunk(
        index: u32,
        id: Option<&str>,
        name: Option<&str>,
        args: &str,
    ) -> StreamChunk {
        StreamChunk {
            id: "c".into(),
            object: "chat.completion.chunk".into(),
            model: "m".into(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    tool_calls: vec![ToolCallDelta {
                        index,
                        id: id.map(Into::into),
                        kind: name.map(|_| "function".to_string()),
                        function: Some(FunctionCallDelta {
                            name: name.map(Into::into),
                            arguments: Some(args.into()),
                        }),
                    }],
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    struct ToolCallStreamProvider;
    #[async_trait]
    impl Provider for ToolCallStreamProvider {
        fn key(&self) -> ProviderKey {
            ProviderKey::new("openai")
        }
        async fn chat(
            &self,
            _ctx: &Ctx,
            _key: &ApiKey,
            _req: ChatRequest,
        ) -> Result<ChatResponse, KgError> {
            Err(KgError::internal("chat not used"))
        }
        async fn chat_stream(
            &self,
            _ctx: &Ctx,
            _key: &ApiKey,
            _req: ChatRequest,
        ) -> Result<crate::provider::ChunkStream, KgError> {
            // id + name first, then arguments streamed across two chunks.
            let items = vec![
                Ok(tool_call_chunk(0, Some("call_1"), Some("get_weather"), "")),
                Ok(tool_call_chunk(0, None, None, "{\"loc")),
                Ok(tool_call_chunk(0, None, None, "ation\":\"SF\"}")),
            ];
            Ok(futures::stream::iter(items).boxed())
        }
    }

    #[tokio::test]
    async fn stream_capture_records_assembled_tool_call() {
        // A streamed tool-call response has no text deltas; the capture guard must reassemble
        // the fragments and record the tool call as the response body.
        let records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        registry.register(Arc::new(ToolCallStreamProvider), vec![key("k")]);
        let engine = Kgateway::new(registry)
            .with_observer(Arc::new(RecordingObserver {
                records: records.clone(),
            }))
            .with_content_capture(ContentCapture {
                enabled: true,
                max_body_bytes: 1024,
                capture_streaming: true,
            });

        let mut ctx = Ctx::new();
        let stream = engine
            .chat_stream(&mut ctx, stream_req())
            .await
            .expect("stream");
        let _: Vec<_> = stream.collect().await;

        wait_for_record(&records).await;
        let recs = records.lock().unwrap();
        assert_eq!(recs.len(), 1);
        let body = recs[0].response_body.as_deref().unwrap_or_default();
        assert!(body.contains("get_weather"), "captured body: {body}");
        assert!(
            body.contains(r#"{\"location\":\"SF\"}"#) || body.contains(r#"{"location":"SF"}"#),
            "assembled arguments should be present: {body}"
        );
    }

    /// Streams a content chunk with `finish_reason: "stop"`, then a usage chunk ONLY if the
    /// request asked for usage — mirroring how OpenAI-compatible providers behave, so the test
    /// proves the gateway injected `stream_options.include_usage`.
    struct UsageAwareStreamProvider;
    #[async_trait]
    impl Provider for UsageAwareStreamProvider {
        fn key(&self) -> ProviderKey {
            ProviderKey::new("openai")
        }
        async fn chat(
            &self,
            _ctx: &Ctx,
            _key: &ApiKey,
            _req: ChatRequest,
        ) -> Result<ChatResponse, KgError> {
            Err(KgError::internal("chat not used"))
        }
        async fn chat_stream(
            &self,
            _ctx: &Ctx,
            _key: &ApiKey,
            req: ChatRequest,
        ) -> Result<crate::provider::ChunkStream, KgError> {
            let wants_usage = req
                .stream_options
                .as_ref()
                .and_then(|v| v.get("include_usage"))
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            let mut first = content_chunk("hi");
            first.choices[0].finish_reason = Some("stop".into());
            let mut items = vec![Ok(first)];
            if wants_usage {
                items.push(Ok(usage_chunk(3, 4)));
            }
            Ok(futures::stream::iter(items).boxed())
        }
    }

    #[tokio::test]
    async fn stream_injects_include_usage_and_records_usage_and_stop_reason() {
        // The client sets NO stream_options; the gateway must inject `include_usage` so the
        // provider emits a usage chunk, and the record must carry tokens + stop_reason.
        let records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        registry.register(Arc::new(UsageAwareStreamProvider), vec![key("k")]);
        let engine = Kgateway::new(registry).with_observer(Arc::new(RecordingObserver {
            records: records.clone(),
        }));

        let mut ctx = Ctx::new();
        let stream = engine
            .chat_stream(&mut ctx, stream_req())
            .await
            .expect("stream");
        let _: Vec<_> = stream.collect().await;

        wait_for_record(&records).await;
        let recs = records.lock().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(
            recs[0].prompt_tokens, 3,
            "usage recorded → include_usage was injected"
        );
        assert_eq!(recs[0].completion_tokens, 4);
        assert_eq!(recs[0].stop_reason.as_deref(), Some("stop"));
    }

    struct EmbProvider;
    #[async_trait]
    impl Provider for EmbProvider {
        fn key(&self) -> ProviderKey {
            ProviderKey::new("openai")
        }
        async fn chat(
            &self,
            _ctx: &Ctx,
            _key: &ApiKey,
            _req: ChatRequest,
        ) -> Result<ChatResponse, KgError> {
            Err(KgError::internal("chat not used"))
        }
        async fn chat_stream(
            &self,
            _ctx: &Ctx,
            _key: &ApiKey,
            _req: ChatRequest,
        ) -> Result<ChunkStream, KgError> {
            Err(KgError::internal("stream not used"))
        }
        fn as_embeddings(&self) -> Option<&dyn Embeddings> {
            Some(self)
        }
    }
    #[async_trait]
    impl Embeddings for EmbProvider {
        async fn embed(
            &self,
            _ctx: &Ctx,
            _key: &ApiKey,
            req: EmbeddingRequest,
        ) -> Result<EmbeddingResponse, KgError> {
            Ok(EmbeddingResponse {
                embeddings: vec![vec![0.1, 0.2, 0.3]; req.input.len()],
                model: req.model,
                usage: Usage::default(),
            })
        }
    }

    #[tokio::test]
    async fn embed_dispatches_to_capability_and_strips_prefix() {
        let mut registry = Registry::new();
        registry.register(Arc::new(EmbProvider), vec![key("k")]);
        let engine = Kgateway::new(registry);
        let ctx = Ctx::new();
        let resp = engine
            .embed(
                &ctx,
                EmbeddingRequest {
                    model: "openai/text-embedding-3-small".into(),
                    input: vec!["hello".into(), "world".into()],
                },
            )
            .await
            .expect("embeddings should succeed");
        assert_eq!(resp.embeddings.len(), 2);
        assert_eq!(
            resp.model, "text-embedding-3-small",
            "provider prefix stripped"
        );
    }

    #[tokio::test]
    async fn embed_unsupported_when_provider_lacks_capability() {
        // OkProvider does not implement Embeddings → clean Unsupported before dispatch.
        let engine = Kgateway::new(registry_with_ok_provider());
        let ctx = Ctx::new();
        let err = engine
            .embed(
                &ctx,
                EmbeddingRequest {
                    model: "openai/whatever".into(),
                    input: vec!["x".into()],
                },
            )
            .await
            .expect_err("provider lacks embeddings capability");
        assert_eq!(err.kind, KgErrorKind::Unsupported);
    }

    // ---- M6: agentic MCP tool loop ----

    use crate::mcp::StaticMcpClient;
    use crate::schema::{FunctionCall, ToolCall};

    // Provider that requests a tool call on the first turn, then returns a final answer
    // that echoes whether the tool result was fed back into the conversation.
    struct ToolLoopProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Provider for ToolLoopProvider {
        fn key(&self) -> ProviderKey {
            ProviderKey::new("openai")
        }
        async fn chat(
            &self,
            _ctx: &Ctx,
            _key: &ApiKey,
            req: ChatRequest,
        ) -> Result<ChatResponse, KgError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // First turn: tools must have been injected, and we request one.
                assert!(
                    req.tools.iter().any(|t| t.function.name == "get_weather"),
                    "MCP tools should be injected into the request"
                );
                Ok(ChatResponse {
                    id: "1".into(),
                    object: "chat.completion".into(),
                    model: "m".into(),
                    choices: vec![Choice {
                        index: 0,
                        message: Message {
                            role: Role::Assistant,
                            content: None,
                            name: None,
                            tool_calls: vec![ToolCall {
                                id: "call_1".into(),
                                kind: "function".into(),
                                function: FunctionCall {
                                    name: "get_weather".into(),
                                    arguments: "{\"city\":\"Paris\"}".into(),
                                },
                            }],
                            tool_call_id: None,
                        },
                        finish_reason: Some("tool_calls".into()),
                    }],
                    usage: Usage::default(),
                })
            } else {
                // Second turn: the tool result should be in the message history.
                let fed_back = req.messages.iter().any(|m| {
                    matches!(m.role, Role::Tool) && m.text_content() == Some("sunny in Paris")
                });
                let content = if fed_back {
                    "the weather is sunny in Paris"
                } else {
                    "MISSING TOOL RESULT"
                };
                Ok(ok_response("final", content))
            }
        }
        async fn chat_stream(
            &self,
            _ctx: &Ctx,
            _key: &ApiKey,
            _req: ChatRequest,
        ) -> Result<ChunkStream, KgError> {
            Err(KgError::internal("no stream"))
        }
    }

    fn weather_mcp() -> StaticMcpClient {
        StaticMcpClient::new("weather").with_tool(
            "get_weather",
            "Get the weather for a city",
            serde_json::json!({"type": "object", "properties": {"city": {"type": "string"}}}),
            Arc::new(|_args| Ok("sunny in Paris".to_string())),
        )
    }

    #[tokio::test]
    async fn agentic_loop_executes_tool_and_feeds_result_back() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = Registry::new();
        registry.register(
            Arc::new(ToolLoopProvider {
                calls: calls.clone(),
            }),
            vec![key("k")],
        );
        let engine = Kgateway::new(registry).with_mcp(Arc::new(weather_mcp()));

        let mut ctx = Ctx::new();
        let resp = engine
            .chat_agentic(&mut ctx, req(), 8)
            .await
            .expect("agentic run succeeds");
        assert_eq!(
            resp.choices[0].message.text_content(),
            Some("the weather is sunny in Paris")
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "two LLM turns (tool + final)"
        );
    }

    #[tokio::test]
    async fn agentic_loop_returns_immediately_without_tool_calls() {
        // OkProvider never emits tool calls → single turn.
        let engine = Kgateway::new(registry_with_ok_provider()).with_mcp(Arc::new(weather_mcp()));
        let mut ctx = Ctx::new();
        let resp = engine.chat_agentic(&mut ctx, req(), 8).await.unwrap();
        assert_eq!(resp.choices[0].message.text_content(), Some("real answer"));
    }
}
