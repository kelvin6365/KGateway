//! Runway — image and video generation over an **async task API**.
//!
//! Every generation call submits a task and gets back a `taskId`; the result is
//! fetched from `GET /v1/tasks/{taskId}`.
//!
//! ```text
//! POST /v1/text_to_image    → { id }
//! POST /v1/text_to_video    → { id }      (no image input)
//! POST /v1/image_to_video   → { id }      (image input present)
//! GET  /v1/tasks/{id}       → { status: PENDING|RUNNING|SUCCEEDED|FAILED, output: [url] }
//! ```
//!
//! **Images are synchronous, video is not** — a deliberate asymmetry:
//! - [`Images::image_generate`] submits then polls internally under a bounded
//!   budget ([`MAX_IMAGE_POLL`]), because image tasks settle in seconds and fit
//!   inside a request. That poll holds one of the provider's semaphore permits
//!   for its duration.
//! - [`Video`] does **not** poll. Video takes minutes, past both the provider
//!   timeout and the gateway's request timeout, so `video_generate` returns a
//!   handle and the caller polls `video_retrieve`.
//!
//! Every request must carry `X-Runway-Version`; Runway rejects calls without it.
//!
//! **Verification status: mock-only.**

use std::time::Duration;

use async_trait::async_trait;
use kgateway_core::context::Ctx;
use kgateway_core::error::{KgError, KgErrorKind};
use kgateway_core::provider::{
    ApiKey, ChunkStream, ImageData, ImageGenerationRequest, ImageResponse, Images, Provider,
    ProviderKey, Video, VideoData, VideoGenerationRequest, VideoResponse, VideoStatus,
};
use kgateway_core::schema::{ChatRequest, ChatResponse};
use serde::Deserialize;

const DEFAULT_BASE_URL: &str = "https://api.dev.runwayml.com";

/// Mandatory on every call — Runway 400s without it.
const RUNWAY_VERSION: &str = "2024-11-06";

/// Bound on the synchronous image poll. Stays under the 120s provider timeout.
const MAX_IMAGE_POLL: Duration = Duration::from_secs(90);
const IMAGE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Suggested client poll cadence for video jobs, surfaced as `Retry-After`.
const VIDEO_RETRY_AFTER_SECS: u32 = 5;

pub struct RunwayProvider {
    key: ProviderKey,
    base_url: String,
    client: reqwest::Client,
}

impl RunwayProvider {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self::with_identity("runway", base_url)
    }

    pub fn with_identity(key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            key: ProviderKey::new(key),
            base_url: base_url.into(),
            client: crate::http::default_client(),
        }
    }

    /// Attach the auth and version headers every Runway call requires.
    fn authed(&self, rb: reqwest::RequestBuilder, key: &ApiKey) -> reqwest::RequestBuilder {
        rb.bearer_auth(&key.value)
            .header("X-Runway-Version", RUNWAY_VERSION)
    }

    async fn submit(
        &self,
        key: &ApiKey,
        path: &str,
        body: serde_json::Value,
    ) -> Result<String, KgError> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .authed(self.client.post(&url), key)
            .timeout(crate::http::REQUEST_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(net_err)?;
        let task: TaskCreated = self.decode(resp).await?;
        if task.id.is_empty() {
            return Err(KgError::new(
                KgErrorKind::Internal,
                "runway accepted the task but returned no task id",
            ));
        }
        Ok(task.id)
    }

    async fn fetch_task(&self, key: &ApiKey, id: &str) -> Result<Task, KgError> {
        let url = format!("{}/v1/tasks/{}", self.base_url, id);
        let resp = self
            .authed(self.client.get(&url), key)
            .timeout(crate::http::REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(net_err)?;
        self.decode(resp).await
    }

    async fn decode<T: serde::de::DeserializeOwned>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T, KgError> {
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(KgError::provider(text, status.as_u16()).with_provider(self.key.as_str()));
        }
        resp.json()
            .await
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))
    }
}

impl Default for RunwayProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Which video endpoint a request selects, based on which inputs are set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VideoRoute {
    TextToVideo,
    ImageToVideo,
}

impl VideoRoute {
    fn path(self) -> &'static str {
        match self {
            Self::TextToVideo => "/v1/text_to_video",
            Self::ImageToVideo => "/v1/image_to_video",
        }
    }
}

/// Pick the video endpoint. Free function so the input→route rule is testable.
pub(crate) fn video_route(req: &VideoGenerationRequest) -> VideoRoute {
    match req.image.as_deref() {
        Some(s) if !s.trim().is_empty() => VideoRoute::ImageToVideo,
        _ => VideoRoute::TextToVideo,
    }
}

/// Map Runway's task status vocabulary onto the gateway's.
///
/// Unknown values map to `Running` rather than `Failed` — an unrecognized state is
/// not evidence of failure, and the client will poll again.
pub(crate) fn map_status(raw: &str) -> VideoStatus {
    match raw.to_ascii_uppercase().as_str() {
        "PENDING" | "THROTTLED" => VideoStatus::Queued,
        "SUCCEEDED" => VideoStatus::Succeeded,
        "FAILED" => VideoStatus::Failed,
        "CANCELLED" | "CANCELED" => VideoStatus::Cancelled,
        _ => VideoStatus::Running,
    }
}

#[derive(Deserialize)]
struct TaskCreated {
    #[serde(default)]
    id: String,
}

#[derive(Deserialize)]
struct Task {
    #[serde(default)]
    id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    output: Vec<String>,
    #[serde(default, alias = "failureCode", alias = "failure")]
    error: Option<String>,
}

impl Task {
    fn into_video_response(self) -> VideoResponse {
        let status = map_status(&self.status);
        VideoResponse {
            id: self.id,
            status,
            data: self
                .output
                .into_iter()
                .map(|url| VideoData {
                    url: Some(url),
                    b64_json: None,
                })
                .collect(),
            error: if status == VideoStatus::Failed {
                self.error
            } else {
                None
            },
            retry_after: (!status.is_terminal()).then_some(VIDEO_RETRY_AFTER_SECS),
        }
    }
}

#[async_trait]
impl Provider for RunwayProvider {
    fn key(&self) -> ProviderKey {
        self.key.clone()
    }

    async fn chat(
        &self,
        _ctx: &Ctx,
        _key: &ApiKey,
        _req: ChatRequest,
    ) -> Result<ChatResponse, KgError> {
        Err(KgError::unsupported("chat for runway"))
    }

    async fn chat_stream(
        &self,
        _ctx: &Ctx,
        _key: &ApiKey,
        _req: ChatRequest,
    ) -> Result<ChunkStream, KgError> {
        Err(KgError::unsupported("chat for runway"))
    }

    fn as_images(&self) -> Option<&dyn Images> {
        Some(self)
    }

    fn as_video(&self) -> Option<&dyn Video> {
        Some(self)
    }
}

#[async_trait]
impl Images for RunwayProvider {
    async fn image_generate(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: ImageGenerationRequest,
    ) -> Result<ImageResponse, KgError> {
        let mut body = serde_json::json!({
            "model": req.model,
            "promptText": req.prompt,
        });
        if let Some(size) = &req.size {
            body["ratio"] = serde_json::Value::String(size.clone());
        }
        let id = self.submit(key, "/v1/text_to_image", body).await?;

        // Bounded internal poll — image tasks settle in seconds, so the caller can
        // wait. This holds a provider semaphore permit for its duration.
        let deadline = tokio::time::Instant::now() + MAX_IMAGE_POLL;
        loop {
            let task = self.fetch_task(key, &id).await?;
            match map_status(&task.status) {
                VideoStatus::Succeeded => {
                    return Ok(ImageResponse {
                        data: task
                            .output
                            .into_iter()
                            .map(|url| ImageData {
                                url: Some(url),
                                b64_json: None,
                            })
                            .collect(),
                    })
                }
                VideoStatus::Failed | VideoStatus::Cancelled => {
                    let detail = task
                        .error
                        .unwrap_or_else(|| format!("runway task {id} {}", task.status));
                    return Err(KgError::provider(detail, 502).with_provider(self.key.as_str()));
                }
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(KgError::provider(
                    format!(
                        "runway image task {id} did not settle within {}s",
                        MAX_IMAGE_POLL.as_secs()
                    ),
                    504,
                )
                .with_provider(self.key.as_str()));
            }
            tokio::time::sleep(IMAGE_POLL_INTERVAL).await;
        }
    }
}

#[async_trait]
impl Video for RunwayProvider {
    async fn video_generate(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: VideoGenerationRequest,
    ) -> Result<VideoResponse, KgError> {
        let route = video_route(&req);
        let mut body = serde_json::json!({ "model": req.model });
        if let Some(p) = &req.prompt {
            body["promptText"] = serde_json::Value::String(p.clone());
        }
        if route == VideoRoute::ImageToVideo {
            body["promptImage"] = serde_json::Value::String(req.image.clone().unwrap_or_default());
        }
        if let Some(d) = req.duration_seconds {
            body["duration"] = serde_json::Value::from(d);
        }
        if let Some(r) = &req.ratio {
            body["ratio"] = serde_json::Value::String(r.clone());
        }
        if let Some(s) = req.seed {
            body["seed"] = serde_json::Value::from(s);
        }

        let id = self.submit(key, route.path(), body).await?;
        // Submit only — never poll here. See the module header.
        Ok(VideoResponse {
            id,
            status: VideoStatus::Queued,
            data: Vec::new(),
            error: None,
            retry_after: Some(VIDEO_RETRY_AFTER_SECS),
        })
    }

    async fn video_retrieve(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        id: &str,
    ) -> Result<VideoResponse, KgError> {
        Ok(self.fetch_task(key, id).await?.into_video_response())
    }
}

fn net_err(e: reqwest::Error) -> KgError {
    KgError::new(KgErrorKind::Network, e.to_string()).with_retryable(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_key() -> ApiKey {
        ApiKey {
            id: "k".into(),
            value: "rw-test-key".into(),
            weight: 1,
            models: vec![],
        }
    }

    fn vreq(prompt: Option<&str>, image: Option<&str>) -> VideoGenerationRequest {
        VideoGenerationRequest {
            model: "gen4_turbo".into(),
            prompt: prompt.map(Into::into),
            image: image.map(Into::into),
            duration_seconds: Some(5),
            ratio: Some("1280:720".into()),
            seed: None,
        }
    }

    #[test]
    fn video_route_is_selected_by_the_presence_of_an_image() {
        assert_eq!(
            video_route(&vreq(Some("a cat"), None)),
            VideoRoute::TextToVideo
        );
        assert_eq!(
            video_route(&vreq(Some("a cat"), Some("https://x/seed.png"))),
            VideoRoute::ImageToVideo
        );
        // A blank image is not an image — it must not select image-to-video.
        assert_eq!(
            video_route(&vreq(Some("a cat"), Some("   "))),
            VideoRoute::TextToVideo
        );
    }

    #[test]
    fn route_paths_are_distinct() {
        assert_eq!(VideoRoute::TextToVideo.path(), "/v1/text_to_video");
        assert_eq!(VideoRoute::ImageToVideo.path(), "/v1/image_to_video");
    }

    #[test]
    fn status_mapping_covers_the_vendor_vocabulary() {
        assert_eq!(map_status("PENDING"), VideoStatus::Queued);
        assert_eq!(map_status("THROTTLED"), VideoStatus::Queued);
        assert_eq!(map_status("RUNNING"), VideoStatus::Running);
        assert_eq!(map_status("SUCCEEDED"), VideoStatus::Succeeded);
        assert_eq!(map_status("FAILED"), VideoStatus::Failed);
        assert_eq!(map_status("CANCELLED"), VideoStatus::Cancelled);
        assert_eq!(map_status("CANCELED"), VideoStatus::Cancelled);
        // Unknown states are optimistic — the client should poll again, not give up.
        assert_eq!(map_status("SOMETHING_NEW"), VideoStatus::Running);
    }

    #[tokio::test]
    async fn video_generate_submits_and_returns_immediately_without_polling() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text_to_video"))
            .and(header("x-runway-version", RUNWAY_VERSION))
            .and(header("authorization", "Bearer rw-test-key"))
            .and(body_partial_json(serde_json::json!({
                "model": "gen4_turbo",
                "promptText": "a red bicycle",
                "duration": 5,
                "ratio": "1280:720",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "task-1"
            })))
            .expect(1)
            .mount(&server)
            .await;
        // No task-poll mock is mounted: if video_generate polled, this test would
        // fail on an unmatched request. That is the point.

        let p = RunwayProvider::with_base_url(server.uri());
        let out = p
            .video_generate(&Ctx::new(), &test_key(), vreq(Some("a red bicycle"), None))
            .await
            .expect("submit should succeed");

        assert_eq!(out.id, "task-1");
        assert_eq!(out.status, VideoStatus::Queued);
        assert!(out.data.is_empty());
        assert_eq!(out.retry_after, Some(VIDEO_RETRY_AFTER_SECS));
    }

    #[tokio::test]
    async fn video_generate_with_an_image_uses_the_image_to_video_route() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/image_to_video"))
            .and(body_partial_json(serde_json::json!({
                "promptImage": "https://x/seed.png"
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "task-2" })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let p = RunwayProvider::with_base_url(server.uri());
        let out = p
            .video_generate(
                &Ctx::new(),
                &test_key(),
                vreq(Some("animate"), Some("https://x/seed.png")),
            )
            .await
            .expect("image-to-video submit should succeed");
        assert_eq!(out.id, "task-2");
    }

    #[tokio::test]
    async fn video_retrieve_maps_a_succeeded_task_to_artifacts() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/tasks/task-9"))
            .and(header("x-runway-version", RUNWAY_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "task-9",
                "status": "SUCCEEDED",
                "output": ["https://cdn.test/clip.mp4"]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let p = RunwayProvider::with_base_url(server.uri());
        let out = p
            .video_retrieve(&Ctx::new(), &test_key(), "task-9")
            .await
            .expect("retrieve should succeed");

        assert_eq!(out.status, VideoStatus::Succeeded);
        assert_eq!(out.data.len(), 1);
        assert_eq!(
            out.data[0].url.as_deref(),
            Some("https://cdn.test/clip.mp4")
        );
        // A terminal job must not advertise a retry cadence.
        assert_eq!(out.retry_after, None);
    }

    #[tokio::test]
    async fn video_retrieve_surfaces_a_failed_task_as_a_body_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/tasks/task-x"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "task-x",
                "status": "FAILED",
                "failureCode": "SAFETY.INPUT.TEXT"
            })))
            .mount(&server)
            .await;

        let p = RunwayProvider::with_base_url(server.uri());
        // The POLL succeeded even though the JOB failed — those are different
        // outcomes and must not collapse onto one status code.
        let out = p
            .video_retrieve(&Ctx::new(), &test_key(), "task-x")
            .await
            .expect("a failed job is still a successful poll");
        assert_eq!(out.status, VideoStatus::Failed);
        assert_eq!(out.error.as_deref(), Some("SAFETY.INPUT.TEXT"));
        assert_eq!(out.retry_after, None);
    }

    #[tokio::test]
    async fn video_retrieve_keeps_polling_hints_while_running() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/tasks/task-r"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "task-r",
                "status": "RUNNING"
            })))
            .mount(&server)
            .await;

        let p = RunwayProvider::with_base_url(server.uri());
        let out = p
            .video_retrieve(&Ctx::new(), &test_key(), "task-r")
            .await
            .unwrap();
        assert_eq!(out.status, VideoStatus::Running);
        assert_eq!(out.retry_after, Some(VIDEO_RETRY_AFTER_SECS));
    }

    #[tokio::test]
    async fn image_generate_polls_to_completion() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text_to_image"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "img-1" })),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/tasks/img-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "img-1",
                "status": "SUCCEEDED",
                "output": ["https://cdn.test/pic.png"]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let p = RunwayProvider::with_base_url(server.uri());
        let out = p
            .image_generate(
                &Ctx::new(),
                &test_key(),
                ImageGenerationRequest {
                    model: "gen4_image".into(),
                    prompt: "a bicycle".into(),
                    n: None,
                    size: Some("1024:1024".into()),
                },
            )
            .await
            .expect("image generation should resolve synchronously");

        assert_eq!(out.data.len(), 1);
        assert_eq!(out.data[0].url.as_deref(), Some("https://cdn.test/pic.png"));
    }

    #[tokio::test]
    async fn missing_task_id_on_submit_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text_to_video"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let p = RunwayProvider::with_base_url(server.uri());
        let err = p
            .video_generate(&Ctx::new(), &test_key(), vreq(Some("x"), None))
            .await
            .expect_err("an id-less acceptance is unusable");
        assert_eq!(err.kind, KgErrorKind::Internal);
    }

    #[tokio::test]
    async fn chat_is_unsupported() {
        let p = RunwayProvider::new();
        let err = p
            .chat(&Ctx::new(), &test_key(), ChatRequest::default())
            .await
            .expect_err("runway has no chat surface");
        assert_eq!(err.kind, KgErrorKind::Unsupported);
    }

    #[tokio::test]
    async fn error_429_is_retryable_and_tagged() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text_to_video"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .mount(&server)
            .await;

        let p = RunwayProvider::with_base_url(server.uri());
        let err = p
            .video_generate(&Ctx::new(), &test_key(), vreq(Some("x"), None))
            .await
            .expect_err("429 should map to an error");
        assert!(err.is_retryable());
        assert_eq!(err.status, Some(429));
        assert_eq!(err.provider.as_deref(), Some("runway"));
    }
}
