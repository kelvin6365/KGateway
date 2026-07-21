//! Native Anthropic Messages API provider.
//!
//! Anthropic's wire format differs from OpenAI's in several ways that this adapter
//! reconciles against the internal (OpenAI-shaped) schema:
//! - auth via `x-api-key` + `anthropic-version` headers (not bearer);
//! - `system` prompt is a top-level string, not a message with a "system" role;
//! - `max_tokens` is REQUIRED;
//! - responses return `content` as an array of typed blocks;
//! - streaming uses named SSE events (`content_block_delta`, `message_stop`, ...).
//!
//! The SSE parser reuses the same byte-buffering strategy as `openai.rs`: buffer
//! raw bytes and decode only complete frames, so a multibyte UTF-8 char split
//! across two network chunks is never corrupted.

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use kgateway_core::context::Ctx;
use kgateway_core::error::{KgError, KgErrorKind};
use kgateway_core::provider::{ApiKey, ChunkStream, Provider, ProviderKey};
use kgateway_core::schema::{
    ChatRequest, ChatResponse, Choice, ContentPart, Delta, Message, MessageContent, Role,
    StreamChoice, StreamChunk, Usage,
};
use serde::{Deserialize, Serialize};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Parse a `data:<media_type>;base64,<data>` URI into its components.
/// Returns `None` if the URI doesn't match the expected format.
fn parse_data_uri(uri: &str) -> Option<(&str, &str)> {
    let rest = uri.strip_prefix("data:")?;
    let semicolon = rest.find(';')?;
    let comma = rest.find(',')?;
    if comma < semicolon {
        return None;
    }
    let media_type = &rest[..semicolon];
    let data = &rest[comma + 1..];
    // Validate that the encoding is base64
    let encoding = &rest[semicolon + 1..comma];
    if !encoding.contains("base64") {
        return None;
    }
    Some((media_type, data))
}

pub struct AnthropicProvider {
    key: ProviderKey,
    base_url: String,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            key: ProviderKey::new("anthropic"),
            base_url: base_url.into(),
            client: crate::http::default_client(),
        }
    }

    /// Register an Anthropic-compatible provider under a custom name + base URL (e.g.
    /// z.ai's GLM Coding Plan at `https://api.z.ai/api/anthropic`).
    pub fn with_identity(key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            key: ProviderKey::new(key),
            base_url: base_url.into(),
            client: crate::http::default_client(),
        }
    }

    /// Convert an internal [`ChatRequest`] into an Anthropic Messages request body. Maps
    /// tools, assistant `tool_calls` → `tool_use` blocks, and `Role::Tool` → `tool_result`
    /// blocks. Consecutive same-role turns are merged, since Anthropic requires strictly
    /// alternating user/assistant messages (Claude Code batches tool results into one turn).
    fn body(&self, req: &ChatRequest, stream: bool) -> AnthropicRequest {
        let mut system_parts: Vec<String> = Vec::new();
        // (role, content-blocks) pairs, block-form throughout so merging is uniform.
        let mut turns: Vec<(String, Vec<serde_json::Value>)> = Vec::new();

        let mut push = |role: &str, block: serde_json::Value| match turns.last_mut() {
            Some((r, blocks)) if r == role => blocks.push(block),
            _ => turns.push((role.to_string(), vec![block])),
        };

        for m in &req.messages {
            match m.role {
                Role::System => {
                    if let Some(c) = &m.content {
                        system_parts.push(c.to_text().unwrap_or_default());
                    }
                }
                Role::User => {
                    if let Some(content) = &m.content {
                        // Multimodal: emit each content part as its own Anthropic block.
                        match content {
                            MessageContent::Text(t) => {
                                push("user", serde_json::json!({ "type": "text", "text": t }));
                            }
                            MessageContent::Parts(parts) => {
                                for p in parts {
                                    match p {
                                        ContentPart::Text { text } => {
                                            push(
                                                "user",
                                                serde_json::json!({ "type": "text", "text": text }),
                                            );
                                        }
                                        ContentPart::ImageUrl { image_url } => {
                                            // Anthropic image block: source.type = "base64" with media_type,
                                            // or "url" for public URLs.
                                            if image_url.url.starts_with("data:") {
                                                // Parse data URI: data:<media_type>;base64,<data>
                                                if let Some((media_type, data)) =
                                                    parse_data_uri(&image_url.url)
                                                {
                                                    push(
                                                        "user",
                                                        serde_json::json!({
                                                            "type": "image",
                                                            "source": {
                                                                "type": "base64",
                                                                "media_type": media_type,
                                                                "data": data,
                                                            }
                                                        }),
                                                    );
                                                }
                                            } else {
                                                push(
                                                    "user",
                                                    serde_json::json!({
                                                        "type": "image",
                                                        "source": {
                                                            "type": "url",
                                                            "url": image_url.url,
                                                        }
                                                    }),
                                                );
                                            }
                                        }
                                        ContentPart::Other => {
                                            // Unknown part type — skip for Anthropic (can't map safely)
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Role::Tool => {
                    // A tool result is delivered as a user-turn `tool_result` block.
                    push(
                        "user",
                        serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                            "content": m.text_or_empty(),
                        }),
                    );
                }
                Role::Assistant => {
                    if let Some(text) = m.content.as_ref().and_then(|c| c.to_text()) {
                        if !text.is_empty() {
                            push(
                                "assistant",
                                serde_json::json!({ "type": "text", "text": text }),
                            );
                        }
                    }
                    for tc in &m.tool_calls {
                        let input: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                            .unwrap_or_else(|_| serde_json::json!({}));
                        push(
                            "assistant",
                            serde_json::json!({
                                "type": "tool_use",
                                "id": tc.id,
                                "name": tc.function.name,
                                "input": input,
                            }),
                        );
                    }
                }
            }
        }

        let messages = turns
            .into_iter()
            .map(|(role, blocks)| AnthropicMessage {
                role,
                content: serde_json::Value::Array(blocks),
            })
            .collect();

        let tools =
            req.tools
                .iter()
                .map(|t| AnthropicTool {
                    name: t.function.name.clone(),
                    description: t.function.description.clone(),
                    input_schema: t.function.parameters.clone().unwrap_or_else(
                        || serde_json::json!({ "type": "object", "properties": {} }),
                    ),
                })
                .collect();

        // `stop` may be a string or an array in the internal schema; Anthropic wants an array.
        let stop_sequences = req.stop.as_ref().and_then(|v| match v {
            serde_json::Value::String(s) => Some(vec![s.clone()]),
            serde_json::Value::Array(a) => Some(
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect(),
            ),
            _ => None,
        });

        let system = if system_parts.is_empty() {
            None
        } else {
            Some(system_parts.join("\n\n"))
        };

        AnthropicRequest {
            model: req.model_id().to_string(),
            // max_tokens is required by Anthropic; default when the caller omits it.
            max_tokens: req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            messages,
            system,
            temperature: req.temperature,
            top_p: req.top_p,
            stop_sequences,
            tools,
            stream,
        }
    }

    fn url(&self) -> String {
        format!("{}/v1/messages", self.base_url)
    }
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn key(&self) -> ProviderKey {
        self.key.clone()
    }

    async fn chat(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: ChatRequest,
    ) -> Result<ChatResponse, KgError> {
        let resp = self
            .client
            .post(self.url())
            .header("x-api-key", &key.value)
            // Anthropic-compatible endpoints like z.ai's GLM Coding Plan authenticate via
            // `Authorization: Bearer`; real Anthropic uses x-api-key and ignores this.
            .bearer_auth(&key.value)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .timeout(crate::http::REQUEST_TIMEOUT)
            .json(&self.body(&req, false))
            .send()
            .await
            .map_err(net_err)?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(KgError::provider(text, status.as_u16()).with_provider(self.key.as_str()));
        }

        let ar: AnthropicResponse = resp
            .json()
            .await
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))?;

        Ok(ar.into_chat_response())
    }

    async fn chat_stream(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: ChatRequest,
    ) -> Result<ChunkStream, KgError> {
        let model = req.model_id().to_string();
        let resp = self
            .client
            .post(self.url())
            .header("x-api-key", &key.value)
            .bearer_auth(&key.value)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&self.body(&req, true))
            .send()
            .await
            .map_err(net_err)?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(KgError::provider(text, status.as_u16()).with_provider(self.key.as_str()));
        }

        Ok(sse_to_chunks(resp.bytes_stream(), model).boxed())
    }
}

fn net_err(e: reqwest::Error) -> KgError {
    KgError::new(KgErrorKind::Network, e.to_string()).with_retryable(true)
}

// ---- Anthropic wire types ----

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
    /// Tool definitions in Anthropic shape (`name` / `description` / `input_schema`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
    stream: bool,
}

/// One Anthropic message. `content` is a string OR an array of content blocks (`text` /
/// `tool_use` / `tool_result`), so we model it as raw JSON built in `body()`.
#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: String,
    content: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: AnthropicUsage,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    // tool_use fields:
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

impl AnthropicResponse {
    fn into_chat_response(self) -> ChatResponse {
        // Concatenate text blocks; convert tool_use blocks into internal tool calls.
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for b in &self.content {
            match b.kind.as_str() {
                "text" => {
                    if let Some(t) = &b.text {
                        text.push_str(t);
                    }
                }
                "tool_use" => {
                    let args = b
                        .input
                        .as_ref()
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "{}".to_string());
                    tool_calls.push(kgateway_core::schema::ToolCall {
                        id: b.id.clone().unwrap_or_default(),
                        kind: "function".to_string(),
                        function: kgateway_core::schema::FunctionCall {
                            name: b.name.clone().unwrap_or_default(),
                            arguments: args,
                        },
                    });
                }
                _ => {}
            }
        }
        let content = if text.is_empty() { None } else { Some(text) };

        let usage = Usage {
            prompt_tokens: self.usage.input_tokens,
            completion_tokens: self.usage.output_tokens,
            total_tokens: self.usage.input_tokens + self.usage.output_tokens,
        };

        ChatResponse {
            id: self.id,
            object: "chat.completion".to_string(),
            model: self.model,
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: content.map(MessageContent::Text),
                    name: None,
                    tool_calls,
                    tool_call_id: None,
                },
                finish_reason: self.stop_reason,
            }],
            usage,
        }
    }
}

// ---- Streaming ----

/// A single Anthropic SSE event payload (the JSON in a `data:` line). We only need
/// the fields relevant to text streaming; everything else is ignored.
#[derive(Debug, Deserialize)]
struct StreamEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<StreamDelta>,
    #[serde(default)]
    message: Option<StreamMessage>,
    /// `message_delta` carries the running `output_tokens` here (input was set at start).
    #[serde(default)]
    usage: Option<StreamUsage>,
    /// Content-block index (on `content_block_start` / `_delta` / `_stop`).
    #[serde(default)]
    index: u32,
    /// The block on `content_block_start` (e.g. a `tool_use` block with id + name).
    #[serde(default)]
    content_block: Option<StreamContentBlock>,
}

#[derive(Debug, Deserialize)]
struct StreamContentBlock {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
    /// Present on the `message_delta` event's `delta` (e.g. "end_turn", "tool_use", "max_tokens").
    #[serde(default)]
    stop_reason: Option<String>,
    /// Present on an `input_json_delta` — a streamed fragment of a tool call's JSON arguments.
    #[serde(default)]
    partial_json: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamMessage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: String,
    /// `message_start` carries the prompt token count (`input_tokens`).
    #[serde(default)]
    usage: StreamUsage,
}

/// Token usage as it appears on `message_start` (input) and `message_delta` (output).
#[derive(Debug, Default, Deserialize)]
struct StreamUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

/// Build a `StreamChunk` carrying a single tool-call delta fragment, keyed by `index`. Used
/// to translate Anthropic `tool_use` streaming (block start + `input_json_delta`) into the
/// internal `ToolCallDelta` shape.
fn tool_call_chunk(
    msg_id: &str,
    model: &str,
    index: u32,
    id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
) -> StreamChunk {
    let function = if name.is_some() || arguments.is_some() {
        Some(kgateway_core::schema::FunctionCallDelta { name, arguments })
    } else {
        None
    };
    StreamChunk {
        id: msg_id.to_string(),
        object: "chat.completion.chunk".to_string(),
        model: model.to_string(),
        choices: vec![StreamChoice {
            index: 0,
            delta: Delta {
                role: None,
                content: None,
                tool_calls: vec![kgateway_core::schema::ToolCallDelta {
                    index,
                    id,
                    kind: None,
                    function,
                }],
            },
            finish_reason: None,
        }],
        usage: None,
    }
}

/// Parse an Anthropic named-event SSE byte stream into internal `StreamChunk`s.
///
/// Anthropic emits `message_start`, `content_block_start`, `content_block_delta`,
/// `content_block_stop`, `message_delta`, and `message_stop` events. We yield a
/// chunk for every `content_block_delta` carrying a `text_delta`, capture the
/// message id/model from `message_start`, and terminate on `message_stop`.
///
/// A free function over the byte stream so it is unit-testable without a live
/// connection. Byte-buffered (see module docs) to preserve multibyte UTF-8.
fn sse_to_chunks(
    byte_stream: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    model: String,
) -> BoxStream<'static, Result<StreamChunk, KgError>> {
    let s = async_stream::stream! {
        futures::pin_mut!(byte_stream);
        let mut buf: Vec<u8> = Vec::new();
        // Captured from message_start; fall back to the requested model / empty id.
        let mut msg_id = String::new();
        let mut msg_model = model;
        // Prompt tokens arrive on message_start; completion tokens accrue on message_delta.
        let mut input_tokens: u32 = 0;

        while let Some(item) = byte_stream.next().await {
            let bytes = match item {
                Ok(b) => b,
                Err(e) => { yield Err(net_err(e)); return; }
            };
            buf.extend_from_slice(&bytes);

            // Emit every complete SSE frame (terminated by a blank line).
            while let Some(pos) = find_subslice(&buf, b"\n\n") {
                let frame_bytes: Vec<u8> = buf.drain(..pos + 2).collect();
                let frame = String::from_utf8_lossy(&frame_bytes);
                for line in frame.lines() {
                    // Anthropic sends both `event:` and `data:` lines; only the
                    // JSON `data:` payload carries what we need (it has a `type`).
                    let Some(data) = line.trim_start().strip_prefix("data:") else { continue };
                    let data = data.trim();
                    if data.is_empty() { continue; }

                    let ev: StreamEvent = match serde_json::from_str(data) {
                        Ok(ev) => ev,
                        Err(e) => {
                            yield Err(KgError::new(
                                KgErrorKind::Internal,
                                format!("stream decode error: {e}"),
                            ));
                            continue;
                        }
                    };

                    match ev.kind.as_str() {
                        "message_start" => {
                            if let Some(m) = ev.message {
                                if !m.id.is_empty() { msg_id = m.id; }
                                if !m.model.is_empty() { msg_model = m.model; }
                                // Standard Anthropic reports input_tokens here; some bridges
                                // (e.g. z.ai) report 0 here and the real count on message_delta.
                                if m.usage.input_tokens > 0 { input_tokens = m.usage.input_tokens; }
                            }
                        }
                        "content_block_start" => {
                            // A tool_use block opening: emit a tool-call delta carrying the id +
                            // name, keyed by the content-block index so fragments reassemble.
                            if let Some(cb) = &ev.content_block {
                                if cb.kind == "tool_use" {
                                    yield Ok(tool_call_chunk(
                                        &msg_id, &msg_model, ev.index,
                                        cb.id.clone(), cb.name.clone(), None,
                                    ));
                                }
                            }
                        }
                        "content_block_delta" => {
                            let dkind = ev.delta.as_ref().and_then(|d| d.kind.as_deref());
                            match dkind {
                                Some("text_delta") => {
                                    if let Some(text) = ev.delta.and_then(|d| d.text) {
                                        yield Ok(StreamChunk {
                                            id: msg_id.clone(),
                                            object: "chat.completion.chunk".to_string(),
                                            model: msg_model.clone(),
                                            choices: vec![StreamChoice {
                                                index: 0,
                                                delta: Delta {
                                                    role: None,
                                                    content: Some(text),
                                                    ..Default::default()
                                                },
                                                finish_reason: None,
                                            }],
                                            usage: None,
                                        });
                                    }
                                }
                                Some("input_json_delta") => {
                                    // Streamed fragment of a tool call's JSON arguments.
                                    if let Some(pj) = ev.delta.and_then(|d| d.partial_json) {
                                        yield Ok(tool_call_chunk(
                                            &msg_id, &msg_model, ev.index, None, None, Some(pj),
                                        ));
                                    }
                                }
                                _ => {}
                            }
                        }
                        "message_delta" => {
                            // Carries the final stop_reason (in `delta`) and the completion
                            // token count (in `usage.output_tokens`). Some providers also report
                            // the prompt count (`input_tokens`) only here — capture it if so.
                            let (delta_input, output_tokens) = ev
                                .usage
                                .map(|u| (u.input_tokens, u.output_tokens))
                                .unwrap_or((0, 0));
                            if delta_input > 0 { input_tokens = delta_input; }
                            let stop_reason = ev.delta.and_then(|d| d.stop_reason);
                            let choices = match &stop_reason {
                                Some(_) => vec![StreamChoice {
                                    index: 0,
                                    delta: Delta::default(),
                                    finish_reason: stop_reason,
                                }],
                                None => vec![],
                            };
                            yield Ok(StreamChunk {
                                id: msg_id.clone(),
                                object: "chat.completion.chunk".to_string(),
                                model: msg_model.clone(),
                                choices,
                                usage: Some(Usage {
                                    prompt_tokens: input_tokens,
                                    completion_tokens: output_tokens,
                                    total_tokens: input_tokens + output_tokens,
                                }),
                            });
                        }
                        "message_stop" => return,
                        // content_block_start/stop, ping, ... ignored.
                        _ => {}
                    }
                }
            }
        }
    };
    s.boxed()
}

/// Index of the first occurrence of `needle` in `haystack` (byte search).
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::stream;
    use kgateway_core::context::Ctx;
    use kgateway_core::provider::ApiKey;
    use kgateway_core::schema::Message;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_key() -> ApiKey {
        ApiKey {
            id: "test".into(),
            value: "sk-ant-test".into(),
            weight: 1,
            models: vec![],
        }
    }

    fn req_with(messages: Vec<Message>, max_tokens: Option<u32>) -> ChatRequest {
        ChatRequest {
            model: "anthropic/claude-3-5-sonnet-20241022".into(),
            messages,
            temperature: Some(0.7),
            max_tokens,
            ..Default::default()
        }
    }

    #[test]
    fn body_extracts_system_and_defaults_max_tokens() {
        let p = AnthropicProvider::new();
        let req = req_with(
            vec![
                Message::system("be terse"),
                Message::system("and helpful"),
                Message::user("hi"),
            ],
            None,
        );
        let body = p.body(&req, false);
        assert_eq!(body.system.as_deref(), Some("be terse\n\nand helpful"));
        assert_eq!(body.max_tokens, DEFAULT_MAX_TOKENS);
        assert_eq!(body.model, "claude-3-5-sonnet-20241022");
        assert_eq!(body.messages.len(), 1);
        assert_eq!(body.messages[0].role, "user");
    }

    #[test]
    fn body_maps_tools_tool_calls_and_tool_results() {
        use kgateway_core::schema::{FunctionCall, FunctionDef, Tool, ToolCall};
        let p = AnthropicProvider::new();
        let mut req = req_with(
            vec![
                Message::user("weather?"),
                Message {
                    role: Role::Assistant,
                    content: Some("checking".into()),
                    name: None,
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "get_weather".into(),
                            arguments: r#"{"loc":"SF"}"#.into(),
                        },
                    }],
                    tool_call_id: None,
                },
                Message {
                    role: Role::Tool,
                    content: Some("sunny".into()),
                    name: None,
                    tool_calls: vec![],
                    tool_call_id: Some("call_1".into()),
                },
            ],
            Some(50),
        );
        req.tools = vec![Tool {
            kind: "function".into(),
            function: FunctionDef {
                name: "get_weather".into(),
                description: Some("w".into()),
                parameters: Some(serde_json::json!({"type":"object"})),
            },
        }];
        let body = p.body(&req, false);

        assert_eq!(body.tools.len(), 1);
        assert_eq!(body.tools[0].name, "get_weather");
        // user(text) / assistant(text + tool_use) / user(tool_result)
        assert_eq!(body.messages.len(), 3);
        let asst = &body.messages[1];
        assert_eq!(asst.role, "assistant");
        let blocks = asst.content.as_array().unwrap();
        assert!(blocks
            .iter()
            .any(|b| b["type"] == "tool_use" && b["id"] == "call_1"));
        let tool_turn = &body.messages[2];
        assert_eq!(tool_turn.role, "user");
        assert_eq!(tool_turn.content[0]["type"], "tool_result");
        assert_eq!(tool_turn.content[0]["tool_use_id"], "call_1");
    }

    #[tokio::test]
    async fn streaming_parses_tool_use_blocks() {
        // A tool_use block start + two input_json_delta fragments must reassemble into a call.
        let frames = vec![Ok::<_, reqwest::Error>(Bytes::from(concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"get_weather\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"loc\\\":\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"SF\\\"}\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        )))];
        let mut out = sse_to_chunks(stream::iter(frames), "m".into());
        let mut acc = kgateway_core::schema::ToolCallAccumulator::new();
        while let Some(item) = out.next().await {
            let chunk = item.expect("chunk parses");
            for ch in &chunk.choices {
                acc.push(&ch.delta);
            }
        }
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "get_weather");
        assert_eq!(calls[0].function.arguments, r#"{"loc":"SF"}"#);
    }

    #[tokio::test]
    async fn chat_converts_response() {
        let server = MockServer::start().await;
        let resp_body = serde_json::json!({
            "id": "msg_123",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-5-sonnet-20241022",
            "content": [
                {"type": "text", "text": "Hello, "},
                {"type": "text", "text": "world!"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 12, "output_tokens": 5}
        });

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-ant-test"))
            .and(header("anthropic-version", ANTHROPIC_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(resp_body))
            .mount(&server)
            .await;

        let p = AnthropicProvider::with_base_url(server.uri());
        let ctx = Ctx::default();
        let req = req_with(vec![Message::system("sys"), Message::user("hi")], Some(256));
        let out = p.chat(&ctx, &test_key(), req).await.expect("chat ok");

        assert_eq!(out.id, "msg_123");
        assert_eq!(out.object, "chat.completion");
        assert_eq!(out.model, "claude-3-5-sonnet-20241022");
        assert_eq!(out.choices.len(), 1);
        assert_eq!(out.choices[0].message.text_content(), Some("Hello, world!"));
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("end_turn"));
        assert_eq!(out.usage.prompt_tokens, 12);
        assert_eq!(out.usage.completion_tokens, 5);
        assert_eq!(out.usage.total_tokens, 17);
    }

    #[tokio::test]
    async fn chat_sends_extracted_system_and_max_tokens() {
        let server = MockServer::start().await;
        // Match only when the request body has top-level `system` and `max_tokens`.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "system": "you are a bot",
                "max_tokens": 4096,
                "model": "claude-3-5-sonnet-20241022"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_1",
                "model": "claude-3-5-sonnet-20241022",
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let p = AnthropicProvider::with_base_url(server.uri());
        let ctx = Ctx::default();
        let req = req_with(
            vec![Message::system("you are a bot"), Message::user("hi")],
            None,
        );
        let out = p.chat(&ctx, &test_key(), req).await.expect("chat ok");
        assert_eq!(out.choices[0].message.text_content(), Some("ok"));
        // server drop verifies the `.expect(1)` matcher hit.
    }

    #[tokio::test]
    async fn chat_error_429_is_retryable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(429).set_body_string("{\"error\":\"rate limited\"}"),
            )
            .mount(&server)
            .await;

        let p = AnthropicProvider::with_base_url(server.uri());
        let ctx = Ctx::default();
        let req = req_with(vec![Message::user("hi")], None);
        let err = p
            .chat(&ctx, &test_key(), req)
            .await
            .expect_err("should error");
        assert!(err.is_retryable(), "429 must be retryable");
        assert_eq!(err.status, Some(429));
        assert_eq!(err.provider.as_deref(), Some("anthropic"));
    }

    #[tokio::test]
    async fn streaming_accumulates_text_with_split_chunk() {
        // A realistic Anthropic event sequence. The content_block_delta carrying
        // "wor" is deliberately split across a byte-chunk boundary.
        let frames = vec![
            Ok::<_, reqwest::Error>(Bytes::from(
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_9\",\"model\":\"claude-3\"}}\n\n",
            )),
            Ok(Bytes::from(
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello \"}}\n\n",
            )),
            // Split the next frame mid-way to exercise the cross-chunk buffer.
            Ok(Bytes::from(
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"wor",
            )),
            Ok(Bytes::from(
                "ld!\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            )),
            Ok(Bytes::from(
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            )),
        ];

        let mut out = sse_to_chunks(stream::iter(frames), "fallback-model".into());
        let mut text = String::new();
        let mut id = String::new();
        let mut model = String::new();
        while let Some(item) = out.next().await {
            let chunk = item.expect("chunk parses");
            id = chunk.id.clone();
            model = chunk.model.clone();
            if let Some(c) = &chunk.choices[0].delta.content {
                text.push_str(c);
            }
        }
        assert_eq!(text, "Hello world!");
        assert_eq!(id, "msg_9", "id captured from message_start");
        assert_eq!(model, "claude-3", "model captured from message_start");
    }

    #[tokio::test]
    async fn streaming_captures_usage_and_stop_reason() {
        // input_tokens from message_start, output_tokens + stop_reason from message_delta —
        // previously both were dropped, so streamed Anthropic requests logged 0 tokens.
        let frames = vec![Ok::<_, reqwest::Error>(Bytes::from(concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"claude-3\",\"usage\":{\"input_tokens\":12,\"output_tokens\":1}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":15}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        )))];

        let mut out = sse_to_chunks(stream::iter(frames), "fallback".into());
        let mut usage = None;
        let mut stop = None;
        while let Some(item) = out.next().await {
            let chunk = item.expect("chunk parses");
            if let Some(u) = chunk.usage {
                usage = Some(u);
            }
            if let Some(fr) = chunk.choices.first().and_then(|c| c.finish_reason.clone()) {
                stop = Some(fr);
            }
        }
        let u = usage.expect("usage emitted from message_delta");
        assert_eq!(u.prompt_tokens, 12);
        assert_eq!(u.completion_tokens, 15);
        assert_eq!(u.total_tokens, 27);
        assert_eq!(stop.as_deref(), Some("end_turn"));
    }

    #[tokio::test]
    async fn streaming_captures_input_tokens_from_message_delta() {
        // Some Anthropic bridges (e.g. z.ai) report input_tokens=0 on message_start and the
        // real prompt count only on the final message_delta. Capture it from there too.
        let frames = vec![Ok::<_, reqwest::Error>(Bytes::from(concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"glm\",\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":18,\"output_tokens\":12}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        )))];

        let mut out = sse_to_chunks(stream::iter(frames), "fallback".into());
        let mut usage = None;
        while let Some(item) = out.next().await {
            if let Some(u) = item.expect("chunk parses").usage {
                usage = Some(u);
            }
        }
        let u = usage.expect("usage emitted");
        assert_eq!(u.prompt_tokens, 18, "input_tokens taken from message_delta");
        assert_eq!(u.completion_tokens, 12);
        assert_eq!(u.total_tokens, 30);
    }

    #[tokio::test]
    async fn streaming_stops_at_message_stop() {
        let frames = vec![Ok::<_, reqwest::Error>(Bytes::from(
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\ndata: {\"type\":\"message_stop\"}\n\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ignored\"}}\n\n",
        ))];
        let out = sse_to_chunks(stream::iter(frames), "m".into());
        let collected: Vec<_> = out.collect().await;
        assert_eq!(collected.len(), 1, "only the pre-stop delta is yielded");
    }

    #[tokio::test]
    async fn streaming_tool_use_maps_to_tool_call_deltas_and_reassembles() {
        use kgateway_core::schema::ToolCallAccumulator;

        // A realistic tool-turn: a text preamble block (index 0), then a tool_use
        // block (index 1) whose JSON arguments arrive in fragments, closed by a
        // message_delta carrying stop_reason=tool_use.
        let frames = vec![Ok::<_, reqwest::Error>(Bytes::from(concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m7\",\"model\":\"glm-5.2\",\"usage\":{\"input_tokens\":30,\"output_tokens\":0}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Checking\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_w1\",\"name\":\"get_weather\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"HK\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":9}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        )))];

        let mut out = sse_to_chunks(stream::iter(frames), "fallback".into());
        let mut acc = ToolCallAccumulator::new();
        let mut text = String::new();
        let mut stop = None;
        while let Some(item) = out.next().await {
            let chunk = item.expect("chunk parses");
            let Some(choice) = chunk.choices.first() else {
                continue;
            };
            if let Some(c) = &choice.delta.content {
                text.push_str(c);
            }
            acc.push(&choice.delta);
            if let Some(fr) = &choice.finish_reason {
                stop = Some(fr.clone());
            }
        }

        assert_eq!(text, "Checking", "text preamble streams unaffected");
        assert_eq!(stop.as_deref(), Some("tool_use"));
        let calls = acc.finish();
        assert_eq!(calls.len(), 1, "fragments reassemble into one call");
        assert_eq!(calls[0].id, "call_w1");
        assert_eq!(calls[0].function.name, "get_weather");
        assert_eq!(calls[0].function.arguments, "{\"city\":\"HK\"}");
    }
}
