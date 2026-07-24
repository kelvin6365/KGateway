//! Anthropic Messages API **ingress** (`POST /v1/messages`) — lets Anthropic-protocol clients
//! (e.g. Claude Code) talk to KGateway. It is the mirror of the Anthropic *connector*: it
//! translates an inbound Anthropic request into the internal [`ChatRequest`], runs it through
//! the engine (so governance / logging / failover / cache all apply), then translates the
//! response — unary AND streaming (SSE), including `tool_use` — back to Anthropic shape.
//!
//! Route the request to any provider via the model string, e.g. `"zai/glm-4.6"`.

use crate::app::SharedState;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use kgateway_core::context::Ctx;
use kgateway_core::error::{KgError, KgErrorKind};
use kgateway_core::schema::{
    ChatRequest, ChatResponse, FunctionCall, FunctionDef, Message, Role, Tool, ToolCall,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;

// ---- Inbound request types ----

#[derive(Debug, Deserialize)]
pub struct AnthropicMessagesRequest {
    model: String,
    /// System prompt — a string or an array of text blocks.
    #[serde(default)]
    system: Option<Value>,
    messages: Vec<InMessage>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    top_p: Option<f32>,
    #[serde(default)]
    stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default)]
    tools: Option<Vec<InTool>>,
    /// Anthropic request `metadata` (e.g. `user_id`). Modeled only so we can group calls
    /// into a session — Claude Code sets `metadata.user_id` to a per-session identifier.
    /// Passthrough only; not translated into the outbound request.
    #[serde(default)]
    metadata: Option<AnthropicMetadata>,
}

/// The subset of Anthropic request `metadata` we read. `user_id` is the session hint.
#[derive(Debug, Deserialize)]
struct AnthropicMetadata {
    #[serde(default)]
    user_id: Option<String>,
}

/// Derive a session id from an Anthropic `metadata.user_id`. Claude Code sends values
/// shaped like `user_<account-hash>_account__session_<uuid>`; when that shape is present we
/// key on the `session_<uuid>` tail so distinct sessions of one account stay distinct.
/// Anything else is used verbatim (sanitized downstream).
fn derive_session_id(user_id: &str) -> Option<String> {
    let trimmed = user_id.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.rfind("session_") {
        Some(idx) => Some(trimmed[idx..].to_string()),
        None => Some(trimmed.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct InMessage {
    role: String,
    /// A string or an array of content blocks (`text` / `tool_use` / `tool_result`).
    content: Value,
}

#[derive(Debug, Deserialize)]
struct InTool {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    input_schema: Option<Value>,
}

/// Flatten an Anthropic content value (string or array of blocks) to plain text.
fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect(),
        _ => String::new(),
    }
}

/// Translate an inbound Anthropic request into the internal [`ChatRequest`].
pub fn to_chat_request(req: AnthropicMessagesRequest) -> ChatRequest {
    let mut messages = Vec::new();
    if let Some(sys) = &req.system {
        let t = value_text(sys);
        if !t.is_empty() {
            messages.push(Message::system(t));
        }
    }
    for m in &req.messages {
        translate_in_message(m, &mut messages);
    }

    let tools = req
        .tools
        .unwrap_or_default()
        .into_iter()
        .map(|t| Tool {
            kind: "function".to_string(),
            function: FunctionDef {
                name: t.name,
                description: t.description,
                parameters: t.input_schema,
            },
        })
        .collect();

    ChatRequest {
        model: req.model,
        messages,
        temperature: req.temperature,
        top_p: req.top_p,
        max_tokens: req.max_tokens,
        stop: req.stop_sequences.map(|s| json!(s)),
        tools,
        stream: req.stream,
        ..Default::default()
    }
}

/// Expand one Anthropic message into 0+ internal messages. Assistant `tool_use` blocks become
/// `tool_calls`; user `tool_result` blocks each become a `Role::Tool` message (OpenAI shape).
///
/// A mid-conversation `"role": "system"` turn (some clients emit these alongside the top-level
/// `system` field) keeps [`Role::System`] rather than being demoted to a user turn.
fn translate_in_message(m: &InMessage, out: &mut Vec<Message>) {
    let assistant = m.role == "assistant";
    let plain_role = match m.role.as_str() {
        "assistant" => Role::Assistant,
        "system" => Role::System,
        _ => Role::User,
    };
    match &m.content {
        Value::String(s) => out.push(Message {
            role: plain_role,
            content: Some(s.clone().into()),
            name: None,
            tool_calls: vec![],
            tool_call_id: None,
        }),
        Value::Array(blocks) => {
            let mut text = String::new();
            let mut tool_calls = Vec::new();
            let mut tool_results: Vec<(String, String)> = Vec::new();
            for b in blocks {
                match b.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "text" => {
                        if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                            text.push_str(t);
                        }
                    }
                    "tool_use" => tool_calls.push(ToolCall {
                        id: b
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: b
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            arguments: b
                                .get("input")
                                .cloned()
                                .unwrap_or_else(|| json!({}))
                                .to_string(),
                        },
                    }),
                    "tool_result" => tool_results.push((
                        b.get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        b.get("content").map(value_text).unwrap_or_default(),
                    )),
                    "image" => {
                        // Anthropic image block → OpenAI image_url part (best-effort).
                        // We only forward it if it has a base64 source we can re-encode as a data URI.
                        if let Some(source) = b.get("source") {
                            if source.get("type").and_then(|v| v.as_str()) == Some("base64") {
                                if let (Some(media_type), Some(data)) = (
                                    source.get("media_type").and_then(|v| v.as_str()),
                                    source.get("data").and_then(|v| v.as_str()),
                                ) {
                                    let data_uri = format!("data:{media_type};base64,{data}");
                                    // Images require multipart content — convert text to Parts
                                    // (handled below in the push logic)
                                    text.push_str(&format!("[image:{data_uri}]"));
                                }
                            }
                        }
                    }
                    _ => {} // other blocks — ignored for now
                }
            }
            if assistant {
                out.push(Message {
                    role: Role::Assistant,
                    content: if text.is_empty() {
                        None
                    } else {
                        Some(text.into())
                    },
                    name: None,
                    tool_calls,
                    tool_call_id: None,
                });
            } else {
                // Tool results first (each its own tool message), then any user text.
                for (tool_use_id, content) in tool_results {
                    out.push(Message {
                        role: Role::Tool,
                        content: Some(content.into()),
                        name: None,
                        tool_calls: vec![],
                        tool_call_id: Some(tool_use_id),
                    });
                }
                if !text.is_empty() {
                    out.push(Message {
                        role: plain_role,
                        content: Some(text.into()),
                        name: None,
                        tool_calls: vec![],
                        tool_call_id: None,
                    });
                }
            }
        }
        _ => {}
    }
}

// ---- Outbound (response) translation ----

/// Map an internal finish reason to an Anthropic `stop_reason`. Values already in Anthropic
/// form (e.g. from the Anthropic connector) pass through.
fn map_stop_reason(fr: &str) -> String {
    match fr {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        other => other,
    }
    .to_string()
}

/// Translate an internal [`ChatResponse`] into an Anthropic Messages response.
pub fn to_anthropic_response(resp: ChatResponse) -> Value {
    let choice = resp.choices.into_iter().next();
    let mut blocks: Vec<Value> = Vec::new();
    let mut stop_reason = "end_turn".to_string();
    let usage = json!({
        "input_tokens": resp.usage.prompt_tokens,
        "output_tokens": resp.usage.completion_tokens,
    });

    if let Some(c) = choice {
        if let Some(mc) = &c.message.content {
            if let Some(text) = mc.as_text() {
                if !text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
            }
        }
        for tc in &c.message.tool_calls {
            let input: Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or_else(|_| json!({}));
            blocks.push(json!({
                "type": "tool_use",
                "id": tc.id,
                "name": tc.function.name,
                "input": input,
            }));
        }
        stop_reason = if c.message.tool_calls.is_empty() {
            c.finish_reason
                .as_deref()
                .map(map_stop_reason)
                .unwrap_or_else(|| "end_turn".to_string())
        } else {
            "tool_use".to_string()
        };
    }

    json!({
        "id": resp.id,
        "type": "message",
        "role": "assistant",
        "model": resp.model,
        "content": blocks,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage,
    })
}

// ---- Streaming translation ----

fn ev(name: &str, v: Value) -> Result<Event, Infallible> {
    Ok(Event::default().event(name).data(v.to_string()))
}

/// A safe, non-leaking error message for a `KgError` (never the raw upstream body).
fn safe_message(e: &KgError) -> &'static str {
    match e.kind {
        KgErrorKind::Auth => "authentication error",
        KgErrorKind::RateLimit => "rate limit or budget exceeded",
        KgErrorKind::BadRequest => "invalid request",
        KgErrorKind::Unsupported => "unsupported operation",
        KgErrorKind::Network => "upstream network error",
        KgErrorKind::Provider => "upstream provider error",
        KgErrorKind::Internal => "internal error",
    }
}

fn anthropic_error_type(e: &KgError) -> &'static str {
    match e.kind {
        KgErrorKind::Auth => "authentication_error",
        KgErrorKind::RateLimit => "rate_limit_error",
        KgErrorKind::BadRequest => "invalid_request_error",
        KgErrorKind::Network | KgErrorKind::Provider => "api_error",
        _ => "api_error",
    }
}

/// Build the Anthropic error HTTP response (scrubbed message + mapped status).
fn anthropic_error_response(e: KgError) -> Response {
    let body = json!({
        "type": "error",
        "error": { "type": anthropic_error_type(&e), "message": safe_message(&e) },
    });
    let status = axum::http::StatusCode::from_u16(e.http_status())
        .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
    (status, Json(body)).into_response()
}

/// The currently-open Anthropic content block: text, or a tool_use keyed by internal tool index.
#[derive(Clone, Copy)]
enum Cur {
    None,
    Text(u32),
    Tool { internal: u32, aidx: u32 },
}

impl Cur {
    fn aidx(&self) -> Option<u32> {
        match self {
            Cur::None => None,
            Cur::Text(i) | Cur::Tool { aidx: i, .. } => Some(*i),
        }
    }
}

/// Translate an internal chunk stream into an Anthropic SSE event stream.
fn anthropic_sse(inner: kgateway_core::provider::ChunkStream) -> Response {
    let stream = async_stream::stream! {
        futures::pin_mut!(inner);
        let mut started = false;
        let mut next_index: u32 = 0;
        let mut cur = Cur::None;
        let mut stop_reason: Option<String> = None;
        let mut saw_tool = false;
        let mut output_tokens: u32 = 0;

        while let Some(item) = inner.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(e) => {
                    yield ev("error", json!({
                        "type": "error",
                        "error": { "type": anthropic_error_type(&e), "message": safe_message(&e) },
                    }));
                    break;
                }
            };

            if !started {
                started = true;
                let id = if chunk.id.is_empty() { "msg_stream" } else { chunk.id.as_str() };
                yield ev("message_start", json!({
                    "type": "message_start",
                    "message": {
                        "id": id, "type": "message", "role": "assistant", "model": chunk.model,
                        "content": [], "stop_reason": null, "stop_sequence": null,
                        "usage": { "input_tokens": 0, "output_tokens": 0 },
                    },
                }));
            }

            if let Some(u) = &chunk.usage {
                output_tokens = u.completion_tokens;
            }
            let Some(choice) = chunk.choices.first() else { continue };
            if let Some(fr) = &choice.finish_reason {
                stop_reason = Some(map_stop_reason(fr));
            }

            // Text delta → (re)open a text block, then emit a text_delta.
            if let Some(text) = choice.delta.content.as_deref() {
                if !text.is_empty() {
                    if !matches!(cur, Cur::Text(_)) {
                        if let Some(aidx) = cur.aidx() {
                            yield ev("content_block_stop", json!({"type":"content_block_stop","index":aidx}));
                        }
                        let aidx = next_index;
                        next_index += 1;
                        cur = Cur::Text(aidx);
                        yield ev("content_block_start", json!({
                            "type":"content_block_start","index":aidx,
                            "content_block":{"type":"text","text":""},
                        }));
                    }
                    let aidx = cur.aidx().unwrap();
                    yield ev("content_block_delta", json!({
                        "type":"content_block_delta","index":aidx,
                        "delta":{"type":"text_delta","text":text},
                    }));
                }
            }

            // Tool-call fragments → tool_use block start + input_json_delta.
            for frag in &choice.delta.tool_calls {
                saw_tool = true;
                let ti = frag.index;
                let is_current = matches!(cur, Cur::Tool { internal, .. } if internal == ti);
                if !is_current {
                    if let Some(aidx) = cur.aidx() {
                        yield ev("content_block_stop", json!({"type":"content_block_stop","index":aidx}));
                    }
                    let aidx = next_index;
                    next_index += 1;
                    cur = Cur::Tool { internal: ti, aidx };
                    let name = frag.function.as_ref().and_then(|f| f.name.clone()).unwrap_or_default();
                    let id = frag.id.clone().unwrap_or_default();
                    yield ev("content_block_start", json!({
                        "type":"content_block_start","index":aidx,
                        "content_block":{"type":"tool_use","id":id,"name":name,"input":{}},
                    }));
                }
                if let Some(args) = frag.function.as_ref().and_then(|f| f.arguments.clone()) {
                    if !args.is_empty() {
                        let aidx = cur.aidx().unwrap();
                        yield ev("content_block_delta", json!({
                            "type":"content_block_delta","index":aidx,
                            "delta":{"type":"input_json_delta","partial_json":args},
                        }));
                    }
                }
            }
        }

        if let Some(aidx) = cur.aidx() {
            yield ev("content_block_stop", json!({"type":"content_block_stop","index":aidx}));
        }
        if started {
            let sr = stop_reason.unwrap_or_else(|| if saw_tool { "tool_use".to_string() } else { "end_turn".to_string() });
            yield ev("message_delta", json!({
                "type":"message_delta",
                "delta":{"stop_reason":sr,"stop_sequence":null},
                "usage":{"output_tokens":output_tokens},
            }));
            yield ev("message_stop", json!({"type":"message_stop"}));
        }
    };
    Sse::new(stream).into_response()
}

// ---- Handler ----

/// `POST /v1/messages` — Anthropic Messages ingress.
pub async fn messages(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(areq): Json<AnthropicMessagesRequest>,
) -> Response {
    let mut ctx = Ctx::new();
    ctx.virtual_key = crate::handlers::vkey_from_headers(&headers);
    // Group into a session: `x-session-id` header wins, else Claude Code's
    // `metadata.user_id` (from which we key on the `session_<uuid>` segment).
    let session_hint = areq
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_deref())
        .and_then(derive_session_id);
    ctx.session_id = crate::handlers::session_id_from(&headers, session_hint.as_deref());
    crate::otel::apply_trace_context(&mut ctx, &headers);

    let stream = areq.stream.unwrap_or(false);
    let req = to_chat_request(areq);
    let engine = state.engine.load_full();

    if stream {
        match engine.chat_stream(&mut ctx, req).await {
            Ok(inner) => anthropic_sse(inner),
            Err(e) => anthropic_error_response(e),
        }
    } else {
        match engine.chat(&mut ctx, req).await {
            Ok(resp) => Json(to_anthropic_response(resp)).into_response(),
            Err(e) => anthropic_error_response(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(v: Value) -> AnthropicMessagesRequest {
        serde_json::from_value(v).expect("valid request")
    }

    #[test]
    fn translates_system_and_text_messages() {
        let cr = to_chat_request(req(json!({
            "model": "zai/glm-4.6",
            "system": "You are helpful.",
            "max_tokens": 100,
            "messages": [{ "role": "user", "content": "hi" }],
        })));
        assert_eq!(cr.model, "zai/glm-4.6");
        assert_eq!(cr.max_tokens, Some(100));
        assert_eq!(cr.messages.len(), 2);
        assert_eq!(cr.messages[0].role, Role::System);
        assert_eq!(cr.messages[1].role, Role::User);
        assert_eq!(cr.messages[1].text_content(), Some("hi"));
    }

    /// Claude Code emits a mid-conversation `"role": "system"` turn alongside the top-level
    /// `system` field; it must stay a system turn, not silently become a user turn.
    #[test]
    fn mid_conversation_system_role_stays_system() {
        for content in [json!("ctx"), json!([{ "type": "text", "text": "ctx" }])] {
            let cr = to_chat_request(req(json!({
                "model": "zai/glm-4.6",
                "max_tokens": 100,
                "messages": [
                    { "role": "user", "content": "hi" },
                    { "role": "system", "content": content },
                ],
            })));
            assert_eq!(cr.messages.len(), 2);
            assert_eq!(cr.messages[0].role, Role::User);
            assert_eq!(cr.messages[1].role, Role::System);
            assert_eq!(cr.messages[1].text_content(), Some("ctx"));
        }
    }

    #[test]
    fn translates_tools_and_tool_use_and_tool_result() {
        let cr = to_chat_request(req(json!({
            "model": "zai/glm-4.6",
            "max_tokens": 100,
            "tools": [{ "name": "get_weather", "description": "w", "input_schema": {"type":"object"} }],
            "messages": [
                { "role": "user", "content": "weather?" },
                { "role": "assistant", "content": [
                    { "type": "text", "text": "let me check" },
                    { "type": "tool_use", "id": "call_1", "name": "get_weather", "input": {"loc":"SF"} }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "call_1", "content": "sunny" }
                ]}
            ],
        })));
        assert_eq!(cr.tools.len(), 1);
        assert_eq!(cr.tools[0].function.name, "get_weather");
        // user, assistant(text+tool_call), tool
        assert_eq!(cr.messages.len(), 3);
        let asst = &cr.messages[1];
        assert_eq!(asst.role, Role::Assistant);
        assert_eq!(asst.text_content(), Some("let me check"));
        assert_eq!(asst.tool_calls.len(), 1);
        assert_eq!(asst.tool_calls[0].id, "call_1");
        assert_eq!(asst.tool_calls[0].function.arguments, r#"{"loc":"SF"}"#);
        let tool = &cr.messages[2];
        assert_eq!(tool.role, Role::Tool);
        assert_eq!(tool.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(tool.text_content(), Some("sunny"));
    }

    #[test]
    fn response_with_tool_use_maps_stop_reason() {
        use kgateway_core::schema::{Choice, Usage};
        let resp = ChatResponse {
            id: "msg_1".into(),
            object: "chat.completion".into(),
            model: "zai/glm-4.6".into(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: Some("calling".into()),
                    name: None,
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "f".into(),
                            arguments: r#"{"a":1}"#.into(),
                        },
                    }],
                    tool_call_id: None,
                },
                finish_reason: Some("tool_calls".into()),
            }],
            usage: Usage {
                prompt_tokens: 5,
                completion_tokens: 7,
                total_tokens: 12,
            },
        };
        let av = to_anthropic_response(resp);
        assert_eq!(av["stop_reason"], "tool_use");
        assert_eq!(av["content"][0]["type"], "text");
        assert_eq!(av["content"][1]["type"], "tool_use");
        assert_eq!(av["content"][1]["input"]["a"], 1);
        assert_eq!(av["usage"]["input_tokens"], 5);
        assert_eq!(av["usage"]["output_tokens"], 7);
    }
}
