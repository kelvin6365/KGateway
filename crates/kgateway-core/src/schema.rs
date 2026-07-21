//! Internal, provider-neutral request/response schema. Wire-compatible with the
//! OpenAI Chat Completions API (the lingua franca), so it doubles as the client
//! contract. Provider adapters convert to/from their native formats.
//!
//! This is the trimmed M1 subset of the full schema. Fields are added as capabilities
//! land (tools, images, audio, ...). Keep additive & backwards compatible.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Polymorphic message content. On the wire, `content` is either a plain string
/// (text-only) or an array of content parts (multimodal: text, image_url, etc.).
/// This mirrors the OpenAI Chat Completions API exactly.
///
/// Serialization rule:
/// - `Text(s)` → `"s"` (a JSON string)
/// - `Parts([...])` → `[{...}, {...}]` (a JSON array)
///
/// Deserialization accepts both shapes. A `null` content deserializes to `None`
/// at the `Message` level.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// Plain text — the common case. Serializes as a JSON string.
    Text(String),
    /// Array of typed content parts (text, image_url, etc.).
    /// Serializes as a JSON array.
    Parts(Vec<ContentPart>),
}

impl MessageContent {
    /// Extract the plain-text value if this is a `Text` variant.
    /// Returns `None` for `Parts` (multimodal content has no single text value).
    pub fn as_text(&self) -> Option<&str> {
        match self {
            MessageContent::Text(s) => Some(s),
            MessageContent::Parts(_) => None,
        }
    }

    /// Extract text from any content shape: `Text(s)` returns `s`, `Parts` returns
    /// the concatenation of all `text`-typed parts. Useful for logging, caching,
    /// and other places that need a best-effort string representation.
    pub fn to_text(&self) -> Option<String> {
        match self {
            MessageContent::Text(s) => Some(s.clone()),
            MessageContent::Parts(parts) => {
                let texts: Vec<&str> = parts
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                if texts.is_empty() {
                    None
                } else {
                    Some(texts.join(""))
                }
            }
        }
    }
}

impl From<String> for MessageContent {
    fn from(s: String) -> Self {
        MessageContent::Text(s)
    }
}

impl From<&str> for MessageContent {
    fn from(s: &str) -> Self {
        MessageContent::Text(s.to_string())
    }
}

/// One part of a multipart message content. Supports text and image_url
/// (the two OpenAI content part types). Unknown part types are preserved
/// via the `Other` variant so they survive the gateway's deserialize→reserialize
/// round-trip without being dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    ImageUrl {
        image_url: ImageUrl,
    },
    /// Any content part type we don't explicitly model (e.g. `input_audio`,
    /// `file`, proprietary extensions). Preserved verbatim.
    #[serde(other)]
    Other,
}

/// OpenAI `image_url` content part shape. The `url` can be a data URI
/// (`data:image/png;base64,...`) or a public HTTPS URL. `detail` is optional
/// (`low` | `high` | `auto`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    /// Either a public URL or a `data:` URI with base64-encoded image data.
    pub url: String,
    /// Optional fidelity hint: `"low"`, `"high"`, or `"auto"` (default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn user(content: impl Into<MessageContent>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            name: None,
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    pub fn system(content: impl Into<MessageContent>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            name: None,
            tool_calls: vec![],
            tool_call_id: None,
        }
    }

    /// Convenience: get the content as a plain text string if it is text-only.
    /// Returns `None` for multipart/multimodal content.
    pub fn text_content(&self) -> Option<&str> {
        self.content.as_ref()?.as_text()
    }

    /// Best-effort text extraction from any content shape.
    pub fn text_or_empty(&self) -> String {
        self.content
            .as_ref()
            .and_then(|c| c.to_text())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String, // "function"
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// JSON-encoded arguments string (OpenAI convention).
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    #[serde(rename = "type")]
    pub kind: String, // "function"
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

/// A chat completion request in the internal schema.
///
/// Beyond the explicitly-modeled sampling/control params, an `extra` passthrough (flattened
/// on the wire, a passthrough-params convention) forwards any client-supplied field we don't
/// model — so params never silently vanish before reaching the provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// OpenAI o-series / reasoning models use `max_completion_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    /// `stop` may be a string or an array — kept as raw JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u32>,
    /// JSON mode / structured-output schema (`{"type":"json_object"}` etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// e.g. `{"include_usage": true}` — needed to get token usage on streamed responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Fallback provider/model chain; consumed by the router, not sent upstream.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<Fallback>,
    /// Passthrough for any request field not modeled above. Flattened on the wire so
    /// client-supplied params survive to the provider instead of being dropped.
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fallback {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: Message,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub id: String,
    #[serde(default = "default_object")]
    pub object: String,
    pub model: String,
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Usage,
}

fn default_object() -> String {
    "chat.completion".to_string()
}

// ---- Streaming ----

/// One SSE chunk in the internal schema (OpenAI `chat.completion.chunk` shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunk {
    pub id: String,
    #[serde(default = "default_chunk_object")]
    pub object: String,
    pub model: String,
    pub choices: Vec<StreamChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

fn default_chunk_object() -> String {
    "chat.completion.chunk".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChoice {
    pub index: u32,
    pub delta: Delta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Delta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Streamed tool-call fragments. Providers emit a tool call incrementally across chunks
    /// (id + name in the first fragment, `arguments` streamed after), keyed by `index`.
    /// Modeled here so the fragments survive the gateway's deserialize→reserialize round-trip
    /// instead of being dropped; [`ToolCallAccumulator`] reassembles them into `ToolCall`s.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallDelta>,
}

/// One streamed tool-call fragment (OpenAI `delta.tool_calls[]` shape). Every field except
/// `index` is optional because it arrives incrementally.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolCallDelta {
    /// Position of this tool call within the message — the key that ties fragments together.
    pub index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionCallDelta>,
}

/// Streamed function-call fragment: `name` in the first fragment, `arguments` accreted across
/// subsequent ones.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FunctionCallDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

/// Reassembles streamed [`ToolCallDelta`] fragments into complete [`ToolCall`]s. Fragments are
/// merged by `index`: the first non-empty `id`/`type`/`name` wins, and `arguments` strings are
/// concatenated in arrival order. Call [`push`](Self::push) for each streamed [`Delta`], then
/// [`finish`](Self::finish) at end-of-stream (or on `finish_reason == "tool_calls"`).
#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    /// (index -> partial tool call), kept insertion-ordered by first-seen index.
    calls: Vec<(u32, ToolCall)>,
}

impl ToolCallAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Merge one delta's tool-call fragments into the accumulator.
    pub fn push(&mut self, delta: &Delta) {
        for frag in &delta.tool_calls {
            let entry = match self.calls.iter_mut().find(|(i, _)| *i == frag.index) {
                Some((_, tc)) => tc,
                None => {
                    self.calls.push((
                        frag.index,
                        ToolCall {
                            id: String::new(),
                            kind: "function".to_string(),
                            function: FunctionCall {
                                name: String::new(),
                                arguments: String::new(),
                            },
                        },
                    ));
                    &mut self.calls.last_mut().unwrap().1
                }
            };
            if let Some(id) = &frag.id {
                if entry.id.is_empty() {
                    entry.id = id.clone();
                }
            }
            if let Some(kind) = &frag.kind {
                entry.kind = kind.clone();
            }
            if let Some(f) = &frag.function {
                if let Some(name) = &f.name {
                    if entry.function.name.is_empty() {
                        entry.function.name = name.clone();
                    }
                }
                if let Some(args) = &f.arguments {
                    entry.function.arguments.push_str(args);
                }
            }
        }
    }

    /// Whether any tool-call fragment has been seen.
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// The assembled tool calls, ordered by `index`.
    pub fn finish(mut self) -> Vec<ToolCall> {
        self.calls.sort_by_key(|(i, _)| *i);
        self.calls.into_iter().map(|(_, tc)| tc).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deserialize a streamed `delta.tool_calls` frame from the OpenAI wire shape.
    fn delta_from_json(json: &str) -> Delta {
        serde_json::from_str(json).expect("delta parses")
    }

    #[test]
    fn tool_call_delta_survives_wire_roundtrip() {
        // A first fragment (id + name + type + empty args) must deserialize AND re-serialize
        // without dropping the tool_calls — the bug this field fixes.
        let d = delta_from_json(
            r#"{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]}"#,
        );
        assert_eq!(d.tool_calls.len(), 1);
        assert_eq!(d.tool_calls[0].id.as_deref(), Some("call_1"));
        let out = serde_json::to_string(&d).unwrap();
        assert!(
            out.contains("\"tool_calls\""),
            "must forward tool_calls: {out}"
        );
        assert!(out.contains("get_weather"));
    }

    #[test]
    fn accumulator_assembles_streamed_tool_call() {
        // id + name arrive first, then arguments stream across chunks.
        let frames = [
            r#"{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]}"#,
            r#"{"tool_calls":[{"index":0,"function":{"arguments":"{\"loc"}}]}"#,
            r#"{"tool_calls":[{"index":0,"function":{"arguments":"ation\":\"SF\"}"}}]}"#,
        ];
        let mut acc = ToolCallAccumulator::new();
        assert!(acc.is_empty());
        for f in frames {
            acc.push(&delta_from_json(f));
        }
        assert!(!acc.is_empty());
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].kind, "function");
        assert_eq!(calls[0].function.name, "get_weather");
        assert_eq!(calls[0].function.arguments, r#"{"location":"SF"}"#);
    }

    #[test]
    fn accumulator_handles_parallel_tool_calls_by_index() {
        // Two interleaved tool calls, distinguished only by `index`, assembled independently
        // and returned in index order.
        let frames = [
            r#"{"tool_calls":[{"index":0,"id":"a","function":{"name":"f0","arguments":"x"}}]}"#,
            r#"{"tool_calls":[{"index":1,"id":"b","function":{"name":"f1","arguments":"y"}}]}"#,
            r#"{"tool_calls":[{"index":0,"function":{"arguments":"0"}}]}"#,
            r#"{"tool_calls":[{"index":1,"function":{"arguments":"1"}}]}"#,
        ];
        let mut acc = ToolCallAccumulator::new();
        for f in frames {
            acc.push(&delta_from_json(f));
        }
        let calls = acc.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            (calls[0].id.as_str(), calls[0].function.arguments.as_str()),
            ("a", "x0")
        );
        assert_eq!(
            (calls[1].id.as_str(), calls[1].function.arguments.as_str()),
            ("b", "y1")
        );
    }

    #[test]
    fn plain_content_delta_has_no_tool_calls() {
        let d = delta_from_json(r#"{"content":"hello"}"#);
        assert!(d.tool_calls.is_empty());
        // And a content-only delta must not emit an empty tool_calls array on the wire.
        let out = serde_json::to_string(&d).unwrap();
        assert!(
            !out.contains("tool_calls"),
            "no empty array on the wire: {out}"
        );
    }

    // ---- Multimodal content tests ----

    #[test]
    fn plain_string_content_deserializes() {
        let msg: Message = serde_json::from_str(r#"{"role":"user","content":"Hello world"}"#)
            .expect("plain string content parses");
        assert_eq!(msg.text_content(), Some("Hello world"));
        // Round-trip: should serialize back as a string, not an array.
        let out = serde_json::to_string(&msg).unwrap();
        assert!(
            out.contains("\"content\":\"Hello world\""),
            "should serialize as string: {out}"
        );
    }

    #[test]
    fn multipart_text_content_deserializes() {
        let msg: Message =
            serde_json::from_str(r#"{"role":"user","content":[{"type":"text","text":"Hello"}]}"#)
                .expect("multipart text content parses");
        // to_text should extract the text
        assert_eq!(
            msg.content.as_ref().unwrap().to_text().as_deref(),
            Some("Hello")
        );
        // text_content returns None because it's Parts, not Text
        assert_eq!(msg.text_content(), None);
    }

    #[test]
    fn image_url_content_deserializes() {
        let msg: Message = serde_json::from_str(
            r#"{"role":"user","content":[
                {"type":"text","text":"What's in this image?"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,iVBOR...","detail":"high"}}
            ]}"#,
        )
        .expect("image_url content parses");

        // Verify it round-trips through the gateway's serialize→deserialize cycle
        let wire = serde_json::to_string(&msg).unwrap();
        let msg2: Message = serde_json::from_str(&wire).unwrap();

        // Text extraction should return just the text parts
        assert_eq!(
            msg2.content.as_ref().unwrap().to_text().as_deref(),
            Some("What's in this image?")
        );
    }

    #[test]
    fn null_content_deserializes_as_none() {
        let msg: Message =
            serde_json::from_str(r#"{"role":"assistant","content":null}"#).expect("null content");
        assert!(msg.content.is_none());
    }

    #[test]
    fn absent_content_defaults_to_none() {
        let msg: Message = serde_json::from_str(r#"{"role":"assistant"}"#).expect("absent content");
        assert!(msg.content.is_none());
    }

    #[test]
    fn multimodal_message_survives_full_roundtrip() {
        let json = r#"{
            "role": "user",
            "content": [
                {"type": "text", "text": "Describe this"},
                {"type": "image_url", "image_url": {"url": "https://example.com/img.png"}}
            ]
        }"#;
        let msg: Message = serde_json::from_str(json).expect("parses");
        let wire = serde_json::to_string(&msg).expect("serializes");
        assert!(wire.contains("image_url"), "image_url must survive: {wire}");
        assert!(wire.contains("example.com"), "url must survive: {wire}");
    }

    #[test]
    fn unknown_content_part_preserved() {
        // A content part type we don't model should survive the round-trip via the Other variant
        let json = r#"{"role":"user","content":[
            {"type":"text","text":"hello"},
            {"type":"input_audio","input_audio":{"data":"base64...","format":"wav"}}
        ]}"#;
        let msg: Message = serde_json::from_str(json).expect("parses with unknown part");
        // Text extraction should still get the text part
        assert_eq!(
            msg.content.as_ref().unwrap().to_text().as_deref(),
            Some("hello")
        );
    }
}
