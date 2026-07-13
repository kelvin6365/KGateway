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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            name: None,
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            name: None,
            tool_calls: vec![],
            tool_call_id: None,
        }
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
}
