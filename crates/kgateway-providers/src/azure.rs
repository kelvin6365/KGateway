//! Azure OpenAI provider — OpenAI chat wire-compatible but with deployment-based URLs,
//! api-version query parameter, and api-key header authentication.

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use kgateway_core::context::Ctx;
use kgateway_core::error::{KgError, KgErrorKind};
use kgateway_core::provider::{ApiKey, ChunkStream, Provider, ProviderKey};
use kgateway_core::schema::{ChatRequest, ChatResponse, StreamChunk};

const DEFAULT_API_VERSION: &str = "2024-06-01";

pub struct AzureProvider {
    key: ProviderKey,
    base_url: String,
    client: reqwest::Client,
}

impl AzureProvider {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_identity("azure", base_url)
    }

    /// Initialize with a custom provider key and base URL.
    pub fn with_identity(name: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            key: ProviderKey::new(name),
            base_url: base_url.into(),
            client: crate::http::default_client(),
        }
    }

    /// Build the upstream JSON body: strip gateway-only fields, use the bare model id.
    /// Azure ignores the body `model` field (deployment is in the URL) but we send it anyway.
    fn body(&self, req: &ChatRequest, stream: bool) -> Result<serde_json::Value, KgError> {
        let mut v = serde_json::to_value(req).map_err(|e| {
            KgError::new(KgErrorKind::Internal, format!("request encode error: {e}"))
        })?;
        if let Some(obj) = v.as_object_mut() {
            obj.remove("fallbacks"); // gateway-only
            obj.insert(
                "model".into(),
                serde_json::Value::String(req.model_id().to_string()),
            );
            obj.insert("stream".into(), serde_json::Value::Bool(stream));
        }
        Ok(v)
    }
}

#[async_trait]
impl Provider for AzureProvider {
    fn key(&self) -> ProviderKey {
        self.key.clone()
    }

    async fn chat(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: ChatRequest,
    ) -> Result<ChatResponse, KgError> {
        let deployment = req.model_id();
        let url = format!(
            "{}/openai/deployments/{}/chat/completions?api-version={}",
            self.base_url, deployment, DEFAULT_API_VERSION
        );
        let resp = self
            .client
            .post(&url)
            .header("api-key", &key.value)
            .timeout(crate::http::REQUEST_TIMEOUT)
            .json(&self.body(&req, false)?)
            .send()
            .await
            .map_err(net_err)?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(KgError::provider(text, status.as_u16()).with_provider(self.key.as_str()));
        }

        resp.json::<ChatResponse>()
            .await
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))
    }

    async fn chat_stream(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: ChatRequest,
    ) -> Result<ChunkStream, KgError> {
        let deployment = req.model_id();
        let url = format!(
            "{}/openai/deployments/{}/chat/completions?api-version={}",
            self.base_url, deployment, DEFAULT_API_VERSION
        );
        let resp = self
            .client
            .post(&url)
            .header("api-key", &key.value)
            .json(&self.body(&req, true)?)
            .send()
            .await
            .map_err(net_err)?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(KgError::provider(text, status.as_u16()).with_provider(self.key.as_str()));
        }

        let stream = sse_to_chunks(resp.bytes_stream());
        Ok(stream.boxed())
    }
}

fn net_err(e: reqwest::Error) -> KgError {
    KgError::new(KgErrorKind::Network, e.to_string()).with_retryable(true)
}

/// Parse an Azure OpenAI SSE byte stream into `StreamChunk`s. Uses the same logic as OpenAI
/// since Azure streams the same `chat.completion.chunk` SSE frames.
fn sse_to_chunks(
    byte_stream: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
) -> BoxStream<'static, Result<StreamChunk, KgError>> {
    let s = async_stream::stream! {
        futures::pin_mut!(byte_stream);
        let mut buf: Vec<u8> = Vec::new();
        while let Some(item) = byte_stream.next().await {
            let bytes = match item {
                Ok(b) => b,
                Err(e) => { yield Err(net_err(e)); return; }
            };
            buf.extend_from_slice(&bytes);

            while let Some(pos) = find_subslice(&buf, b"\n\n") {
                let frame_bytes: Vec<u8> = buf.drain(..pos + 2).collect();
                let frame = String::from_utf8_lossy(&frame_bytes);
                for line in frame.lines() {
                    let Some(data) = line.trim_start().strip_prefix("data:") else { continue };
                    let data = data.trim();
                    if data == "[DONE]" { return; }
                    match serde_json::from_str::<StreamChunk>(data) {
                        Ok(chunk) => yield Ok(chunk),
                        Err(e) => yield Err(KgError::new(
                            KgErrorKind::Internal,
                            format!("stream decode error: {e}"),
                        )),
                    }
                }
            }
        }
    };
    s.boxed()
}

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

    #[tokio::test]
    async fn chat_sends_deployment_in_url_and_api_key_header() {
        use kgateway_core::provider::ApiKey;
        use wiremock::matchers::{header, method, path_regex, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // Mock the Azure OpenAI endpoint.
        Mock::given(method("POST"))
            .and(path_regex(r"/openai/deployments/gpt-4o/chat/completions"))
            .and(query_param("api-version", DEFAULT_API_VERSION))
            .and(header("api-key", "test-key-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-abc123",
                "object": "chat.completion",
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Hello from Azure!"
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 5,
                    "total_tokens": 15
                }
            })))
            .mount(&server)
            .await;

        let provider = AzureProvider::new(server.uri());
        let ctx = Ctx::new();
        let key = ApiKey {
            id: "azure-key".into(),
            value: "test-key-123".into(),
            weight: 1,
            models: vec![],
        };

        let req = ChatRequest {
            model: "azure/gpt-4o".into(),
            messages: vec![kgateway_core::schema::Message::user("Hi")],
            ..Default::default()
        };

        let resp = provider
            .chat(&ctx, &key, req)
            .await
            .expect("chat should succeed");

        assert_eq!(resp.id, "chatcmpl-abc123");
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(
            resp.choices[0].message.text_content(),
            Some("Hello from Azure!")
        );
        assert_eq!(resp.usage.total_tokens, 15);
    }

    #[tokio::test]
    async fn chat_stream_accumulates_text_from_sse_frames() {
        use kgateway_core::provider::ApiKey;
        use wiremock::matchers::{header, method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let sse_response = "data: {\"id\":\"a\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}\n\n\
                           data: {\"id\":\"a\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" \"}}]}\n\n\
                           data: {\"id\":\"a\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Azure\"}}]}\n\n\
                           data: [DONE]\n\n";

        Mock::given(method("POST"))
            .and(path_regex(r"/openai/deployments/gpt-4o/chat/completions"))
            .and(header("api-key", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse_response))
            .mount(&server)
            .await;

        let provider = AzureProvider::new(server.uri());
        let ctx = Ctx::new();
        let key = ApiKey {
            id: "k".into(),
            value: "test-key".into(),
            weight: 1,
            models: vec![],
        };

        let req = ChatRequest {
            model: "azure/gpt-4o".into(),
            messages: vec![kgateway_core::schema::Message::user("hi")],
            ..Default::default()
        };

        let mut stream = provider
            .chat_stream(&ctx, &key, req)
            .await
            .expect("stream should start");

        let mut accumulated_text = String::new();
        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.expect("chunk should parse");
            if let Some(content) = &chunk.choices[0].delta.content {
                accumulated_text.push_str(content);
            }
        }

        assert_eq!(accumulated_text, "Hello Azure");
    }

    #[tokio::test]
    async fn chat_stream_handles_frames_split_across_byte_chunks() {
        use kgateway_core::provider::ApiKey;
        use wiremock::matchers::{header, method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // Deliberately split a frame mid-JSON to test byte-buffering.
        let sse_response = "data: {\"id\":\"a\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Spl";
        let sse_part2 = "it\"}}]}\n\ndata: [DONE]\n\n";

        // We can't directly split the response in wiremock, but we can verify the parser
        // handles splits by testing sse_to_chunks directly below. For now, test a complete frame.
        let combined = format!("{}{}", sse_response, sse_part2);

        Mock::given(method("POST"))
            .and(path_regex(r"/openai/deployments/gpt-4o/chat/completions"))
            .and(header("api-key", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_string(combined))
            .mount(&server)
            .await;

        let provider = AzureProvider::new(server.uri());
        let ctx = Ctx::new();
        let key = ApiKey {
            id: "k".into(),
            value: "test-key".into(),
            weight: 1,
            models: vec![],
        };

        let req = ChatRequest {
            model: "azure/gpt-4o".into(),
            messages: vec![kgateway_core::schema::Message::user("hi")],
            ..Default::default()
        };

        let mut stream = provider
            .chat_stream(&ctx, &key, req)
            .await
            .expect("stream should start");

        let mut accumulated_text = String::new();
        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.expect("chunk should parse");
            if let Some(content) = &chunk.choices[0].delta.content {
                accumulated_text.push_str(content);
            }
        }

        assert_eq!(accumulated_text, "Split");
    }

    #[tokio::test]
    async fn sse_parses_frames_split_across_byte_chunks_unit_test() {
        // Direct unit test of sse_to_chunks without HTTP layer.
        // A chunk deliberately split mid-frame.
        let frames = vec![
            Ok::<_, reqwest::Error>(Bytes::from(
                "data: {\"id\":\"a\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel",
            )),
            Ok(Bytes::from(
                "lo\"}}]}\n\ndata: {\"id\":\"a\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"}}]}\n\n",
            )),
            Ok(Bytes::from("data: [DONE]\n\n")),
        ];
        let mut out = sse_to_chunks(stream::iter(frames));
        let mut text = String::new();
        while let Some(item) = out.next().await {
            let chunk = item.expect("chunk should parse");
            if let Some(c) = &chunk.choices[0].delta.content {
                text.push_str(c);
            }
        }
        assert_eq!(text, "Hello world");
    }

    #[tokio::test]
    async fn chat_error_on_401() {
        use kgateway_core::provider::ApiKey;
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path_regex(r"/openai/deployments/.*/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .mount(&server)
            .await;

        let provider = AzureProvider::new(server.uri());
        let ctx = Ctx::new();
        let key = ApiKey {
            id: "k".into(),
            value: "bad-key".into(),
            weight: 1,
            models: vec![],
        };

        let req = ChatRequest {
            model: "azure/gpt-4o".into(),
            messages: vec![kgateway_core::schema::Message::user("hi")],
            ..Default::default()
        };

        let err = provider
            .chat(&ctx, &key, req)
            .await
            .expect_err("should fail with 401");

        // Verify the error is tagged with provider "azure".
        let err_str = format!("{:?}", err);
        assert!(
            err_str.contains("azure"),
            "error should mention azure provider"
        );
    }

    #[test]
    fn body_strips_gateway_fields_and_deployment_only() {
        let p = AzureProvider::new("https://my-resource.openai.azure.com");
        let req = ChatRequest {
            model: "azure/gpt-4o".into(),
            messages: vec![kgateway_core::schema::Message::user("hi")],
            fallbacks: vec![kgateway_core::schema::Fallback {
                provider: "openai".into(),
                model: "gpt-4".into(),
            }],
            ..Default::default()
        };
        let body = p.body(&req, true).unwrap();
        assert_eq!(body["model"], "gpt-4o", "provider prefix stripped");
        assert_eq!(body["stream"], true);
        assert!(
            body.get("fallbacks").is_none(),
            "gateway-only field removed"
        );
    }
}
