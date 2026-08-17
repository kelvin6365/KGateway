//! ElevenLabs provider — registered for **audio** only (speech synthesis and
//! transcription). ElevenLabs has no chat/completions surface at all, so `chat`
//! and `chat_stream` return `Unsupported` the way [`crate::cohere`] does.
//!
//! Two wire details differ from every other connector here:
//! - Auth is the `xi-api-key` header, not `Authorization: Bearer`.
//! - The **voice is a path segment**, not a body field:
//!   `POST /v1/text-to-speech/{voice_id}`. The gateway's [`SpeechRequest::voice`]
//!   carries it, and [`SpeechRequest::model`] becomes the body's `model_id`.
//!
//! TTS *streaming* (`/v1/text-to-speech/{id}/stream`) is deliberately not wired:
//! the [`Audio`] trait has no streaming method, and adding one is a separate
//! trait change rather than a provider change.

use async_trait::async_trait;
use kgateway_core::context::Ctx;
use kgateway_core::error::{KgError, KgErrorKind};
use kgateway_core::provider::{
    ApiKey, Audio, Provider, ProviderKey, SpeechRequest, SpeechResponse, TranscriptionRequest,
    TranscriptionResponse,
};
use kgateway_core::schema::{ChatRequest, ChatResponse};
use serde::Deserialize;

const DEFAULT_BASE_URL: &str = "https://api.elevenlabs.io";

/// ElevenLabs' default voice ("Rachel"), used when a request omits `voice`.
/// The API has no server-side default — the voice is in the URL, so something
/// must be chosen here or the request cannot be built at all.
const DEFAULT_VOICE_ID: &str = "21m00Tcm4TlvDq8ikWAM";

pub struct ElevenLabsProvider {
    key: ProviderKey,
    base_url: String,
    client: reqwest::Client,
}

impl ElevenLabsProvider {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            key: ProviderKey::new("elevenlabs"),
            base_url: base_url.into(),
            client: crate::http::default_client(),
        }
    }

    pub fn with_identity(key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            key: ProviderKey::new(key),
            base_url: base_url.into(),
            client: crate::http::default_client(),
        }
    }
}

impl Default for ElevenLabsProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for ElevenLabsProvider {
    fn key(&self) -> ProviderKey {
        self.key.clone()
    }

    async fn chat(
        &self,
        _ctx: &Ctx,
        _key: &ApiKey,
        _req: ChatRequest,
    ) -> Result<ChatResponse, KgError> {
        Err(KgError::unsupported("chat for elevenlabs"))
    }

    async fn chat_stream(
        &self,
        _ctx: &Ctx,
        _key: &ApiKey,
        _req: ChatRequest,
    ) -> Result<kgateway_core::provider::ChunkStream, KgError> {
        Err(KgError::unsupported("chat for elevenlabs"))
    }

    fn as_audio(&self) -> Option<&dyn Audio> {
        Some(self)
    }
}

#[derive(Deserialize)]
struct ElevenLabsTranscription {
    #[serde(default)]
    text: String,
}

/// Map the gateway's short format names onto ElevenLabs `output_format` tokens.
/// ElevenLabs wants a codec+rate+bitrate triple; passing a bare `"mp3"` is a 422.
fn output_format(format: Option<&str>) -> &'static str {
    match format {
        Some("wav") => "pcm_44100",
        Some("pcm") => "pcm_24000",
        Some("ulaw") => "ulaw_8000",
        // Covers None and "mp3" — the vendor default.
        _ => "mp3_44100_128",
    }
}

/// Content type for a resolved `output_format` token. ElevenLabs sets this header
/// itself, so this is only the fallback when it doesn't.
fn content_type_for(output_format: &str) -> &'static str {
    if output_format.starts_with("pcm") {
        "audio/wav"
    } else if output_format.starts_with("ulaw") {
        "audio/basic"
    } else {
        "audio/mpeg"
    }
}

#[async_trait]
impl Audio for ElevenLabsProvider {
    async fn speech(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: SpeechRequest,
    ) -> Result<SpeechResponse, KgError> {
        let voice = if req.voice.trim().is_empty() {
            DEFAULT_VOICE_ID
        } else {
            req.voice.trim()
        };
        let fmt = output_format(req.format.as_deref());
        let url = format!(
            "{}/v1/text-to-speech/{}?output_format={}",
            self.base_url, voice, fmt
        );
        let body = serde_json::json!({
            "text": req.input,
            "model_id": req.model,
        });

        let resp = self
            .client
            .post(&url)
            .header("xi-api-key", &key.value)
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

        // Raw audio bytes, not JSON. Capture the content type before consuming.
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_else(|| content_type_for(fmt))
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
        let url = format!("{}/v1/speech-to-text", self.base_url);
        // ElevenLabs names the file part `file` and the model part `model_id`
        // (OpenAI uses `model`), so this is not a drop-in of the OpenAI form.
        let file_part = reqwest::multipart::Part::bytes(req.audio)
            .file_name(req.filename)
            .mime_str("application/octet-stream")
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("multipart error: {e}")))?;
        let form = reqwest::multipart::Form::new()
            .part("file", file_part)
            .text("model_id", req.model);

        let resp = self
            .client
            .post(&url)
            .header("xi-api-key", &key.value)
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

        let parsed: ElevenLabsTranscription = resp
            .json()
            .await
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))?;
        Ok(TranscriptionResponse { text: parsed.text })
    }
}

fn net_err(e: reqwest::Error) -> KgError {
    KgError::new(KgErrorKind::Network, e.to_string()).with_retryable(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_key() -> ApiKey {
        ApiKey {
            id: "k".into(),
            value: "xi-test-key".into(),
            weight: 1,
            models: vec![],
        }
    }

    #[test]
    fn output_format_maps_short_names_to_vendor_tokens() {
        assert_eq!(output_format(None), "mp3_44100_128");
        assert_eq!(output_format(Some("mp3")), "mp3_44100_128");
        assert_eq!(output_format(Some("wav")), "pcm_44100");
        assert_eq!(output_format(Some("ulaw")), "ulaw_8000");
        // An unrecognized format falls back rather than producing a 422 upstream.
        assert_eq!(output_format(Some("flac")), "mp3_44100_128");
    }

    #[test]
    fn content_type_tracks_the_resolved_format() {
        assert_eq!(content_type_for("mp3_44100_128"), "audio/mpeg");
        assert_eq!(content_type_for("pcm_44100"), "audio/wav");
        assert_eq!(content_type_for("ulaw_8000"), "audio/basic");
    }

    #[tokio::test]
    async fn chat_is_unsupported() {
        let p = ElevenLabsProvider::new();
        let err = p
            .chat(&Ctx::new(), &test_key(), ChatRequest::default())
            .await
            .expect_err("elevenlabs has no chat surface");
        assert_eq!(err.kind, KgErrorKind::Unsupported);
    }

    #[tokio::test]
    async fn speech_puts_voice_in_path_and_uses_xi_api_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text-to-speech/voice-abc"))
            .and(header("xi-api-key", "xi-test-key"))
            .and(query_param("output_format", "mp3_44100_128"))
            .and(body_partial_json(serde_json::json!({
                "text": "hello there",
                "model_id": "eleven_multilingual_v2",
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "audio/mpeg")
                    .set_body_bytes(vec![0xFF, 0xFB, 0x90]),
            )
            .expect(1)
            .mount(&server)
            .await;

        let p = ElevenLabsProvider::with_base_url(server.uri());
        let out = p
            .speech(
                &Ctx::new(),
                &test_key(),
                SpeechRequest {
                    model: "eleven_multilingual_v2".into(),
                    input: "hello there".into(),
                    voice: "voice-abc".into(),
                    format: None,
                },
            )
            .await
            .expect("speech should succeed");

        assert_eq!(out.audio, vec![0xFF, 0xFB, 0x90]);
        assert_eq!(out.content_type, "audio/mpeg");
    }

    #[tokio::test]
    async fn speech_falls_back_to_default_voice_when_blank() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/v1/text-to-speech/{DEFAULT_VOICE_ID}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2]))
            .expect(1)
            .mount(&server)
            .await;

        let p = ElevenLabsProvider::with_base_url(server.uri());
        let out = p
            .speech(
                &Ctx::new(),
                &test_key(),
                SpeechRequest {
                    model: "eleven_turbo_v2_5".into(),
                    input: "hi".into(),
                    voice: "   ".into(),
                    format: None,
                },
            )
            .await
            .expect("blank voice should fall back, not fail");
        assert_eq!(out.audio, vec![1, 2]);
    }

    #[tokio::test]
    async fn transcribe_decodes_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/speech-to-text"))
            .and(header("xi-api-key", "xi-test-key"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "text": "transcribed words" })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let p = ElevenLabsProvider::with_base_url(server.uri());
        let out = p
            .transcribe(
                &Ctx::new(),
                &test_key(),
                TranscriptionRequest {
                    model: "scribe_v1".into(),
                    audio: vec![0, 1, 2, 3],
                    filename: "clip.wav".into(),
                },
            )
            .await
            .expect("transcription should decode");
        assert_eq!(out.text, "transcribed words");
    }

    #[tokio::test]
    async fn speech_error_429_is_retryable_and_tagged() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text-to-speech/v1"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(&server)
            .await;

        let p = ElevenLabsProvider::with_base_url(server.uri());
        let err = p
            .speech(
                &Ctx::new(),
                &test_key(),
                SpeechRequest {
                    model: "eleven_turbo_v2_5".into(),
                    input: "hi".into(),
                    voice: "v1".into(),
                    format: None,
                },
            )
            .await
            // `SpeechResponse` carries raw audio and is deliberately not `Debug`,
            // so unwrap the error side rather than using `expect_err`.
            .err()
            .expect("429 should map to an error");

        assert!(err.is_retryable(), "429 must be retryable");
        assert_eq!(err.status, Some(429));
        assert_eq!(err.provider.as_deref(), Some("elevenlabs"));
    }
}
