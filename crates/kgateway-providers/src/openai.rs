//! OpenAI provider — the reference implementation. The internal schema is already
//! OpenAI-wire-compatible, so conversion is mostly pass-through. Other OpenAI-compatible
//! providers (groq, ollama, ...) reuse this via a thin wrapper in M2.

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use kgateway_core::context::Ctx;
use kgateway_core::error::{KgError, KgErrorKind};
use kgateway_core::provider::{
    ApiKey, Audio, ChunkStream, EmbeddingRequest, EmbeddingResponse, Embeddings, ImageData,
    ImageGenerationRequest, ImageResponse, Images, Provider, ProviderKey, SpeechRequest,
    SpeechResponse, TranscriptionRequest, TranscriptionResponse,
};
use kgateway_core::schema::{ChatRequest, ChatResponse, StreamChunk, Usage};
use serde::Deserialize;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub struct OpenAiProvider {
    key: ProviderKey,
    base_url: String,
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            key: ProviderKey::new("openai"),
            base_url: base_url.into(),
            client: crate::http::default_client(),
        }
    }

    /// Used by the OpenAI-compatible wrapper (M2) to reuse this logic under a
    /// different provider id + base URL.
    pub fn with_identity(key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            key: ProviderKey::new(key),
            base_url: base_url.into(),
            client: crate::http::default_client(),
        }
    }

    /// Build the upstream JSON body: strip gateway-only fields, use the bare model id.
    /// Propagates a serialization failure rather than silently POSTing a `null` body.
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

impl Default for OpenAiProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn key(&self) -> ProviderKey {
        self.key.clone()
    }

    async fn chat(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: ChatRequest,
    ) -> Result<ChatResponse, KgError> {
        let url = format!("{}/chat/completions", self.base_url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&key.value)
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
        let url = format!("{}/chat/completions", self.base_url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&key.value)
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

    fn as_embeddings(&self) -> Option<&dyn Embeddings> {
        Some(self)
    }

    fn as_images(&self) -> Option<&dyn Images> {
        Some(self)
    }

    fn as_audio(&self) -> Option<&dyn Audio> {
        Some(self)
    }
}

// OpenAI /embeddings response shape (also used by all OpenAI-compatible providers).
#[derive(Deserialize)]
struct OaiEmbeddingResponse {
    data: Vec<OaiEmbeddingData>,
    model: String,
    #[serde(default)]
    usage: OaiEmbeddingUsage,
}

#[derive(Deserialize)]
struct OaiEmbeddingData {
    embedding: Vec<f32>,
    index: usize,
}

#[derive(Deserialize, Default)]
struct OaiEmbeddingUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
}

#[async_trait]
impl Embeddings for OpenAiProvider {
    async fn embed(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, KgError> {
        let url = format!("{}/embeddings", self.base_url);
        let body = serde_json::json!({ "model": req.model, "input": req.input });
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&key.value)
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

        let mut parsed: OaiEmbeddingResponse = resp
            .json()
            .await
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))?;
        // Preserve input order regardless of how the provider returns them.
        parsed.data.sort_by_key(|d| d.index);
        Ok(EmbeddingResponse {
            model: parsed.model,
            embeddings: parsed.data.into_iter().map(|d| d.embedding).collect(),
            usage: Usage {
                prompt_tokens: parsed.usage.prompt_tokens,
                completion_tokens: 0,
                total_tokens: parsed.usage.total_tokens,
            },
        })
    }
}

// OpenAI /images/generations response shape.
#[derive(Deserialize)]
struct OaiImageResponse {
    data: Vec<OaiImageData>,
}

#[derive(Deserialize)]
struct OaiImageData {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    b64_json: Option<String>,
}

#[async_trait]
impl Images for OpenAiProvider {
    async fn image_generate(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: ImageGenerationRequest,
    ) -> Result<ImageResponse, KgError> {
        let url = format!("{}/images/generations", self.base_url);
        // The engine already stripped the provider prefix, so `req.model` is the bare id.
        // `n`/`size` skip-serialize when `None` (see the request type), so no nulls are sent.
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&key.value)
            .timeout(crate::http::REQUEST_TIMEOUT)
            .json(&req)
            .send()
            .await
            .map_err(net_err)?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(KgError::provider(text, status.as_u16()).with_provider(self.key.as_str()));
        }

        let parsed: OaiImageResponse = resp
            .json()
            .await
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))?;
        Ok(ImageResponse {
            data: parsed
                .data
                .into_iter()
                .map(|d| ImageData {
                    url: d.url,
                    b64_json: d.b64_json,
                })
                .collect(),
        })
    }
}

// OpenAI /audio/transcriptions response shape.
#[derive(Deserialize)]
struct OaiTranscriptionResponse {
    text: String,
}

#[async_trait]
impl Audio for OpenAiProvider {
    async fn speech(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: SpeechRequest,
    ) -> Result<SpeechResponse, KgError> {
        let url = format!("{}/audio/speech", self.base_url);
        // OpenAI names the field `response_format`; default to mp3 when unspecified.
        let body = serde_json::json!({
            "model": req.model,
            "input": req.input,
            "voice": req.voice,
            "response_format": req.format.as_deref().unwrap_or("mp3"),
        });
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&key.value)
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

        // The body is raw audio bytes (not JSON). Capture the content type before
        // consuming the response, falling back to a sensible default.
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("audio/mpeg")
            .to_string();
        let bytes = resp.bytes().await.map_err(net_err)?;
        Ok(SpeechResponse {
            audio: bytes.to_vec(),
            content_type,
        })
    }

    async fn transcribe(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: TranscriptionRequest,
    ) -> Result<TranscriptionResponse, KgError> {
        let url = format!("{}/audio/transcriptions", self.base_url);
        // multipart/form-data: the raw audio file part + the model text part.
        let file_part = reqwest::multipart::Part::bytes(req.audio)
            .file_name(req.filename)
            .mime_str("application/octet-stream")
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("multipart error: {e}")))?;
        let form = reqwest::multipart::Form::new()
            .part("file", file_part)
            .text("model", req.model);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&key.value)
            .timeout(crate::http::REQUEST_TIMEOUT)
            .multipart(form)
            .send()
            .await
            .map_err(net_err)?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(KgError::provider(text, status.as_u16()).with_provider(self.key.as_str()));
        }

        let parsed: OaiTranscriptionResponse = resp
            .json()
            .await
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))?;
        Ok(TranscriptionResponse { text: parsed.text })
    }
}

fn net_err(e: reqwest::Error) -> KgError {
    KgError::new(KgErrorKind::Network, e.to_string()).with_retryable(true)
}

/// Parse an OpenAI SSE byte stream into `StreamChunk`s. Handles multi-line frames,
/// partial buffers spanning byte chunks, and the terminal `[DONE]` sentinel.
///
/// Split out as a free function (pure over the byte stream) so it is unit-testable
/// without a live HTTP connection — see the tests module.
fn sse_to_chunks(
    byte_stream: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
) -> BoxStream<'static, Result<StreamChunk, KgError>> {
    let s = async_stream::stream! {
        futures::pin_mut!(byte_stream);
        // Buffer raw BYTES, not a lossy-decoded String: a multibyte UTF-8 char (emoji,
        // CJK, …) can be split across two byte chunks, and decoding each chunk
        // independently would corrupt it. We only decode complete frames.
        let mut buf: Vec<u8> = Vec::new();
        while let Some(item) = byte_stream.next().await {
            let bytes = match item {
                Ok(b) => b,
                Err(e) => { yield Err(net_err(e)); return; }
            };
            buf.extend_from_slice(&bytes);

            // Emit every complete SSE frame (terminated by a blank line).
            while let Some(pos) = find_subslice(&buf, b"\n\n") {
                let frame_bytes: Vec<u8> = buf.drain(..pos + 2).collect();
                // A complete frame ends on a newline boundary, so it is valid UTF-8.
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

    #[tokio::test]
    async fn sse_parses_frames_split_across_byte_chunks() {
        // A chunk deliberately split mid-frame to exercise the cross-chunk buffer.
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
    async fn sse_preserves_multibyte_char_split_across_chunks() {
        // The 🎉 emoji (4 UTF-8 bytes, F0 9F 8E 89) is split across two byte chunks.
        // Byte-buffering must reassemble it; lossy per-chunk decoding would corrupt it.
        let full = "data: {\"id\":\"a\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"🎉\"}}]}\n\n";
        let bytes = full.as_bytes();
        let split = bytes.iter().position(|&b| b == 0xF0).unwrap() + 2; // mid-emoji
        let frames = vec![
            Ok::<_, reqwest::Error>(Bytes::copy_from_slice(&bytes[..split])),
            Ok(Bytes::copy_from_slice(&bytes[split..])),
        ];
        let mut out = sse_to_chunks(stream::iter(frames));
        let chunk = out.next().await.unwrap().expect("chunk parses");
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("🎉"));
    }

    #[tokio::test]
    async fn sse_stops_at_done_sentinel() {
        let frames = vec![Ok::<_, reqwest::Error>(Bytes::from(
            "data: {\"id\":\"a\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n\ndata: [DONE]\n\ndata: {\"should\":\"ignore\"}\n\n",
        ))];
        let out = sse_to_chunks(stream::iter(frames));
        let collected: Vec<_> = out.collect().await;
        assert_eq!(collected.len(), 1, "only one chunk before [DONE]");
    }

    #[tokio::test]
    async fn sse_forwards_streamed_tool_call_deltas() {
        // A tool-call delta must survive the parse → internal-schema → reserialize round-trip.
        // Before `Delta` modeled `tool_calls`, these fragments were silently dropped.
        let frames = vec![Ok::<_, reqwest::Error>(Bytes::from(
            "data: {\"id\":\"a\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{}\"}}]}}]}\n\n",
        ))];
        let mut out = sse_to_chunks(stream::iter(frames));
        let chunk = out.next().await.unwrap().expect("chunk parses");
        let tc = &chunk.choices[0].delta.tool_calls;
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].id.as_deref(), Some("call_1"));
        assert_eq!(
            tc[0].function.as_ref().and_then(|f| f.name.as_deref()),
            Some("get_weather")
        );
    }

    #[test]
    fn body_strips_gateway_fields_and_prefix() {
        let p = OpenAiProvider::new();
        let req = ChatRequest {
            model: "openai/gpt-4o".into(),
            messages: vec![kgateway_core::schema::Message::user("hi")],
            fallbacks: vec![kgateway_core::schema::Fallback {
                provider: "anthropic".into(),
                model: "claude".into(),
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

    #[tokio::test]
    async fn embeddings_decode_and_order() {
        use kgateway_core::provider::{ApiKey, EmbeddingRequest, Embeddings};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Return data out of order to prove we sort by `index`.
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "model": "text-embedding-3-small",
                "data": [
                    { "object": "embedding", "index": 1, "embedding": [0.4, 0.5] },
                    { "object": "embedding", "index": 0, "embedding": [0.1, 0.2] }
                ],
                "usage": { "prompt_tokens": 5, "total_tokens": 5 }
            })))
            .mount(&server)
            .await;

        let provider = OpenAiProvider::with_base_url(server.uri());
        let ctx = Ctx::new();
        let key = ApiKey {
            id: "k".into(),
            value: "sk".into(),
            weight: 1,
            models: vec![],
        };
        let resp = provider
            .embed(
                &ctx,
                &key,
                EmbeddingRequest {
                    model: "text-embedding-3-small".into(),
                    input: vec!["a".into(), "b".into()],
                },
            )
            .await
            .expect("embeddings should decode");

        assert_eq!(resp.embeddings.len(), 2);
        assert_eq!(resp.embeddings[0], vec![0.1, 0.2], "sorted by index");
        assert_eq!(resp.embeddings[1], vec![0.4, 0.5]);
        assert_eq!(resp.usage.total_tokens, 5);
    }

    #[tokio::test]
    async fn image_generate_decodes_url() {
        use kgateway_core::provider::{ApiKey, ImageGenerationRequest, Images};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "url": "https://img/1.png" }]
            })))
            .mount(&server)
            .await;

        let provider = OpenAiProvider::with_base_url(server.uri());
        let ctx = Ctx::new();
        let key = ApiKey {
            id: "k".into(),
            value: "sk".into(),
            weight: 1,
            models: vec![],
        };
        let resp = Images::image_generate(
            &provider,
            &ctx,
            &key,
            ImageGenerationRequest {
                model: "dall-e-3".into(),
                prompt: "a cat".into(),
                n: None,
                size: None,
            },
        )
        .await
        .expect("image should decode");

        assert_eq!(resp.data.len(), 1);
        assert_eq!(resp.data[0].url.as_deref(), Some("https://img/1.png"));
    }

    #[tokio::test]
    async fn speech_returns_raw_bytes_and_content_type() {
        use kgateway_core::provider::{ApiKey, Audio, SpeechRequest};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/speech"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(vec![1, 2, 3])
                    .insert_header("content-type", "audio/mpeg"),
            )
            .mount(&server)
            .await;

        let provider = OpenAiProvider::with_base_url(server.uri());
        let ctx = Ctx::new();
        let key = ApiKey {
            id: "k".into(),
            value: "sk".into(),
            weight: 1,
            models: vec![],
        };
        let resp = Audio::speech(
            &provider,
            &ctx,
            &key,
            SpeechRequest {
                model: "tts-1".into(),
                input: "hello".into(),
                voice: "alloy".into(),
                format: None,
            },
        )
        .await
        .expect("speech should return bytes");

        assert_eq!(resp.audio, vec![1, 2, 3]);
        assert_eq!(resp.content_type, "audio/mpeg");
    }

    #[tokio::test]
    async fn transcribe_decodes_text() {
        use kgateway_core::provider::{ApiKey, Audio, TranscriptionRequest};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/transcriptions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "text": "hello world"
            })))
            .mount(&server)
            .await;

        let provider = OpenAiProvider::with_base_url(server.uri());
        let ctx = Ctx::new();
        let key = ApiKey {
            id: "k".into(),
            value: "sk".into(),
            weight: 1,
            models: vec![],
        };
        let resp = Audio::transcribe(
            &provider,
            &ctx,
            &key,
            TranscriptionRequest {
                model: "whisper-1".into(),
                audio: vec![0, 1, 2, 3],
                filename: "audio.mp3".into(),
            },
        )
        .await
        .expect("transcription should decode");

        assert_eq!(resp.text, "hello world");
    }
}
