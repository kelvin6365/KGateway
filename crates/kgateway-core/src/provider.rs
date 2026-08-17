//! Provider abstraction. **Deliberately NOT** a 100+ method god-interface —
//! a slim required `Provider` trait plus opt-in capability traits. A provider
//! implements only what it supports; the router checks capabilities before dispatch.
//! See `docs/03-providers.md`.

use crate::context::Ctx;
use crate::error::KgError;
use crate::schema::{ChatRequest, ChatResponse, StreamChunk};
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

/// Stable provider identifier (e.g. "openai", "anthropic", "groq").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderKey(pub String);

impl ProviderKey {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A credential for a provider. Providers may hold several; the router picks one
/// via weighted-random selection (see `router::keyselect`).
///
/// `Debug` is hand-implemented to REDACT `value` — deriving it would leak the plaintext
/// secret into any `{:?}`/`tracing::debug!(?key)` call. `#[serde(skip_serializing)]`
/// only covers JSON serialization, not Debug, so the guard must be at the type level.
#[derive(Clone, Serialize, Deserialize)]
pub struct ApiKey {
    /// Opaque identifier for logging (never the secret itself).
    pub id: String,
    /// The secret value (resolved from env/config; keep out of logs).
    #[serde(skip_serializing)]
    pub value: String,
    /// Relative weight for load balancing (default 1).
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Optional model allow-list this key may serve.
    #[serde(default)]
    pub models: Vec<String>,
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKey")
            .field("id", &self.id)
            .field("value", &"<redacted>")
            .field("weight", &self.weight)
            .field("models", &self.models)
            .finish()
    }
}

fn default_weight() -> u32 {
    1
}

pub type ChunkStream = BoxStream<'static, Result<StreamChunk, KgError>>;

/// The required contract for every provider.
#[async_trait]
pub trait Provider: Send + Sync {
    fn key(&self) -> ProviderKey;

    async fn chat(
        &self,
        ctx: &Ctx,
        key: &ApiKey,
        req: ChatRequest,
    ) -> Result<ChatResponse, KgError>;

    async fn chat_stream(
        &self,
        ctx: &Ctx,
        key: &ApiKey,
        req: ChatRequest,
    ) -> Result<ChunkStream, KgError>;

    /// Capability accessors: a provider that implements a capability trait overrides the
    /// matching accessor to return `Some(self)`. The engine consults these to dispatch,
    /// returning a clean `Unsupported` (before dispatch) for unsupported operations —
    /// rather than a runtime error mid-call.
    fn as_embeddings(&self) -> Option<&dyn Embeddings> {
        None
    }
    fn as_images(&self) -> Option<&dyn Images> {
        None
    }
    fn as_audio(&self) -> Option<&dyn Audio> {
        None
    }
    fn as_rerank(&self) -> Option<&dyn Rerank> {
        None
    }

    fn as_video(&self) -> Option<&dyn Video> {
        None
    }
}

// ---- Opt-in capability traits (implemented only where supported) ----

/// Embedding request/response (minimal; expand in M3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    pub model: String,
    pub embeddings: Vec<Vec<f32>>,
    #[serde(default)]
    pub usage: crate::schema::Usage,
}

#[async_trait]
pub trait Embeddings: Provider {
    async fn embed(
        &self,
        ctx: &Ctx,
        key: &ApiKey,
        req: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, KgError>;
}

// ---- Images ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageGenerationRequest {
    pub model: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b64_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageResponse {
    pub data: Vec<ImageData>,
}

#[async_trait]
pub trait Images: Provider {
    async fn image_generate(
        &self,
        ctx: &Ctx,
        key: &ApiKey,
        req: ImageGenerationRequest,
    ) -> Result<ImageResponse, KgError>;
}

// ---- Audio ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeechRequest {
    pub model: String,
    pub input: String,
    pub voice: String,
    /// Output format, e.g. "mp3", "wav". Defaults to the provider's default. Accepts the
    /// OpenAI `response_format` field name too.
    #[serde(
        default,
        alias = "response_format",
        skip_serializing_if = "Option::is_none"
    )]
    pub format: Option<String>,
}

/// Synthesized audio bytes + their content type.
pub struct SpeechResponse {
    pub audio: Vec<u8>,
    pub content_type: String,
}

/// A transcription request carrying the raw audio file bytes.
pub struct TranscriptionRequest {
    pub model: String,
    pub audio: Vec<u8>,
    pub filename: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionResponse {
    pub text: String,
}

#[async_trait]
pub trait Audio: Provider {
    async fn speech(
        &self,
        ctx: &Ctx,
        key: &ApiKey,
        req: SpeechRequest,
    ) -> Result<SpeechResponse, KgError>;

    async fn transcribe(
        &self,
        ctx: &Ctx,
        key: &ApiKey,
        req: TranscriptionRequest,
    ) -> Result<TranscriptionResponse, KgError>;
}

// ---- Rerank ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankRequest {
    pub model: String,
    pub query: String,
    pub documents: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankResult {
    pub index: usize,
    pub relevance_score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankResponse {
    pub results: Vec<RerankResult>,
}

#[async_trait]
pub trait Rerank: Provider {
    async fn rerank(
        &self,
        ctx: &Ctx,
        key: &ApiKey,
        req: RerankRequest,
    ) -> Result<RerankResponse, KgError>;
}

// ---- Video ----

/// Lifecycle of an asynchronous video generation job.
///
/// Providers map their own vocabulary onto these five. Anything unrecognized maps
/// to [`VideoStatus::Running`] — optimistically, because an unknown state is not a
/// failure and the client will poll again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoStatus {
    /// Accepted upstream, not yet started.
    Queued,
    /// Generating.
    Running,
    /// Finished; [`VideoResponse::data`] carries the artifact(s).
    Succeeded,
    /// Terminal failure; [`VideoResponse::error`] carries the upstream reason.
    Failed,
    /// Cancelled upstream or by the account owner.
    Cancelled,
}

impl VideoStatus {
    /// Whether polling should stop.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// A video generation request.
///
/// Every field beyond `model` is optional so vendors can diverge without a
/// breaking change; each adapter validates what it actually needs. Which *mode*
/// runs is inferred from the inputs — `image` present selects image-to-video,
/// otherwise text-to-video.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoGenerationRequest {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Seed image: an `https://` URL or a `data:image/...;base64,` URI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<u32>,
    /// Provider-specific aspect/resolution token, e.g. `"1280:720"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

/// One produced artifact. Mirrors [`ImageData`] deliberately: a URL when the
/// vendor hosts the file, inline base64 only when it doesn't.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b64_json: Option<String>,
}

/// The state of one video job. Returned by BOTH [`Video::video_generate`] (where
/// `status` is normally `Queued` and `data` is empty) and
/// [`Video::video_retrieve`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoResponse {
    /// Opaque job handle.
    ///
    /// Providers set the **raw upstream id** here; the engine rewrites it to
    /// `provider/keyid:rawid` before it reaches the client. Clients echo it back
    /// verbatim to `GET /v1/videos/{provider}/{rest}`.
    pub id: String,
    pub status: VideoStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data: Vec<VideoData>,
    /// Upstream failure reason. Only set alongside [`VideoStatus::Failed`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Seconds the client should wait before polling again. Adapters set a
    /// vendor-aware value; the handler also emits it as a `Retry-After` header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<u32>,
}

/// Asynchronous video generation.
///
/// **Both methods must return promptly.** Generation runs for minutes — far past
/// the 120s provider timeout and the gateway's 120s request timeout — so
/// `video_generate` SUBMITS and returns a handle; it must never poll to
/// completion. A blocking implementation would hold a per-provider semaphore
/// permit for minutes *and still* time out. This is the one capability whose
/// contract is asynchronous, and the split exists for exactly that reason.
#[async_trait]
pub trait Video: Provider {
    async fn video_generate(
        &self,
        ctx: &Ctx,
        key: &ApiKey,
        req: VideoGenerationRequest,
    ) -> Result<VideoResponse, KgError>;

    /// Current state of a previously submitted job. `id` is the RAW upstream id —
    /// the engine has already stripped the `provider/keyid:` routing prefix.
    async fn video_retrieve(
        &self,
        ctx: &Ctx,
        key: &ApiKey,
        id: &str,
    ) -> Result<VideoResponse, KgError>;
}
