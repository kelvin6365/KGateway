//! Replicate provider — chat over the **predictions** API.
//!
//! Replicate is not an OpenAI-wire vendor. A call creates a *prediction* resource
//! that runs asynchronously; the caller then either waits for it or polls it:
//!
//! ```text
//! POST /v1/models/{owner}/{name}/predictions   (or /v1/predictions for a version id)
//!   → { id, status: "starting", urls: { get, stream, cancel } }
//! GET  urls.get     → poll until status is succeeded/failed/canceled
//! GET  urls.stream  → SSE token stream (used by chat_stream)
//! ```
//!
//! Two model-id shapes select the route (`model_route`):
//! - a bare 64-hex string is a **version id** → `/v1/predictions` with `{version, input}`
//! - `owner/name` is a **model slug** → `/v1/models/{owner}/{name}/predictions`
//!
//! Non-streaming calls send `Prefer: wait`, which makes Replicate hold the
//! connection until the prediction settles. That is not guaranteed, so the
//! connector falls back to polling under a **bounded budget** (`MAX_POLL`) that
//! sits below both the 120s provider timeout and the gateway's 120s
//! `TimeoutLayer`. Exhausting it returns a retryable error rather than hanging.
//!
//! **Verification status: mock-only.**

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use kgateway_core::context::Ctx;
use kgateway_core::error::{KgError, KgErrorKind};
use kgateway_core::provider::{ApiKey, ChunkStream, Provider, ProviderKey};
use kgateway_core::schema::{
    ChatRequest, ChatResponse, Choice, Delta, Message, MessageContent, Role, StreamChoice,
    StreamChunk, Usage,
};
use serde::Deserialize;

const DEFAULT_BASE_URL: &str = "https://api.replicate.com";

/// Upper bound on how long a non-streaming call will poll before giving up.
/// Must stay under `http::REQUEST_TIMEOUT` (120s) and the axum `TimeoutLayer`.
const MAX_POLL: Duration = Duration::from_secs(90);

/// Gap between polls. Replicate's own clients use ~2s.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub struct ReplicateProvider {
    key: ProviderKey,
    base_url: String,
    client: reqwest::Client,
}

impl ReplicateProvider {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self::with_identity("replicate", base_url)
    }

    pub fn with_identity(key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            key: ProviderKey::new(key),
            base_url: base_url.into(),
            client: crate::http::default_client(),
        }
    }

    fn create_url(&self, model: &str) -> String {
        match model_route(model) {
            ModelRoute::Version => format!("{}/v1/predictions", self.base_url),
            ModelRoute::Slug => format!("{}/v1/models/{}/predictions", self.base_url, model),
        }
    }
}

impl Default for ReplicateProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Which prediction-creation route a model id selects.
#[derive(Debug, PartialEq, Eq)]
enum ModelRoute {
    /// Bare 64-char hex version id → `/v1/predictions`, version in the body.
    Version,
    /// `owner/name` slug → `/v1/models/{owner}/{name}/predictions`.
    Slug,
}

/// Classify a Replicate model id. Split out as a free function so the routing
/// rule is testable without HTTP — getting this wrong sends every request to the
/// wrong endpoint.
fn model_route(model: &str) -> ModelRoute {
    let is_version_id = model.len() == 64
        && model
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase());
    if is_version_id {
        ModelRoute::Version
    } else {
        ModelRoute::Slug
    }
}

/// Flatten the gateway's chat messages into Replicate's `{prompt, system_prompt}`
/// input shape. Replicate models take a single prompt string, not a turn list.
fn build_input(req: &ChatRequest) -> serde_json::Value {
    let mut system = Vec::new();
    let mut turns = Vec::new();
    for m in &req.messages {
        let text = match &m.content {
            Some(MessageContent::Text(t)) => t.clone(),
            Some(MessageContent::Parts(parts)) => parts
                .iter()
                .filter_map(|p| match p {
                    kgateway_core::schema::ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
            None => String::new(),
        };
        if text.is_empty() {
            continue;
        }
        match m.role {
            Role::System => system.push(text),
            Role::User => turns.push(format!("User: {text}")),
            Role::Assistant => turns.push(format!("Assistant: {text}")),
            // Tool results carry no Replicate equivalent; fold them in as context.
            Role::Tool => turns.push(format!("Tool: {text}")),
        }
    }

    let mut input = serde_json::Map::new();
    input.insert("prompt".into(), turns.join("\n").into());
    if !system.is_empty() {
        input.insert("system_prompt".into(), system.join("\n\n").into());
    }
    if let Some(t) = req.temperature {
        input.insert("temperature".into(), t.into());
    }
    if let Some(m) = req.max_tokens {
        input.insert("max_tokens".into(), m.into());
    }
    serde_json::Value::Object(input)
}

#[derive(Deserialize)]
struct Prediction {
    #[serde(default)]
    id: String,
    #[serde(default)]
    status: String,
    /// Model output: Replicate emits either a single string or an array of token
    /// strings that the caller concatenates.
    #[serde(default)]
    output: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<serde_json::Value>,
    #[serde(default)]
    urls: PredictionUrls,
}

#[derive(Deserialize, Default)]
struct PredictionUrls {
    #[serde(default)]
    get: Option<String>,
    #[serde(default)]
    stream: Option<String>,
}

impl Prediction {
    fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "succeeded" | "failed" | "canceled")
    }

    /// Concatenate the output into a single string, accepting both shapes.
    fn output_text(&self) -> String {
        match &self.output {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .concat(),
            _ => String::new(),
        }
    }
}

#[async_trait]
impl Provider for ReplicateProvider {
    fn key(&self) -> ProviderKey {
        self.key.clone()
    }

    async fn chat(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: ChatRequest,
    ) -> Result<ChatResponse, KgError> {
        let model = req.model.clone();
        let mut body = serde_json::json!({ "input": build_input(&req) });
        if model_route(&model) == ModelRoute::Version {
            body["version"] = serde_json::Value::String(model.clone());
        }

        let resp = self
            .client
            .post(self.create_url(&model))
            .bearer_auth(&key.value)
            // Ask Replicate to hold the connection until the prediction settles.
            // Honoured on a best-effort basis, hence the poll fallback below.
            .header("Prefer", "wait")
            .timeout(crate::http::REQUEST_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(net_err)?;

        let mut prediction: Prediction = self.decode(resp).await?;

        // Poll until terminal, bounded. `Prefer: wait` usually makes this a no-op.
        let deadline = tokio::time::Instant::now() + MAX_POLL;
        while !prediction.is_terminal() {
            if tokio::time::Instant::now() >= deadline {
                return Err(KgError::provider(
                    format!(
                        "replicate prediction {} did not settle within {}s",
                        prediction.id,
                        MAX_POLL.as_secs()
                    ),
                    504,
                )
                .with_provider(self.key.as_str())
                .with_retryable(true));
            }
            let Some(get_url) = prediction.urls.get.clone() else {
                return Err(KgError::new(
                    KgErrorKind::Internal,
                    "replicate prediction is not terminal and exposes no poll URL",
                ));
            };
            tokio::time::sleep(POLL_INTERVAL).await;
            let resp = self
                .client
                .get(&get_url)
                .bearer_auth(&key.value)
                .timeout(crate::http::REQUEST_TIMEOUT)
                .send()
                .await
                .map_err(net_err)?;
            prediction = self.decode(resp).await?;
        }

        if prediction.status != "succeeded" {
            // The upstream reason stays server-side; `handlers::error_body` scrubs it.
            let detail = prediction
                .error
                .as_ref()
                .map(|e| e.to_string())
                .unwrap_or_else(|| prediction.status.clone());
            return Err(KgError::provider(detail, 502).with_provider(self.key.as_str()));
        }

        Ok(ChatResponse {
            id: prediction.id.clone(),
            object: "chat.completion".to_string(),
            model,
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: Some(MessageContent::Text(prediction.output_text())),
                    name: None,
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            // Replicate reports token counts only in prediction logs, which this
            // connector deliberately does not scrape — usage stays zeroed rather
            // than guessed, so cost analytics show "—" instead of a wrong number.
            usage: Usage::default(),
        })
    }

    async fn chat_stream(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: ChatRequest,
    ) -> Result<ChunkStream, KgError> {
        let model = req.model.clone();
        let mut body = serde_json::json!({ "input": build_input(&req), "stream": true });
        if model_route(&model) == ModelRoute::Version {
            body["version"] = serde_json::Value::String(model.clone());
        }

        let resp = self
            .client
            .post(self.create_url(&model))
            .bearer_auth(&key.value)
            .timeout(crate::http::REQUEST_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(net_err)?;

        let prediction: Prediction = self.decode(resp).await?;
        let stream_url = prediction.urls.stream.clone().ok_or_else(|| {
            KgError::new(
                KgErrorKind::Internal,
                "replicate did not return a stream URL for this prediction",
            )
        })?;

        // Second request: the SSE token stream. No `.timeout()` here — that would
        // cap the whole stream rather than the connect, and the engine already
        // applies its own idle-timeout guard.
        let sse = self
            .client
            .get(&stream_url)
            .bearer_auth(&key.value)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .map_err(net_err)?;

        let status = sse.status();
        if !status.is_success() {
            let text = sse.text().await.unwrap_or_default();
            return Err(KgError::provider(text, status.as_u16()).with_provider(self.key.as_str()));
        }

        Ok(sse_to_chunks(sse.bytes_stream(), prediction.id, model))
    }
}

impl ReplicateProvider {
    /// Shared status-check + JSON decode for prediction responses.
    async fn decode(&self, resp: reqwest::Response) -> Result<Prediction, KgError> {
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(KgError::provider(text, status.as_u16()).with_provider(self.key.as_str()));
        }
        resp.json()
            .await
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))
    }
}

/// Parse Replicate's SSE token stream into `StreamChunk`s.
///
/// Replicate frames differ from OpenAI's: events are named (`event: output`,
/// `event: done`) and the `data:` payload is a **raw token**, not JSON. A blank
/// `data:` line is a literal newline in the output, which is why the payload is
/// joined with `\n` rather than trimmed away.
///
/// Free function over the byte stream so it is unit-testable without HTTP.
fn sse_to_chunks<S>(byte_stream: S, id: String, model: String) -> ChunkStream
where
    S: futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    Box::pin(async_stream::stream! {
        let mut buf: Vec<u8> = Vec::new();
        futures::pin_mut!(byte_stream);

        while let Some(next) = byte_stream.next().await {
            let bytes = match next {
                Ok(b) => b,
                Err(e) => {
                    yield Err(net_err(e));
                    return;
                }
            };
            buf.extend_from_slice(&bytes);

            while let Some(pos) = find_subslice(&buf, b"\n\n") {
                let frame = buf.drain(..pos + 2).collect::<Vec<u8>>();
                let frame = String::from_utf8_lossy(&frame);

                let mut event = "";
                let mut data_lines: Vec<&str> = Vec::new();
                for line in frame.lines() {
                    if let Some(rest) = line.strip_prefix("event:") {
                        event = rest.trim();
                    } else if let Some(rest) = line.strip_prefix("data:") {
                        // Exactly one leading space is part of the SSE framing;
                        // anything beyond it is real output.
                        data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
                    }
                }

                match event {
                    "done" => return,
                    "error" => {
                        yield Err(KgError::provider(data_lines.join("\n"), 502)
                            .with_provider("replicate"));
                        return;
                    }
                    // "output" and anything unrecognized-but-carrying-data.
                    _ if !data_lines.is_empty() => {
                        yield Ok(StreamChunk {
                            id: id.clone(),
                            object: "chat.completion.chunk".to_string(),
                            model: model.clone(),
                            choices: vec![StreamChoice {
                                index: 0,
                                delta: Delta {
                                    role: None,
                                    content: Some(data_lines.join("\n")),
                                    tool_calls: Vec::new(),
                                },
                                finish_reason: None,
                            }],
                            usage: None,
                        });
                    }
                    _ => {}
                }
            }
        }
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn net_err(e: reqwest::Error) -> KgError {
    KgError::new(KgErrorKind::Network, e.to_string()).with_retryable(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const VERSION_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn test_key() -> ApiKey {
        ApiKey {
            id: "k".into(),
            value: "r8-test-key".into(),
            weight: 1,
            models: vec![],
        }
    }

    fn req(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: vec![Message::system("be terse"), Message::user("hi")],
            temperature: Some(0.5),
            ..Default::default()
        }
    }

    #[test]
    fn model_route_discriminates_version_ids_from_slugs() {
        assert_eq!(model_route(VERSION_ID), ModelRoute::Version);
        assert_eq!(model_route("meta/llama-2-70b-chat"), ModelRoute::Slug);
        // 63 chars — one short of a version id, so it stays a slug.
        assert_eq!(model_route(&VERSION_ID[..63]), ModelRoute::Slug);
        // Right length, but not hex.
        assert_eq!(model_route(&"z".repeat(64)), ModelRoute::Slug);
    }

    #[test]
    fn build_input_lifts_system_and_labels_turns() {
        let input = build_input(&req("meta/llama-2-70b-chat"));
        assert_eq!(input["system_prompt"], "be terse");
        assert_eq!(input["prompt"], "User: hi");
        assert_eq!(input["temperature"], 0.5);
        // `max_tokens` was unset, so it must be absent rather than defaulted.
        assert!(input.get("max_tokens").is_none());
    }

    #[test]
    fn output_text_accepts_both_string_and_array_shapes() {
        let mut p = Prediction {
            id: "p".into(),
            status: "succeeded".into(),
            output: Some(serde_json::json!("hello world")),
            error: None,
            urls: PredictionUrls::default(),
        };
        assert_eq!(p.output_text(), "hello world");
        p.output = Some(serde_json::json!(["hel", "lo ", "world"]));
        assert_eq!(p.output_text(), "hello world");
        p.output = None;
        assert_eq!(p.output_text(), "");
    }

    #[tokio::test]
    async fn chat_slug_model_posts_to_model_route_and_waits() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/models/meta/llama-2-70b-chat/predictions"))
            .and(header("authorization", "Bearer r8-test-key"))
            .and(header("prefer", "wait"))
            .and(body_partial_json(serde_json::json!({
                "input": { "prompt": "User: hi", "system_prompt": "be terse" }
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "pred-1",
                "status": "succeeded",
                "output": ["Hel", "lo!"]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let p = ReplicateProvider::with_base_url(server.uri());
        let out = p
            .chat(&Ctx::new(), &test_key(), req("meta/llama-2-70b-chat"))
            .await
            .expect("chat should succeed");

        assert_eq!(out.choices[0].message.text_content(), Some("Hello!"));
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("stop"));
        assert_eq!(out.model, "meta/llama-2-70b-chat");
    }

    #[tokio::test]
    async fn chat_version_id_posts_to_predictions_with_version_in_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/predictions"))
            .and(body_partial_json(
                serde_json::json!({ "version": VERSION_ID }),
            ))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "pred-2",
                "status": "succeeded",
                "output": "ok"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let p = ReplicateProvider::with_base_url(server.uri());
        let out = p
            .chat(&Ctx::new(), &test_key(), req(VERSION_ID))
            .await
            .expect("version-id chat should succeed");
        assert_eq!(out.choices[0].message.text_content(), Some("ok"));
    }

    #[tokio::test]
    async fn chat_failed_prediction_maps_to_provider_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/models/a/b/predictions"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "pred-3",
                "status": "failed",
                "error": "CUDA out of memory"
            })))
            .mount(&server)
            .await;

        let p = ReplicateProvider::with_base_url(server.uri());
        let err = p
            .chat(&Ctx::new(), &test_key(), req("a/b"))
            .await
            .expect_err("a failed prediction must surface as an error");

        // A terminal upstream failure maps to 502, which is retryable — and that
        // is deliberate. `is_retryable()` gates failover to the NEXT provider in
        // the chain, not a retry against the same one, and Replicate prediction
        // failures are usually capacity/OOM rather than a rejection of the input.
        // So a configured fallback should get its shot.
        assert_eq!(err.status, Some(502));
        assert_eq!(err.provider.as_deref(), Some("replicate"));
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn chat_error_429_is_retryable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/models/a/b/predictions"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(&server)
            .await;

        let p = ReplicateProvider::with_base_url(server.uri());
        let err = p
            .chat(&Ctx::new(), &test_key(), req("a/b"))
            .await
            .expect_err("429 should map to an error");
        assert!(err.is_retryable(), "429 must be retryable");
        assert_eq!(err.status, Some(429));
        assert_eq!(err.provider.as_deref(), Some("replicate"));
    }

    // ---- SSE parsing (no HTTP) ----

    fn chunks_from(frames: Vec<&'static str>) -> ChunkStream {
        let stream = futures::stream::iter(
            frames
                .into_iter()
                .map(|f| Ok::<_, reqwest::Error>(bytes::Bytes::from(f))),
        );
        sse_to_chunks(stream, "pred".into(), "meta/llama".into())
    }

    async fn collect_text(mut s: ChunkStream) -> String {
        let mut out = String::new();
        while let Some(c) = s.next().await {
            let c = c.expect("no stream error expected");
            if let Some(t) = &c.choices[0].delta.content {
                out.push_str(t);
            }
        }
        out
    }

    #[tokio::test]
    async fn sse_concatenates_output_tokens() {
        let s = chunks_from(vec![
            "event: output\ndata: Hel\n\n",
            "event: output\ndata: lo!\n\n",
            "event: done\ndata: \n\n",
        ]);
        assert_eq!(collect_text(s).await, "Hello!");
    }

    #[tokio::test]
    async fn sse_handles_frames_split_across_byte_chunks() {
        let s = chunks_from(vec![
            "event: out",
            "put\ndata: par",
            "tial\n\nevent: done\ndata: \n\n",
        ]);
        assert_eq!(collect_text(s).await, "partial");
    }

    #[tokio::test]
    async fn sse_preserves_newlines_encoded_as_blank_data_lines() {
        // Replicate encodes a literal newline as two data lines, one empty.
        let s = chunks_from(vec!["event: output\ndata: a\ndata: \ndata: b\n\n"]);
        assert_eq!(collect_text(s).await, "a\n\nb");
    }

    #[tokio::test]
    async fn sse_done_event_terminates_before_later_frames() {
        let s = chunks_from(vec![
            "event: output\ndata: kept\n\n",
            "event: done\ndata: \n\n",
            "event: output\ndata: dropped\n\n",
        ]);
        assert_eq!(collect_text(s).await, "kept");
    }

    #[tokio::test]
    async fn sse_error_event_surfaces_as_stream_error() {
        let mut s = chunks_from(vec!["event: error\ndata: model exploded\n\n"]);
        let first = s.next().await.expect("one item expected");
        let err = first.expect_err("error event must yield Err");
        assert_eq!(err.provider.as_deref(), Some("replicate"));
        assert_eq!(err.status, Some(502));
    }
}
