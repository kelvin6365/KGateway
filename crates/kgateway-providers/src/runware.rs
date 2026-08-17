//! Runware — image and video generation over a **single-endpoint task-array**
//! protocol.
//!
//! Unlike every other connector here, Runware has no per-operation paths. Every
//! request POSTs to the base URL itself with an **empty path**, and the body is a
//! JSON *array* of task objects whose `taskType` selects the operation:
//!
//! ```text
//! POST {base}   [{ "taskType": "imageInference", "taskUUID": "...", ... }]
//! POST {base}   [{ "taskType": "videoInference", "taskUUID": "...", ... }]
//! POST {base}   [{ "taskType": "getResponse",    "taskUUID": "..." }]   ← polling
//! → { "data": [ { "taskUUID": "...", "status": "...", "videoURL": "..." } ] }
//! ```
//!
//! Two consequences worth stating plainly:
//! - `base_url` must **never** have a path appended to it.
//! - The caller mints the `taskUUID`, so it is known before the response arrives.
//!
//! Like [`crate::runway`], images poll synchronously (they settle in seconds) and
//! video does not (it takes minutes, past the gateway's request timeout).
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

const DEFAULT_BASE_URL: &str = "https://api.runware.ai/v1";

const MAX_IMAGE_POLL: Duration = Duration::from_secs(90);
const IMAGE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const VIDEO_RETRY_AFTER_SECS: u32 = 5;

pub struct RunwareProvider {
    key: ProviderKey,
    base_url: String,
    client: reqwest::Client,
}

impl RunwareProvider {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self::with_identity("runware", base_url)
    }

    pub fn with_identity(key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            key: ProviderKey::new(key),
            // Trailing slashes would turn into an empty path segment on a protocol
            // that posts to the bare base URL.
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: crate::http::default_client(),
        }
    }

    /// POST a one-task array to the bare base URL and return the first result.
    async fn send_task(
        &self,
        key: &ApiKey,
        task: serde_json::Value,
    ) -> Result<TaskResult, KgError> {
        let resp = self
            .client
            // No path is appended — that is the protocol, not an oversight.
            .post(&self.base_url)
            .bearer_auth(&key.value)
            .timeout(crate::http::REQUEST_TIMEOUT)
            .json(&serde_json::Value::Array(vec![task]))
            .send()
            .await
            .map_err(net_err)?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(KgError::provider(text, status.as_u16()).with_provider(self.key.as_str()));
        }

        let envelope: Envelope = resp
            .json()
            .await
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))?;

        // Runware reports per-task errors inside a 200 envelope, so a successful
        // HTTP status is not a successful task.
        if let Some(err) = envelope.errors.into_iter().next() {
            let detail = err
                .message
                .or(err.code)
                .unwrap_or_else(|| "runware task error".to_string());
            return Err(KgError::provider(detail, 502).with_provider(self.key.as_str()));
        }

        envelope.data.into_iter().next().ok_or_else(|| {
            KgError::new(
                KgErrorKind::Internal,
                "runware returned an empty task-result array",
            )
        })
    }

    async fn poll_task(&self, key: &ApiKey, task_uuid: &str) -> Result<TaskResult, KgError> {
        self.send_task(
            key,
            serde_json::json!({ "taskType": "getResponse", "taskUUID": task_uuid }),
        )
        .await
    }
}

impl Default for RunwareProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Map Runware's status vocabulary onto the gateway's.
///
/// Runware omits `status` entirely when a task completed inline, so an absent
/// value means success — the opposite of the usual default, and the reason this
/// takes an `Option`.
pub(crate) fn map_status(raw: Option<&str>, has_output: bool) -> VideoStatus {
    match raw.map(|s| s.to_ascii_lowercase()) {
        None => {
            if has_output {
                VideoStatus::Succeeded
            } else {
                VideoStatus::Running
            }
        }
        Some(s) => match s.as_str() {
            "success" | "succeeded" | "completed" => VideoStatus::Succeeded,
            "error" | "failed" => VideoStatus::Failed,
            "cancelled" | "canceled" => VideoStatus::Cancelled,
            "pending" | "queued" => VideoStatus::Queued,
            _ => VideoStatus::Running,
        },
    }
}

#[derive(Deserialize, Default)]
struct Envelope {
    #[serde(default)]
    data: Vec<TaskResult>,
    #[serde(default)]
    errors: Vec<TaskError>,
}

#[derive(Deserialize)]
struct TaskError {
    #[serde(default)]
    message: Option<String>,
    #[serde(default, alias = "errorId")]
    code: Option<String>,
}

#[derive(Deserialize)]
struct TaskResult {
    #[serde(default, alias = "taskUUID")]
    task_uuid: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default, alias = "videoURL")]
    video_url: Option<String>,
    #[serde(default, alias = "imageURL")]
    image_url: Option<String>,
    #[serde(default, alias = "imageBase64Data")]
    image_base64: Option<String>,
}

impl TaskResult {
    fn into_video_response(self) -> VideoResponse {
        let has_output = self.video_url.is_some();
        let status = map_status(self.status.as_deref(), has_output);
        VideoResponse {
            id: self.task_uuid,
            status,
            data: self
                .video_url
                .into_iter()
                .map(|url| VideoData {
                    url: Some(url),
                    b64_json: None,
                })
                .collect(),
            error: None,
            retry_after: (!status.is_terminal()).then_some(VIDEO_RETRY_AFTER_SECS),
        }
    }
}

#[async_trait]
impl Provider for RunwareProvider {
    fn key(&self) -> ProviderKey {
        self.key.clone()
    }

    async fn chat(
        &self,
        _ctx: &Ctx,
        _key: &ApiKey,
        _req: ChatRequest,
    ) -> Result<ChatResponse, KgError> {
        Err(KgError::unsupported("chat for runware"))
    }

    async fn chat_stream(
        &self,
        _ctx: &Ctx,
        _key: &ApiKey,
        _req: ChatRequest,
    ) -> Result<ChunkStream, KgError> {
        Err(KgError::unsupported("chat for runware"))
    }

    fn as_images(&self) -> Option<&dyn Images> {
        Some(self)
    }

    fn as_video(&self) -> Option<&dyn Video> {
        Some(self)
    }
}

#[async_trait]
impl Images for RunwareProvider {
    async fn image_generate(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: ImageGenerationRequest,
    ) -> Result<ImageResponse, KgError> {
        let task_uuid = uuid::Uuid::new_v4().to_string();
        let mut task = serde_json::json!({
            "taskType": "imageInference",
            "taskUUID": task_uuid,
            "model": req.model,
            "positivePrompt": req.prompt,
            "numberResults": req.n.unwrap_or(1),
        });
        // Runware wants explicit pixel dimensions, e.g. "1024x1024".
        if let Some((w, h)) = req.size.as_deref().and_then(parse_dimensions) {
            task["width"] = serde_json::Value::from(w);
            task["height"] = serde_json::Value::from(h);
        }

        let mut result = self.send_task(key, task).await?;

        // Bounded poll: image tasks usually complete inline, so this is normally
        // zero round-trips. It holds a provider semaphore permit while it runs.
        let deadline = tokio::time::Instant::now() + MAX_IMAGE_POLL;
        loop {
            let has_output = result.image_url.is_some() || result.image_base64.is_some();
            match map_status(result.status.as_deref(), has_output) {
                VideoStatus::Succeeded => {
                    return Ok(ImageResponse {
                        data: vec![ImageData {
                            url: result.image_url,
                            b64_json: result.image_base64,
                        }],
                    })
                }
                VideoStatus::Failed | VideoStatus::Cancelled => {
                    return Err(KgError::provider(
                        format!("runware image task {} did not succeed", result.task_uuid),
                        502,
                    )
                    .with_provider(self.key.as_str()))
                }
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(KgError::provider(
                    format!(
                        "runware image task {} did not settle within {}s",
                        result.task_uuid,
                        MAX_IMAGE_POLL.as_secs()
                    ),
                    504,
                )
                .with_provider(self.key.as_str()));
            }
            tokio::time::sleep(IMAGE_POLL_INTERVAL).await;
            result = self.poll_task(key, &task_uuid).await?;
        }
    }
}

/// Parse a `"1024x1024"` or `"1024:1024"` size token into `(width, height)`.
pub(crate) fn parse_dimensions(size: &str) -> Option<(u32, u32)> {
    let (w, h) = size.split_once(['x', 'X', ':'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

#[async_trait]
impl Video for RunwareProvider {
    async fn video_generate(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: VideoGenerationRequest,
    ) -> Result<VideoResponse, KgError> {
        // The caller mints the id, so the handle is known even if the response
        // omits it.
        let task_uuid = uuid::Uuid::new_v4().to_string();
        let mut task = serde_json::json!({
            "taskType": "videoInference",
            "taskUUID": task_uuid,
            "model": req.model,
            "deliveryMethod": "async",
        });
        if let Some(p) = &req.prompt {
            task["positivePrompt"] = serde_json::Value::String(p.clone());
        }
        if let Some(img) = &req.image {
            if !img.trim().is_empty() {
                task["frameImages"] = serde_json::json!([{ "inputImage": img }]);
            }
        }
        if let Some(d) = req.duration_seconds {
            task["duration"] = serde_json::Value::from(d);
        }
        if let Some((w, h)) = req.ratio.as_deref().and_then(parse_dimensions) {
            task["width"] = serde_json::Value::from(w);
            task["height"] = serde_json::Value::from(h);
        }
        if let Some(s) = req.seed {
            task["seed"] = serde_json::Value::from(s);
        }

        let result = self.send_task(key, task).await?;
        // Submit only — never poll to completion here.
        let id = if result.task_uuid.is_empty() {
            task_uuid
        } else {
            result.task_uuid
        };
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
        let mut result = self.poll_task(key, id).await?;
        // Echo the id the caller polled with — Runware may omit it on a pending task.
        if result.task_uuid.is_empty() {
            result.task_uuid = id.to_string();
        }
        Ok(result.into_video_response())
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
            value: "rw-api-key".into(),
            weight: 1,
            models: vec![],
        }
    }

    fn vreq() -> VideoGenerationRequest {
        VideoGenerationRequest {
            model: "klingai:5@3".into(),
            prompt: Some("a red bicycle".into()),
            image: None,
            duration_seconds: Some(5),
            ratio: Some("1280x720".into()),
            seed: None,
        }
    }

    #[test]
    fn dimensions_accept_both_separators_and_reject_junk() {
        assert_eq!(parse_dimensions("1024x1024"), Some((1024, 1024)));
        assert_eq!(parse_dimensions("1280X720"), Some((1280, 720)));
        assert_eq!(parse_dimensions("1280:720"), Some((1280, 720)));
        assert_eq!(parse_dimensions("square"), None);
        assert_eq!(parse_dimensions("1024"), None);
        assert_eq!(parse_dimensions("axb"), None);
    }

    #[test]
    fn absent_status_means_success_only_when_output_is_present() {
        // Runware omits `status` on an inline completion — absence is success there.
        assert_eq!(map_status(None, true), VideoStatus::Succeeded);
        // ...but absence with no output is still in flight, not done.
        assert_eq!(map_status(None, false), VideoStatus::Running);
        assert_eq!(map_status(Some("success"), false), VideoStatus::Succeeded);
        assert_eq!(map_status(Some("error"), false), VideoStatus::Failed);
        assert_eq!(map_status(Some("pending"), false), VideoStatus::Queued);
        assert_eq!(map_status(Some("processing"), false), VideoStatus::Running);
        assert_eq!(map_status(Some("cancelled"), false), VideoStatus::Cancelled);
    }

    #[test]
    fn base_url_never_keeps_a_trailing_slash() {
        // The protocol posts to the bare base URL, so a trailing slash would
        // create an empty path segment.
        let p = RunwareProvider::with_base_url("https://api.runware.ai/v1/");
        assert_eq!(p.base_url, "https://api.runware.ai/v1");
    }

    #[tokio::test]
    async fn video_generate_posts_a_task_array_to_the_bare_base_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            // Empty path — the whole point of this protocol.
            .and(path("/"))
            .and(header("authorization", "Bearer rw-api-key"))
            .and(body_partial_json(serde_json::json!([{
                "taskType": "videoInference",
                "model": "klingai:5@3",
                "positivePrompt": "a red bicycle",
                "duration": 5,
                "width": 1280,
                "height": 720,
            }])))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "taskUUID": "uuid-1", "status": "pending" }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let p = RunwareProvider::with_base_url(server.uri());
        let out = p
            .video_generate(&Ctx::new(), &test_key(), vreq())
            .await
            .expect("submit should succeed");

        assert_eq!(out.id, "uuid-1");
        assert_eq!(out.status, VideoStatus::Queued);
        assert!(out.data.is_empty());
    }

    #[tokio::test]
    async fn video_retrieve_polls_with_a_get_response_task() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(serde_json::json!([{
                "taskType": "getResponse",
                "taskUUID": "uuid-9",
            }])))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{
                    "taskUUID": "uuid-9",
                    "status": "success",
                    "videoURL": "https://cdn.test/clip.mp4"
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let p = RunwareProvider::with_base_url(server.uri());
        let out = p
            .video_retrieve(&Ctx::new(), &test_key(), "uuid-9")
            .await
            .expect("poll should succeed");

        assert_eq!(out.status, VideoStatus::Succeeded);
        assert_eq!(
            out.data[0].url.as_deref(),
            Some("https://cdn.test/clip.mp4")
        );
        assert_eq!(out.retry_after, None);
    }

    /// Runware reports task failures INSIDE a 200 envelope, so HTTP success is not
    /// task success. Missing this would surface an error as an empty result.
    #[tokio::test]
    async fn task_level_errors_inside_a_200_envelope_become_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errors": [{ "message": "invalid model", "errorId": "E_MODEL" }]
            })))
            .mount(&server)
            .await;

        let p = RunwareProvider::with_base_url(server.uri());
        let err = p
            .video_generate(&Ctx::new(), &test_key(), vreq())
            .await
            .expect_err("a task-level error must not read as success");
        assert_eq!(err.status, Some(502));
        assert_eq!(err.provider.as_deref(), Some("runware"));
    }

    #[tokio::test]
    async fn empty_data_array_is_an_error_not_a_silent_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": [] })),
            )
            .mount(&server)
            .await;

        let p = RunwareProvider::with_base_url(server.uri());
        let err = p
            .video_generate(&Ctx::new(), &test_key(), vreq())
            .await
            .expect_err("an empty result array is unusable");
        assert_eq!(err.kind, KgErrorKind::Internal);
    }

    #[tokio::test]
    async fn image_generate_returns_an_inline_result_without_polling() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(serde_json::json!([{
                "taskType": "imageInference",
                "width": 1024,
                "height": 1024,
            }])))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{
                    "taskUUID": "img-1",
                    "imageURL": "https://cdn.test/pic.png"
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let p = RunwareProvider::with_base_url(server.uri());
        let out = p
            .image_generate(
                &Ctx::new(),
                &test_key(),
                ImageGenerationRequest {
                    model: "runware:101@1".into(),
                    prompt: "a bicycle".into(),
                    n: None,
                    size: Some("1024x1024".into()),
                },
            )
            .await
            .expect("inline image result should resolve with no poll");

        assert_eq!(out.data[0].url.as_deref(), Some("https://cdn.test/pic.png"));
    }

    #[tokio::test]
    async fn chat_is_unsupported() {
        let p = RunwareProvider::new();
        let err = p
            .chat(&Ctx::new(), &test_key(), ChatRequest::default())
            .await
            .expect_err("runware has no chat surface");
        assert_eq!(err.kind, KgErrorKind::Unsupported);
    }

    #[tokio::test]
    async fn error_429_is_retryable_and_tagged() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .mount(&server)
            .await;

        let p = RunwareProvider::with_base_url(server.uri());
        let err = p
            .video_generate(&Ctx::new(), &test_key(), vreq())
            .await
            .expect_err("429 should map to an error");
        assert!(err.is_retryable());
        assert_eq!(err.status, Some(429));
        assert_eq!(err.provider.as_deref(), Some("runware"));
    }
}
