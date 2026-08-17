//! Sarvam AI provider — a **hybrid**: OpenAI-wire for chat, bespoke for audio.
//!
//! Three things make this a native connector rather than an
//! [`crate::openai_compat`] entry:
//!
//! 1. **Split auth.** Chat authenticates with `Authorization: Bearer`; the audio
//!    endpoints use an `api-subscription-key` header instead.
//! 2. **Split path prefix.** Chat lives at `/v1/chat/completions`, but the audio
//!    routes have **no `/v1`** — they are `/text-to-speech` and `/speech-to-text`.
//! 3. Sarvam returns synthesized audio as **base64 inside a JSON envelope**
//!    (`{"audios": ["…"]}`), not as a raw response body.
//!
//! Registering this name in `openai_compat::KNOWN` instead would silently produce
//! a chat-only provider with no audio capability and no error, because the name
//! arm in `build_engine` matches before the compat fallthrough.
//!
//! **Verification status: mock-only.** The chat path is a plain OpenAI wire and is
//! low-risk, but the audio field names below (`inputs`/`speaker`/
//! `target_language_code`/`audios`, and `transcript` on the STT response) are
//! wiremock-verified only and should be confirmed against a live account before
//! anyone depends on them.

use async_trait::async_trait;
use base64::Engine as _;
use kgateway_core::context::Ctx;
use kgateway_core::error::{KgError, KgErrorKind};
use kgateway_core::provider::{
    ApiKey, Audio, ChunkStream, Provider, ProviderKey, SpeechRequest, SpeechResponse,
    TranscriptionRequest, TranscriptionResponse,
};
use kgateway_core::schema::{ChatRequest, ChatResponse};
use serde::Deserialize;

use crate::openai::OpenAiProvider;

const DEFAULT_BASE_URL: &str = "https://api.sarvam.ai";

/// Sarvam requires a target language on every TTS call and has no server-side
/// default. Indian English is the safest general choice.
const DEFAULT_LANGUAGE: &str = "en-IN";

/// Default TTS speaker when a request omits `voice`.
const DEFAULT_SPEAKER: &str = "meera";

pub struct SarvamProvider {
    key: ProviderKey,
    base_url: String,
    client: reqwest::Client,
    /// Delegate for the OpenAI-wire chat surface. Constructed with this
    /// provider's identity so errors and routing report `sarvam`, not `openai`.
    chat: OpenAiProvider,
}

impl SarvamProvider {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self::with_identity("sarvam", base_url)
    }

    pub fn with_identity(key: impl Into<String>, base_url: impl Into<String>) -> Self {
        let key = ProviderKey::new(key);
        let base_url = base_url.into();
        // The chat delegate needs the `/v1` that the audio routes must not have.
        let chat = OpenAiProvider::with_identity(key.as_str(), format!("{base_url}/v1"));
        Self {
            key,
            base_url,
            client: crate::http::default_client(),
            chat,
        }
    }
}

impl Default for SarvamProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for SarvamProvider {
    fn key(&self) -> ProviderKey {
        self.key.clone()
    }

    async fn chat(
        &self,
        ctx: &Ctx,
        key: &ApiKey,
        req: ChatRequest,
    ) -> Result<ChatResponse, KgError> {
        self.chat.chat(ctx, key, req).await
    }

    async fn chat_stream(
        &self,
        ctx: &Ctx,
        key: &ApiKey,
        req: ChatRequest,
    ) -> Result<ChunkStream, KgError> {
        self.chat.chat_stream(ctx, key, req).await
    }

    fn as_audio(&self) -> Option<&dyn Audio> {
        Some(self)
    }
}

#[derive(Deserialize)]
struct SarvamSpeechResponse {
    /// One base64 clip per input string. The gateway sends exactly one input, so
    /// exactly one clip comes back.
    #[serde(default)]
    audios: Vec<String>,
}

#[derive(Deserialize)]
struct SarvamTranscriptionResponse {
    #[serde(default)]
    transcript: String,
}

#[async_trait]
impl Audio for SarvamProvider {
    async fn speech(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: SpeechRequest,
    ) -> Result<SpeechResponse, KgError> {
        // No `/v1` on this route — see the module header.
        let url = format!("{}/text-to-speech", self.base_url);
        let speaker = if req.voice.trim().is_empty() {
            DEFAULT_SPEAKER
        } else {
            req.voice.trim()
        };
        let body = serde_json::json!({
            "inputs": [req.input],
            "model": req.model,
            "speaker": speaker,
            "target_language_code": DEFAULT_LANGUAGE,
        });

        let resp = self
            .client
            .post(&url)
            .header("api-subscription-key", &key.value)
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

        let parsed: SarvamSpeechResponse = resp
            .json()
            .await
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))?;
        let encoded = parsed.audios.into_iter().next().ok_or_else(|| {
            KgError::new(
                KgErrorKind::Internal,
                "sarvam returned no audio for the request",
            )
        })?;
        let audio = base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("audio decode error: {e}")))?;

        Ok(SpeechResponse {
            audio,
            // Sarvam synthesizes WAV regardless of the requested short format.
            content_type: "audio/wav".to_string(),
        })
    }

    async fn transcribe(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: TranscriptionRequest,
    ) -> Result<TranscriptionResponse, KgError> {
        let url = format!("{}/speech-to-text", self.base_url);
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
            .header("api-subscription-key", &key.value)
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

        let parsed: SarvamTranscriptionResponse = resp
            .json()
            .await
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))?;
        Ok(TranscriptionResponse {
            text: parsed.transcript,
        })
    }
}

fn net_err(e: reqwest::Error) -> KgError {
    KgError::new(KgErrorKind::Network, e.to_string()).with_retryable(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgateway_core::schema::Message;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_key() -> ApiKey {
        ApiKey {
            id: "k".into(),
            value: "sarvam-test-key".into(),
            weight: 1,
            models: vec![],
        }
    }

    #[test]
    fn chat_delegate_carries_sarvam_identity_and_v1_prefix() {
        let p = SarvamProvider::with_base_url("https://example.test");
        assert_eq!(p.key().as_str(), "sarvam");
        // The delegate must report `sarvam` so errors and routing are attributed
        // correctly rather than surfacing as `openai`.
        assert_eq!(p.chat.key().as_str(), "sarvam");
    }

    #[tokio::test]
    async fn chat_uses_bearer_auth_on_the_v1_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sarvam-test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "c1",
                "object": "chat.completion",
                "created": 1,
                "model": "sarvam-m",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "namaste" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let p = SarvamProvider::with_base_url(server.uri());
        let out = p
            .chat(
                &Ctx::new(),
                &test_key(),
                ChatRequest {
                    model: "sarvam-m".into(),
                    messages: vec![Message::user("hi")],
                    ..Default::default()
                },
            )
            .await
            .expect("chat should succeed");
        assert_eq!(out.choices[0].message.text_content(), Some("namaste"));
    }

    #[tokio::test]
    async fn speech_uses_subscription_key_on_the_unversioned_path() {
        let server = MockServer::start().await;
        // Encodes to "AQIDBA==".
        let clip = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3, 4]);
        Mock::given(method("POST"))
            // No `/v1` here — that is the whole point of this connector.
            .and(path("/text-to-speech"))
            .and(header("api-subscription-key", "sarvam-test-key"))
            .and(body_partial_json(serde_json::json!({
                "inputs": ["bolo"],
                "model": "bulbul:v2",
                "speaker": "arvind",
                "target_language_code": "en-IN",
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "audios": [clip] })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let p = SarvamProvider::with_base_url(server.uri());
        let out = p
            .speech(
                &Ctx::new(),
                &test_key(),
                SpeechRequest {
                    model: "bulbul:v2".into(),
                    input: "bolo".into(),
                    voice: "arvind".into(),
                    format: None,
                },
            )
            .await
            .expect("speech should succeed");

        // Base64 in the envelope must be decoded to raw bytes for the caller.
        assert_eq!(out.audio, vec![1, 2, 3, 4]);
        assert_eq!(out.content_type, "audio/wav");
    }

    #[tokio::test]
    async fn speech_defaults_the_speaker_when_voice_is_blank() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/text-to-speech"))
            .and(body_partial_json(
                serde_json::json!({ "speaker": DEFAULT_SPEAKER }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "audios": [base64::engine::general_purpose::STANDARD.encode([9u8])]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let p = SarvamProvider::with_base_url(server.uri());
        let out = p
            .speech(
                &Ctx::new(),
                &test_key(),
                SpeechRequest {
                    model: "bulbul:v2".into(),
                    input: "x".into(),
                    voice: String::new(),
                    format: None,
                },
            )
            .await
            .expect("blank voice should fall back");
        assert_eq!(out.audio, vec![9]);
    }

    #[tokio::test]
    async fn speech_with_empty_audio_list_is_an_error_not_a_silent_empty_clip() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/text-to-speech"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "audios": []
            })))
            .mount(&server)
            .await;

        let p = SarvamProvider::with_base_url(server.uri());
        let err = p
            .speech(
                &Ctx::new(),
                &test_key(),
                SpeechRequest {
                    model: "bulbul:v2".into(),
                    input: "x".into(),
                    voice: "meera".into(),
                    format: None,
                },
            )
            .await
            // `SpeechResponse` carries raw audio and is deliberately not `Debug`.
            .err()
            .expect("an empty clip list must surface, not return 0 bytes");
        assert_eq!(err.kind, KgErrorKind::Internal);
    }

    #[tokio::test]
    async fn transcribe_reads_the_transcript_field() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/speech-to-text"))
            .and(header("api-subscription-key", "sarvam-test-key"))
            .respond_with(
                // Sarvam names this `transcript`, not `text` as OpenAI does.
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "transcript": "sun raha hoon" })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let p = SarvamProvider::with_base_url(server.uri());
        let out = p
            .transcribe(
                &Ctx::new(),
                &test_key(),
                TranscriptionRequest {
                    model: "saarika:v2".into(),
                    audio: vec![0, 1],
                    filename: "a.wav".into(),
                },
            )
            .await
            .expect("transcription should decode");
        assert_eq!(out.text, "sun raha hoon");
    }

    #[tokio::test]
    async fn speech_error_429_is_retryable_and_tagged() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/text-to-speech"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .mount(&server)
            .await;

        let p = SarvamProvider::with_base_url(server.uri());
        let err = p
            .speech(
                &Ctx::new(),
                &test_key(),
                SpeechRequest {
                    model: "bulbul:v2".into(),
                    input: "x".into(),
                    voice: "meera".into(),
                    format: None,
                },
            )
            .await
            // `SpeechResponse` carries raw audio and is deliberately not `Debug`.
            .err()
            .expect("429 should map to an error");

        assert!(err.is_retryable(), "429 must be retryable");
        assert_eq!(err.status, Some(429));
        assert_eq!(err.provider.as_deref(), Some("sarvam"));
    }
}
