//! Native Google Gemini `generateContent` API provider.
//!
//! Gemini's wire format differs from OpenAI's in several ways that this adapter
//! reconciles against the internal (OpenAI-shaped) schema:
//! - auth via the `x-goog-api-key` header (not bearer);
//! - the model id is part of the URL path (`/models/{model}:generateContent`), not
//!   the request body;
//! - there is no "system" role in the turn array — instead a top-level
//!   `systemInstruction` object carries the system prompt;
//! - turns use `"user"` / `"model"` roles (NOT `"assistant"`);
//! - `temperature` / `max_tokens` live under a nested `generationConfig` object;
//! - responses return `candidates[].content.parts[].text` instead of `choices[].message`.
//!
//! Streaming (`:streamGenerateContent?alt=sse`) is a noted follow-on — not implemented
//! here. `chat_stream` returns `KgError::unsupported`.

use async_trait::async_trait;
use kgateway_core::context::Ctx;
use kgateway_core::error::{KgError, KgErrorKind};
use kgateway_core::provider::{ApiKey, ChunkStream, Provider, ProviderKey};
use kgateway_core::schema::{
    ChatRequest, ChatResponse, Choice, Message, MessageContent, Role, Usage,
};
use serde::{Deserialize, Serialize};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

pub struct GeminiProvider {
    key: ProviderKey,
    base_url: String,
    client: reqwest::Client,
}

impl GeminiProvider {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            key: ProviderKey::new("gemini"),
            base_url: base_url.into(),
            client: crate::http::default_client(),
        }
    }

    /// Register a Gemini-compatible provider under a custom name + base URL.
    pub fn with_identity(key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            key: ProviderKey::new(key),
            base_url: base_url.into(),
            client: crate::http::default_client(),
        }
    }

    /// Convert an internal [`ChatRequest`] into a Gemini `generateContent` request body.
    fn body(&self, req: &ChatRequest) -> GeminiRequest {
        // Gemini takes the system prompt as a top-level `systemInstruction` object;
        // pull every Role::System message out of the array and join their contents
        // into the instruction's parts.
        let mut system_parts: Vec<GeminiPart> = Vec::new();
        let mut contents: Vec<GeminiContent> = Vec::new();

        for m in &req.messages {
            match m.role {
                Role::System => {
                    if let Some(t) = m.content.as_ref().and_then(|c| c.to_text()) {
                        system_parts.push(GeminiPart { text: t });
                    }
                }
                // Gemini has no "system" or "tool" role in the turn array; tool
                // results map to a user turn for now (keep it simple until
                // tool-use lands).
                Role::User | Role::Tool => contents.push(GeminiContent {
                    role: "user".into(),
                    parts: vec![GeminiPart {
                        text: m.text_or_empty(),
                    }],
                }),
                // Gemini's assistant-equivalent role is "model", not "assistant".
                Role::Assistant => contents.push(GeminiContent {
                    role: "model".into(),
                    parts: vec![GeminiPart {
                        text: m.text_or_empty(),
                    }],
                }),
            }
        }

        let system_instruction = if system_parts.is_empty() {
            None
        } else {
            Some(GeminiSystemInstruction {
                parts: system_parts,
            })
        };

        let generation_config = if req.max_tokens.is_none() && req.temperature.is_none() {
            None
        } else {
            Some(GeminiGenerationConfig {
                max_output_tokens: req.max_tokens,
                temperature: req.temperature,
            })
        };

        GeminiRequest {
            contents,
            system_instruction,
            generation_config,
        }
    }

    fn url(&self, model: &str) -> String {
        format!("{}/models/{}:generateContent", self.base_url, model)
    }
}

impl Default for GeminiProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    fn key(&self) -> ProviderKey {
        self.key.clone()
    }

    async fn chat(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: ChatRequest,
    ) -> Result<ChatResponse, KgError> {
        let model = req.model_id().to_string();
        let body = self.body(&req);

        let resp = self
            .client
            .post(self.url(&model))
            .header("x-goog-api-key", &key.value)
            .timeout(crate::http::REQUEST_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(net_err)?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(KgError::provider(text, status.as_u16()).with_provider(self.key.as_str()));
        }

        let gr: GeminiResponse = resp
            .json()
            .await
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))?;

        Ok(gr.into_chat_response(model))
    }

    async fn chat_stream(
        &self,
        _ctx: &Ctx,
        _key: &ApiKey,
        _req: ChatRequest,
    ) -> Result<ChunkStream, KgError> {
        // Follow-on: Gemini supports streaming via
        // `:streamGenerateContent?alt=sse`, which emits a sequence of partial
        // `GeminiResponse`-shaped JSON objects as SSE `data:` frames. Not
        // implemented yet; focus here is non-streaming correctness.
        Err(KgError::unsupported("gemini streaming"))
    }
}

fn net_err(e: reqwest::Error) -> KgError {
    KgError::new(KgErrorKind::Network, e.to_string()).with_retryable(true)
}

// ---- Gemini wire types ----

#[derive(Debug, Serialize)]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystemInstruction>,
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
}

#[derive(Debug, Serialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiPart {
    text: String,
}

#[derive(Debug, Serialize)]
struct GeminiSystemInstruction {
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize)]
struct GeminiGenerationConfig {
    #[serde(rename = "maxOutputTokens", skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Debug, Default, Deserialize)]
struct GeminiResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(default, rename = "usageMetadata")]
    usage_metadata: GeminiUsageMetadata,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    #[serde(default)]
    content: Option<GeminiResponseContent>,
    #[serde(default, rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiResponseContent {
    #[serde(default)]
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Default, Deserialize)]
struct GeminiUsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    prompt_token_count: u32,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates_token_count: u32,
    #[serde(default, rename = "totalTokenCount")]
    total_token_count: u32,
}

impl GeminiResponse {
    fn into_chat_response(self, model: String) -> ChatResponse {
        let first = self.candidates.first();

        // Concatenate all text parts of the first candidate into a single
        // assistant message.
        let text: String = first
            .and_then(|c| c.content.as_ref())
            .map(|c| c.parts.iter().map(|p| p.text.as_str()).collect())
            .unwrap_or_default();

        let content = if text.is_empty() { None } else { Some(text) };
        let finish_reason = first.and_then(|c| c.finish_reason.clone());

        let usage = Usage {
            prompt_tokens: self.usage_metadata.prompt_token_count,
            completion_tokens: self.usage_metadata.candidates_token_count,
            total_tokens: self.usage_metadata.total_token_count,
        };

        ChatResponse {
            id: String::new(),
            object: "chat.completion".to_string(),
            model,
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: content.map(MessageContent::Text),
                    name: None,
                    tool_calls: vec![],
                    tool_call_id: None,
                },
                finish_reason,
            }],
            usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgateway_core::context::Ctx;
    use kgateway_core::provider::ApiKey;
    use kgateway_core::schema::Message;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_key() -> ApiKey {
        ApiKey {
            id: "test".into(),
            value: "goog-test-key".into(),
            weight: 1,
            models: vec![],
        }
    }

    fn req_with(messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: "gemini/gemini-1.5-pro".into(),
            messages,
            temperature: Some(0.5),
            max_tokens: Some(256),
            ..Default::default()
        }
    }

    #[test]
    fn body_maps_roles_and_lifts_system() {
        let p = GeminiProvider::new();
        let req = req_with(vec![
            Message::system("be terse"),
            Message::user("hi"),
            Message {
                role: Role::Assistant,
                content: Some("hello!".into()),
                name: None,
                tool_calls: vec![],
                tool_call_id: None,
            },
        ]);
        let body = p.body(&req);

        assert_eq!(
            body.system_instruction.as_ref().unwrap().parts[0].text,
            "be terse"
        );
        assert_eq!(body.contents.len(), 2);
        assert_eq!(body.contents[0].role, "user");
        assert_eq!(body.contents[1].role, "model");
        let gc = body.generation_config.unwrap();
        assert_eq!(gc.max_output_tokens, Some(256));
        assert_eq!(gc.temperature, Some(0.5));
    }

    #[tokio::test]
    async fn chat_converts_response_and_sends_wire_format() {
        let server = MockServer::start().await;
        let resp_body = serde_json::json!({
            "candidates": [
                {
                    "content": {
                        "parts": [{"text": "Hello, "}, {"text": "world!"}],
                        "role": "model"
                    },
                    "finishReason": "STOP"
                }
            ],
            "usageMetadata": {
                "promptTokenCount": 12,
                "candidatesTokenCount": 5,
                "totalTokenCount": 17
            }
        });

        Mock::given(method("POST"))
            .and(path("/models/gemini-1.5-pro:generateContent"))
            .and(header("x-goog-api-key", "goog-test-key"))
            .and(body_partial_json(serde_json::json!({
                "contents": [
                    {"role": "user", "parts": [{"text": "hi"}]},
                    {"role": "model", "parts": [{"text": "hello!"}]}
                ],
                "systemInstruction": {"parts": [{"text": "be terse"}]}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(resp_body))
            .expect(1)
            .mount(&server)
            .await;

        let p = GeminiProvider::with_base_url(server.uri());
        let ctx = Ctx::default();
        let req = req_with(vec![
            Message::system("be terse"),
            Message::user("hi"),
            Message {
                role: Role::Assistant,
                content: Some("hello!".into()),
                name: None,
                tool_calls: vec![],
                tool_call_id: None,
            },
        ]);
        let out = p.chat(&ctx, &test_key(), req).await.expect("chat ok");

        assert_eq!(out.object, "chat.completion");
        assert_eq!(out.model, "gemini-1.5-pro");
        assert_eq!(out.choices.len(), 1);
        assert_eq!(out.choices[0].message.text_content(), Some("Hello, world!"));
        assert_eq!(out.choices[0].message.role, Role::Assistant);
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("STOP"));
        assert_eq!(out.usage.prompt_tokens, 12);
        assert_eq!(out.usage.completion_tokens, 5);
        assert_eq!(out.usage.total_tokens, 17);
    }

    #[tokio::test]
    async fn chat_error_429_is_retryable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/gemini-1.5-pro:generateContent"))
            .respond_with(
                ResponseTemplate::new(429).set_body_string("{\"error\":\"rate limited\"}"),
            )
            .mount(&server)
            .await;

        let p = GeminiProvider::with_base_url(server.uri());
        let ctx = Ctx::default();
        let req = req_with(vec![Message::user("hi")]);
        let err = p
            .chat(&ctx, &test_key(), req)
            .await
            .expect_err("should error");
        assert!(err.is_retryable(), "429 must be retryable");
        assert_eq!(err.status, Some(429));
        assert_eq!(err.provider.as_deref(), Some("gemini"));
    }

    #[tokio::test]
    async fn chat_stream_is_unsupported() {
        let p = GeminiProvider::new();
        let ctx = Ctx::default();
        let req = req_with(vec![Message::user("hi")]);
        let result = p.chat_stream(&ctx, &test_key(), req).await;
        let err = match result {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        };
        assert_eq!(err.kind, KgErrorKind::Unsupported);
        assert!(err.message.contains("gemini streaming"));
    }
}
