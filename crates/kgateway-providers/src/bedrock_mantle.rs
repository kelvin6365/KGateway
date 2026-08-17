//! Amazon Bedrock **Mantle** — a distinct service from `bedrock`, not a variant.
//!
//! Three things separate it from the classic Bedrock connector:
//!
//! 1. **Different host and signing service.** `https://bedrock-mantle.{region}.api.aws`,
//!    signed for the service name `bedrock-mantle` rather than `bedrock`. The SigV4
//!    machinery itself is shared with [`crate::bedrock`] — its `SigningInput`
//!    already carries `service`, so nothing about the signing changed.
//! 2. **Dual auth.** A non-empty key value is a Mantle API key sent as
//!    `Authorization: Bearer`; an empty key falls back to SigV4 over AWS
//!    credentials supplied as `ACCESS_KEY_ID:SECRET_ACCESS_KEY`.
//! 3. **Three wire surfaces on one host**, chosen by model family
//!    ([`ModelSurface`]):
//!
//! | Model prefix | Path | Wire |
//! |---|---|---|
//! | `claude*` | `/anthropic/v1/messages` | Anthropic Messages |
//! | `gpt-5*`, `gemma-4*` | `/openai/v1/chat/completions` | OpenAI |
//! | everything else | `/v1/chat/completions` | OpenAI |
//!
//! Unlike bedrock-runtime, the Anthropic version travels as an HTTP header
//! (`anthropic-version`), not as an `anthropic_version` body field, and the model
//! id is sent verbatim — there is no inference-profile rewriting.
//!
//! **Verification status: mock-only.**

use async_trait::async_trait;
use chrono::Utc;
use kgateway_core::context::Ctx;
use kgateway_core::error::{KgError, KgErrorKind};
use kgateway_core::provider::{ApiKey, ChunkStream, Provider, ProviderKey};
use kgateway_core::schema::{ChatRequest, ChatResponse};

use crate::anthropic::{AnthropicProvider, AnthropicResponse};
use crate::bedrock::{parse_credentials, sign, SigningInput};
use crate::openai::OpenAiProvider;

/// SigV4 signing service name. Deliberately NOT `bedrock` — a mismatch here
/// produces a signature that AWS rejects with an opaque 403.
const SIGNING_SERVICE: &str = "bedrock-mantle";

const DEFAULT_REGION: &str = "us-east-1";

/// Anthropic API version for the Claude surface, sent as a header here.
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct BedrockMantleProvider {
    key: ProviderKey,
    region: String,
    client: reqwest::Client,
}

/// Which of Mantle's three wire surfaces a model id selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelSurface {
    /// Anthropic Messages at `/anthropic/v1/messages`.
    Anthropic,
    /// OpenAI wire at `/openai/v1/chat/completions`.
    OpenAiNamespaced,
    /// OpenAI wire at `/v1/chat/completions`.
    OpenAiRoot,
}

impl ModelSurface {
    fn path(self) -> &'static str {
        match self {
            Self::Anthropic => "/anthropic/v1/messages",
            Self::OpenAiNamespaced => "/openai/v1/chat/completions",
            Self::OpenAiRoot => "/v1/chat/completions",
        }
    }
}

/// Classify a model id onto its wire surface.
///
/// Free function so the routing table is testable without HTTP — sending a Claude
/// model to an OpenAI path (or vice versa) fails in a way that looks like a model
/// error rather than a routing bug.
pub(crate) fn model_surface(model: &str) -> ModelSurface {
    // Mantle ids may carry a `region/` prefix; strip it before matching.
    let bare = model.rsplit('/').next().unwrap_or(model);
    let lower = bare.to_ascii_lowercase();
    if lower.starts_with("claude") || lower.starts_with("anthropic.claude") {
        ModelSurface::Anthropic
    } else if lower.starts_with("gpt-5") || lower.starts_with("gemma-4") {
        ModelSurface::OpenAiNamespaced
    } else {
        ModelSurface::OpenAiRoot
    }
}

/// Strip an optional leading `region/` selector from a model id, returning
/// `(region_override, bare_model_id)`.
pub(crate) fn split_region_prefix(model: &str) -> (Option<&str>, &str) {
    match model.split_once('/') {
        // A single leading segment that looks like an AWS region selects it.
        Some((head, rest)) if is_region_like(head) && !rest.is_empty() => (Some(head), rest),
        _ => (None, model),
    }
}

/// Heuristic for "this path segment is an AWS region", e.g. `us-east-1`.
fn is_region_like(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() >= 3
        && parts
            .last()
            .is_some_and(|p| p.chars().all(|c| c.is_ascii_digit()))
        && parts
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_alphanumeric()))
}

impl BedrockMantleProvider {
    pub fn new() -> Self {
        Self::with_region(DEFAULT_REGION)
    }

    /// `region` may also be a full `http(s)://` endpoint, which overrides the
    /// computed host — used for interface VPC endpoints and by the tests.
    pub fn with_region(region: impl Into<String>) -> Self {
        Self::with_identity("bedrock_mantle", region)
    }

    pub fn with_identity(key: impl Into<String>, region: impl Into<String>) -> Self {
        Self {
            key: ProviderKey::new(key),
            region: region.into(),
            client: crate::http::default_client(),
        }
    }

    /// Clone the parts needed to issue a request. Cheap: `reqwest::Client` is an
    /// `Arc` internally, so this shares the connection pool rather than rebuilding it.
    fn clone_shallow(&self) -> Self {
        Self {
            key: self.key.clone(),
            region: self.region.clone(),
            client: self.client.clone(),
        }
    }

    /// Base URL for this provider. A configured value starting with `http` is an
    /// explicit endpoint override; otherwise the region names the regional host.
    fn base_url(&self) -> String {
        if self.region.starts_with("http") {
            self.region.trim_end_matches('/').to_string()
        } else {
            format!("https://bedrock-mantle.{}.api.aws", self.region)
        }
    }

    /// Region used in the SigV4 credential scope. An endpoint override carries no
    /// region, so fall back to the default rather than signing with a URL.
    fn signing_region(&self) -> &str {
        if self.region.starts_with("http") {
            DEFAULT_REGION
        } else {
            &self.region
        }
    }

    fn host(&self) -> String {
        self.base_url()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or_default()
            .to_string()
    }

    /// Build the request, applying whichever auth mode the key selects.
    ///
    /// SigV4 has to sign a hash of the exact bytes, so the body is serialized here
    /// and passed through rather than handed to `.json()`.
    fn signed_request(
        &self,
        key: &ApiKey,
        path: &str,
        body: Vec<u8>,
    ) -> Result<reqwest::RequestBuilder, KgError> {
        let url = format!("{}{}", self.base_url(), path);
        let mut rb = self
            .client
            .post(&url)
            .header("content-type", "application/json");

        if key.value.trim().is_empty() {
            return Err(KgError::new(
                KgErrorKind::Auth,
                "bedrock_mantle key must be a Mantle API key, or \
                 'ACCESS_KEY_ID:SECRET_ACCESS_KEY' for SigV4",
            ));
        }

        // A colon-delimited value is an AWS credential pair → SigV4. Anything else
        // is a Mantle API key → Bearer.
        if let Ok((access_key_id, secret)) = parse_credentials(&key.value) {
            let host = self.host();
            let signed = sign(&SigningInput {
                access_key_id,
                secret,
                region: self.signing_region(),
                service: SIGNING_SERVICE,
                host: &host,
                path,
                payload: &body,
                now: Utc::now(),
            });
            rb = rb
                .header("authorization", &signed.authorization)
                .header("x-amz-date", &signed.amz_date)
                .header("x-amz-content-sha256", &signed.payload_hash)
                .header("host", host);
        } else {
            rb = rb.bearer_auth(&key.value);
        }

        Ok(rb.timeout(crate::http::REQUEST_TIMEOUT).body(body))
    }

    async fn fail_on_status(&self, resp: reqwest::Response) -> Result<reqwest::Response, KgError> {
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(KgError::provider(text, status.as_u16()).with_provider(self.key.as_str()));
        }
        Ok(resp)
    }
}

impl Default for BedrockMantleProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for BedrockMantleProvider {
    fn key(&self) -> ProviderKey {
        self.key.clone()
    }

    async fn chat(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        mut req: ChatRequest,
    ) -> Result<ChatResponse, KgError> {
        // `req.model` is the FULL routed string (`bedrock_mantle/[region/]model`),
        // so strip the provider segment first — otherwise the region test below
        // sees the provider name and never fires.
        let (region_override, bare) = split_region_prefix(req.model_id());
        let model = bare.to_string();
        // Take ownership before rewriting `req.model`, which the borrow above reads.
        let region_override = region_override.map(str::to_string);
        // The region selector is a routing hint and must never reach upstream, so
        // rewrite the request to the bare id in both cases.
        req.model = model.clone();
        let surface = model_surface(&model);

        // An explicit `region/` prefix overrides the configured region for this
        // call — host and SigV4 credential scope both have to follow it, or the
        // request is signed for the wrong region.
        //
        // An operator-pinned endpoint (a VPC endpoint, or a test server) always
        // wins: a per-request hint must not be able to redirect traffic off it.
        let routed = match region_override {
            Some(r) if !self.region.starts_with("http") => {
                Self::with_identity(self.key.as_str(), r)
            }
            _ => Self::clone_shallow(self),
        };

        match surface {
            ModelSurface::Anthropic => {
                // Reuse the Anthropic body mapper so multimodal parts, tool calls,
                // and system-prompt lifting behave identically to the native path.
                let mapper = AnthropicProvider::new();
                let body = serde_json::to_vec(&mapper.body(&req, false)).map_err(|e| {
                    KgError::new(KgErrorKind::Internal, format!("request encode error: {e}"))
                })?;
                let rb = routed
                    .signed_request(key, surface.path(), body)?
                    // Header, not a body field — this is the Mantle difference.
                    .header("anthropic-version", ANTHROPIC_VERSION);
                let resp = self
                    .fail_on_status(rb.send().await.map_err(net_err)?)
                    .await?;
                let parsed: AnthropicResponse = resp.json().await.map_err(|e| {
                    KgError::new(KgErrorKind::Internal, format!("decode error: {e}"))
                })?;
                Ok(parsed.into_chat_response())
            }
            ModelSurface::OpenAiNamespaced | ModelSurface::OpenAiRoot => {
                let mapper = OpenAiProvider::new();
                let body = serde_json::to_vec(&mapper.body(&req, false)?).map_err(|e| {
                    KgError::new(KgErrorKind::Internal, format!("request encode error: {e}"))
                })?;
                let rb = routed.signed_request(key, surface.path(), body)?;
                let resp = self
                    .fail_on_status(rb.send().await.map_err(net_err)?)
                    .await?;
                resp.json()
                    .await
                    .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))
            }
        }
    }

    async fn chat_stream(
        &self,
        _ctx: &Ctx,
        _key: &ApiKey,
        _req: ChatRequest,
    ) -> Result<ChunkStream, KgError> {
        // Streaming needs per-surface SSE handling under two different auth modes,
        // where SigV4 must sign the streaming request body identically. Deferred to
        // a follow-on rather than shipped half-working — the same stance
        // `bedrock` takes for its eventstream framing.
        Err(KgError::unsupported("bedrock_mantle streaming"))
    }
}

fn net_err(e: reqwest::Error) -> KgError {
    KgError::new(KgErrorKind::Network, e.to_string()).with_retryable(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgateway_core::schema::Message;
    use wiremock::matchers::{body_partial_json, header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn bearer_key() -> ApiKey {
        ApiKey {
            id: "k".into(),
            value: "mantle-api-key".into(),
            weight: 1,
            models: vec![],
        }
    }

    fn sigv4_key() -> ApiKey {
        ApiKey {
            id: "k".into(),
            value: "AKIDEXAMPLE:wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            weight: 1,
            models: vec![],
        }
    }

    fn req(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: vec![Message::user("hi")],
            ..Default::default()
        }
    }

    #[test]
    fn model_surface_routes_the_three_families() {
        assert_eq!(
            model_surface("claude-sonnet-4-5"),
            ModelSurface::Anthropic,
            "Claude must take the Anthropic wire"
        );
        assert_eq!(
            model_surface("anthropic.claude-3-5-sonnet"),
            ModelSurface::Anthropic
        );
        assert_eq!(model_surface("gpt-5-mini"), ModelSurface::OpenAiNamespaced);
        assert_eq!(model_surface("gemma-4-27b"), ModelSurface::OpenAiNamespaced);
        assert_eq!(model_surface("gpt-oss-120b"), ModelSurface::OpenAiRoot);
        assert_eq!(model_surface("llama-3-70b"), ModelSurface::OpenAiRoot);
        // The region prefix must not change the routing decision.
        assert_eq!(
            model_surface("us-west-2/claude-sonnet-4-5"),
            ModelSurface::Anthropic
        );
    }

    #[test]
    fn surface_paths_are_distinct() {
        assert_eq!(ModelSurface::Anthropic.path(), "/anthropic/v1/messages");
        assert_eq!(
            ModelSurface::OpenAiNamespaced.path(),
            "/openai/v1/chat/completions"
        );
        assert_eq!(ModelSurface::OpenAiRoot.path(), "/v1/chat/completions");
    }

    #[test]
    fn region_prefix_is_split_only_when_it_looks_like_a_region() {
        assert_eq!(
            split_region_prefix("us-east-1/claude-x"),
            (Some("us-east-1"), "claude-x")
        );
        assert_eq!(
            split_region_prefix("ap-southeast-2/gpt-5"),
            (Some("ap-southeast-2"), "gpt-5")
        );
        // `meta/llama` is an owner/name slug, not a region selector.
        assert_eq!(split_region_prefix("meta/llama-3"), (None, "meta/llama-3"));
        assert_eq!(split_region_prefix("claude-x"), (None, "claude-x"));
    }

    #[test]
    fn host_is_derived_from_region_and_overridable() {
        let p = BedrockMantleProvider::with_region("eu-west-1");
        assert_eq!(p.base_url(), "https://bedrock-mantle.eu-west-1.api.aws");
        assert_eq!(p.host(), "bedrock-mantle.eu-west-1.api.aws");
        assert_eq!(p.signing_region(), "eu-west-1");

        // An explicit endpoint wins, and signing falls back to the default region.
        let p = BedrockMantleProvider::with_region("http://127.0.0.1:8080");
        assert_eq!(p.base_url(), "http://127.0.0.1:8080");
        assert_eq!(p.host(), "127.0.0.1:8080");
        assert_eq!(p.signing_region(), DEFAULT_REGION);
    }

    #[tokio::test]
    async fn claude_model_uses_anthropic_path_with_version_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/anthropic/v1/messages"))
            .and(header("anthropic-version", ANTHROPIC_VERSION))
            .and(header("authorization", "Bearer mantle-api-key"))
            .and(body_partial_json(serde_json::json!({
                "messages": [{ "role": "user" }]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_1",
                "model": "claude-sonnet-4-5",
                "content": [{ "type": "text", "text": "hello" }],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 4, "output_tokens": 2 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let p = BedrockMantleProvider::with_region(server.uri());
        let out = p
            .chat(&Ctx::new(), &bearer_key(), req("claude-sonnet-4-5"))
            .await
            .expect("claude chat should succeed");

        assert_eq!(out.choices[0].message.text_content(), Some("hello"));
        assert_eq!(out.usage.total_tokens, 6);
    }

    #[tokio::test]
    async fn gpt5_model_uses_the_namespaced_openai_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/openai/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "c1",
                "object": "chat.completion",
                "model": "gpt-5-mini",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "yo" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let p = BedrockMantleProvider::with_region(server.uri());
        let out = p
            .chat(&Ctx::new(), &bearer_key(), req("gpt-5-mini"))
            .await
            .expect("gpt-5 chat should succeed");
        assert_eq!(out.choices[0].message.text_content(), Some("yo"));
    }

    #[tokio::test]
    async fn other_models_use_the_root_openai_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "c2",
                "object": "chat.completion",
                "model": "gpt-oss-120b",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "root" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let p = BedrockMantleProvider::with_region(server.uri());
        let out = p
            .chat(&Ctx::new(), &bearer_key(), req("gpt-oss-120b"))
            .await
            .expect("root-path chat should succeed");
        assert_eq!(out.choices[0].message.text_content(), Some("root"));
    }

    #[tokio::test]
    async fn credential_pair_signs_with_sigv4_instead_of_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            // SigV4 mode must send the signed header trio and NOT a bearer token.
            .and(header_exists("x-amz-date"))
            .and(header_exists("x-amz-content-sha256"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "c3",
                "object": "chat.completion",
                "model": "m",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "signed" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let p = BedrockMantleProvider::with_region(server.uri());
        let out = p
            .chat(&Ctx::new(), &sigv4_key(), req("m"))
            .await
            .expect("sigv4 chat should succeed");
        assert_eq!(out.choices[0].message.text_content(), Some("signed"));
    }

    /// The signing service name is the whole reason this is a separate connector.
    /// Signing as `bedrock` here would produce a 403 that reads like a bad key.
    #[test]
    fn signing_service_is_bedrock_mantle_not_bedrock() {
        assert_eq!(SIGNING_SERVICE, "bedrock-mantle");
        let signed = sign(&SigningInput {
            access_key_id: "AKIDEXAMPLE",
            secret: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: SIGNING_SERVICE,
            host: "bedrock-mantle.us-east-1.api.aws",
            path: "/v1/chat/completions",
            payload: b"{}",
            now: Utc::now(),
        });
        assert!(
            signed
                .authorization
                .contains("/us-east-1/bedrock-mantle/aws4_request"),
            "credential scope must name the mantle service, got {}",
            signed.authorization
        );
    }

    #[tokio::test]
    async fn empty_key_is_rejected_before_any_request() {
        let p = BedrockMantleProvider::with_region("us-east-1");
        let err = p
            .chat(
                &Ctx::new(),
                &ApiKey {
                    id: "k".into(),
                    value: "  ".into(),
                    weight: 1,
                    models: vec![],
                },
                req("m"),
            )
            .await
            .expect_err("a blank key must fail fast, not send an unauthenticated call");
        assert_eq!(err.kind, KgErrorKind::Auth);
    }

    /// Regression: the engine passes the FULL routed model, so the region test must
    /// run against `model_id()`. Against `req.model` the first segment is the
    /// provider name, the prefix never fires, and `us-west-2/claude-x` is sent
    /// upstream as the model id.
    #[tokio::test]
    async fn region_prefix_is_stripped_from_the_model_sent_upstream() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/anthropic/v1/messages"))
            // The region must NOT survive into the model id.
            .and(body_partial_json(serde_json::json!({
                "model": "claude-sonnet-4-5"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_1",
                "model": "claude-sonnet-4-5",
                "content": [{ "type": "text", "text": "ok" }],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 1, "output_tokens": 1 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let p = BedrockMantleProvider::with_region(server.uri());
        let out = p
            .chat(
                &Ctx::new(),
                &bearer_key(),
                // Exactly what dispatch_one passes down.
                req("bedrock_mantle/us-west-2/claude-sonnet-4-5"),
            )
            .await
            .expect("a region-prefixed routed model must resolve");
        assert_eq!(out.choices[0].message.text_content(), Some("ok"));
    }

    #[tokio::test]
    async fn routed_model_without_a_region_still_reaches_the_right_surface() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/anthropic/v1/messages"))
            .and(body_partial_json(serde_json::json!({
                "model": "claude-sonnet-4-5"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_2",
                "model": "claude-sonnet-4-5",
                "content": [{ "type": "text", "text": "ok" }],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 1, "output_tokens": 1 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let p = BedrockMantleProvider::with_region(server.uri());
        let out = p
            .chat(
                &Ctx::new(),
                &bearer_key(),
                req("bedrock_mantle/claude-sonnet-4-5"),
            )
            .await
            .expect("the common case must keep working");
        assert_eq!(out.choices[0].message.text_content(), Some("ok"));
    }

    #[tokio::test]
    async fn error_429_is_retryable_and_tagged() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_string("throttled"))
            .mount(&server)
            .await;

        let p = BedrockMantleProvider::with_region(server.uri());
        let err = p
            .chat(&Ctx::new(), &bearer_key(), req("m"))
            .await
            .expect_err("429 should map to an error");
        assert!(err.is_retryable());
        assert_eq!(err.status, Some(429));
        assert_eq!(err.provider.as_deref(), Some("bedrock_mantle"));
    }

    #[tokio::test]
    async fn streaming_is_unsupported_not_silently_broken() {
        let p = BedrockMantleProvider::new();
        let err = p
            .chat_stream(&Ctx::new(), &bearer_key(), req("m"))
            .await
            .err()
            .expect("streaming is not implemented in this cut");
        assert_eq!(err.kind, KgErrorKind::Unsupported);
    }
}
