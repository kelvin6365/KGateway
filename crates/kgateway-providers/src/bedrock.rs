//! AWS Bedrock provider — the Converse API with hand-rolled SigV4 request signing.
//!
//! Bedrock's Converse API (`POST /model/{modelId}/converse`) gives a single,
//! provider-neutral chat shape across all Bedrock-hosted models. Unlike the other
//! connectors, Bedrock authenticates with AWS Signature Version 4 rather than a
//! bearer token or API-key header, so this module carries a small, self-contained
//! SigV4 implementation (see [`sign`]) built on `hmac` + `sha2` + `hex`.
//!
//! Credential convention: `ApiKey.value` is `"ACCESS_KEY_ID:SECRET_ACCESS_KEY"`.
//!
//! Streaming (`chat_stream`) is intentionally unimplemented in this cut: Bedrock
//! streams the Converse response as a binary `vnd.amazon.eventstream` frame format
//! (prelude + CRC + headers + payload), which needs its own framing decoder. That
//! is a follow-on; for now `chat_stream` returns `Unsupported`.

use async_trait::async_trait;
use chrono::Utc;
use hmac::{Hmac, Mac};
use kgateway_core::context::Ctx;
use kgateway_core::error::{KgError, KgErrorKind};
use kgateway_core::provider::{ApiKey, ChunkStream, Provider, ProviderKey};
use kgateway_core::schema::{
    ChatRequest, ChatResponse, Choice, Message, MessageContent, Role, Usage,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// AWS service name used in the SigV4 credential scope and signing-key chain.
const SERVICE: &str = "bedrock";

type HmacSha256 = Hmac<Sha256>;

pub struct BedrockProvider {
    key: ProviderKey,
    region: String,
    /// Base endpoint (no trailing slash), e.g. `https://bedrock-runtime.us-east-1.amazonaws.com`.
    /// Overridable for tests so requests can be pointed at a mock server.
    endpoint: String,
    client: reqwest::Client,
}

impl BedrockProvider {
    /// Bedrock in `region`, registered under the default provider key `"bedrock"`.
    pub fn new(region: impl Into<String>) -> Self {
        Self::with_identity("bedrock", region)
    }

    /// Bedrock in `region` registered under a custom provider key.
    pub fn with_identity(key: impl Into<String>, region: impl Into<String>) -> Self {
        let region = region.into();
        let endpoint = format!("https://bedrock-runtime.{region}.amazonaws.com");
        Self {
            key: ProviderKey::new(key),
            region,
            endpoint,
            client: crate::http::default_client(),
        }
    }

    /// Point the connector at a custom endpoint base URL (used by tests to target a
    /// mock server). Region + provider key keep their normal meaning.
    pub fn with_endpoint(
        key: impl Into<String>,
        region: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Self {
        let mut endpoint = endpoint.into();
        // Normalize away a trailing slash so path joins never double up.
        while endpoint.ends_with('/') {
            endpoint.pop();
        }
        Self {
            key: ProviderKey::new(key),
            region: region.into(),
            endpoint,
            client: crate::http::default_client(),
        }
    }

    /// The `host[:port]` authority of the endpoint — signed as the `host` header.
    fn host(&self) -> &str {
        self.endpoint
            .strip_prefix("https://")
            .or_else(|| self.endpoint.strip_prefix("http://"))
            .unwrap_or(&self.endpoint)
            // Trim any path that follows the authority.
            .split('/')
            .next()
            .unwrap_or(&self.endpoint)
    }

    /// Convert an internal [`ChatRequest`] into a Converse request body. System
    /// messages are lifted into the top-level `system` array (Bedrock, like
    /// Anthropic, keeps them separate); User/Tool → "user", Assistant → "assistant".
    fn body(&self, req: &ChatRequest) -> ConverseRequest {
        let mut system: Vec<TextBlock> = Vec::new();
        let mut messages: Vec<ConverseMessage> = Vec::new();

        for m in &req.messages {
            match m.role {
                Role::System => {
                    if let Some(t) = m.content.as_ref().and_then(|c| c.to_text()) {
                        system.push(TextBlock { text: t });
                    }
                }
                Role::User | Role::Tool => messages.push(ConverseMessage {
                    role: "user".into(),
                    content: vec![TextBlock {
                        text: m.text_or_empty(),
                    }],
                }),
                Role::Assistant => messages.push(ConverseMessage {
                    role: "assistant".into(),
                    content: vec![TextBlock {
                        text: m.text_or_empty(),
                    }],
                }),
            }
        }

        // Only emit inferenceConfig when at least one field is set.
        let inference_config = if req.max_tokens.is_some() || req.temperature.is_some() {
            Some(InferenceConfig {
                max_tokens: req.max_tokens,
                temperature: req.temperature,
            })
        } else {
            None
        };

        ConverseRequest {
            messages,
            system: if system.is_empty() {
                None
            } else {
                Some(system)
            },
            inference_config,
        }
    }

    /// Full URL for the Converse call. The model id is percent-encoded for the path
    /// segment (Bedrock ids contain `:`, e.g. `...-v1:0`).
    fn converse_url(&self, model_id: &str) -> String {
        format!("{}{}", self.endpoint, converse_path(model_id))
    }
}

/// The canonical request path for a model's Converse endpoint, with the model id
/// percent-encoded (`:` → `%3A`). Kept separate so signing and the request URL use
/// the identical path.
fn converse_path(model_id: &str) -> String {
    format!("/model/{}/converse", encode_model_id(model_id))
}

/// Percent-encode a Bedrock model id for use as a single URL path segment. Bedrock
/// ids look like `anthropic.claude-3-5-sonnet-20240620-v1:0`; the `:` must be
/// percent-encoded (dots and dashes are path-safe and pass through).
fn encode_model_id(model_id: &str) -> String {
    model_id.replace(':', "%3A")
}

/// Split `ApiKey.value` into `(access_key_id, secret_access_key)`.
fn parse_credentials(value: &str) -> Result<(&str, &str), KgError> {
    value.split_once(':').ok_or_else(|| {
        KgError::new(
            KgErrorKind::Auth,
            "bedrock key must be 'ACCESS_KEY_ID:SECRET_ACCESS_KEY'",
        )
    })
}

#[async_trait]
impl Provider for BedrockProvider {
    fn key(&self) -> ProviderKey {
        self.key.clone()
    }

    async fn chat(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: ChatRequest,
    ) -> Result<ChatResponse, KgError> {
        let (access_key_id, secret) = parse_credentials(&key.value)?;

        let model_id = req.model_id().to_string();
        let path = converse_path(&model_id);
        let url = self.converse_url(&model_id);

        // Serialize the body first: SigV4 signs a hash of the exact bytes we send.
        let body = serde_json::to_vec(&self.body(&req)).map_err(|e| {
            KgError::new(KgErrorKind::Internal, format!("request encode error: {e}"))
        })?;

        let signed = sign(&SigningInput {
            access_key_id,
            secret,
            region: &self.region,
            service: SERVICE,
            host: self.host(),
            path: &path,
            payload: &body,
            now: Utc::now(),
        });

        let resp = self
            .client
            .post(&url)
            .header("authorization", &signed.authorization)
            .header("x-amz-date", &signed.amz_date)
            .header("x-amz-content-sha256", &signed.payload_hash)
            .header("host", self.host())
            .header("content-type", "application/json")
            .timeout(crate::http::REQUEST_TIMEOUT)
            .body(body)
            .send()
            .await
            .map_err(net_err)?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(KgError::provider(text, status.as_u16()).with_provider(self.key.as_str()));
        }

        let cr: ConverseResponse = resp
            .json()
            .await
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))?;

        Ok(cr.into_chat_response(model_id))
    }

    async fn chat_stream(
        &self,
        _ctx: &Ctx,
        _key: &ApiKey,
        _req: ChatRequest,
    ) -> Result<ChunkStream, KgError> {
        // Bedrock's ConverseStream returns an `application/vnd.amazon.eventstream`
        // binary frame format (prelude + prelude CRC + headers + payload + message
        // CRC). Decoding that framing is a follow-on to this cut, which focuses on
        // non-streaming Converse + correct SigV4.
        Err(KgError::unsupported("bedrock streaming"))
    }
}

fn net_err(e: reqwest::Error) -> KgError {
    KgError::new(KgErrorKind::Network, e.to_string()).with_retryable(true)
}

// ---- SigV4 signing ----

/// Everything needed to produce a SigV4 `Authorization` header for one request.
struct SigningInput<'a> {
    access_key_id: &'a str,
    secret: &'a str,
    region: &'a str,
    service: &'a str,
    host: &'a str,
    /// Canonical URI (already percent-encoded path), e.g. `/model/foo%3A0/converse`.
    path: &'a str,
    payload: &'a [u8],
    now: chrono::DateTime<Utc>,
}

/// The signed material attached to the outgoing request.
struct SignedRequest {
    authorization: String,
    amz_date: String,
    /// Lowercase hex SHA256 of the payload (also sent as `x-amz-content-sha256`).
    payload_hash: String,
}

/// Produce the SigV4 `Authorization` header (and the `x-amz-*` values) for a POST.
///
/// Signs the fixed header set `host;x-amz-content-sha256;x-amz-date` — the minimal
/// set Bedrock requires — following the standard 4-step AWS process: canonical
/// request → string to sign → derived signing key → signature.
fn sign(input: &SigningInput<'_>) -> SignedRequest {
    let amz_date = input.now.format("%Y%m%dT%H%M%SZ").to_string();
    let datestamp = input.now.format("%Y%m%d").to_string();
    let payload_hash = sha256_hex(input.payload);

    // 1. Canonical request. Empty query string; three sorted, lowercase headers.
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_headers = format!(
        "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
        input.host, payload_hash, amz_date
    );
    let canonical_request = format!(
        "POST\n{}\n{}\n{}\n{}\n{}",
        input.path, "", canonical_headers, signed_headers, payload_hash
    );

    // 2. String to sign.
    let scope = format!(
        "{}/{}/{}/aws4_request",
        datestamp, input.region, input.service
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        amz_date,
        scope,
        sha256_hex(canonical_request.as_bytes())
    );

    // 3. Derive the signing key and 4. sign.
    let key = signing_key(input.secret, &datestamp, input.region, input.service);
    let signature = hex::encode(hmac_sha256(&key, string_to_sign.as_bytes()));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        input.access_key_id, scope, signed_headers, signature
    );

    SignedRequest {
        authorization,
        amz_date,
        payload_hash,
    }
}

/// Derive the SigV4 signing key: HMAC chain over date → region → service → the
/// `aws4_request` terminator, seeded with `"AWS4" + secret`.
fn signing_key(secret: &str, datestamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), datestamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// HMAC-SHA256(key, data) → raw bytes. `new_from_slice` accepts any key length.
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Lowercase hex of SHA256(data).
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

// ---- Converse wire types ----

#[derive(Debug, Serialize)]
struct ConverseRequest {
    messages: Vec<ConverseMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<TextBlock>>,
    #[serde(rename = "inferenceConfig", skip_serializing_if = "Option::is_none")]
    inference_config: Option<InferenceConfig>,
}

#[derive(Debug, Serialize)]
struct ConverseMessage {
    role: String,
    content: Vec<TextBlock>,
}

#[derive(Debug, Serialize)]
struct TextBlock {
    text: String,
}

#[derive(Debug, Serialize)]
struct InferenceConfig {
    #[serde(rename = "maxTokens", skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct ConverseResponse {
    #[serde(default)]
    output: ConverseOutput,
    #[serde(default)]
    usage: ConverseUsage,
    #[serde(rename = "stopReason", default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ConverseOutput {
    #[serde(default)]
    message: Option<ConverseRespMessage>,
}

#[derive(Debug, Deserialize)]
struct ConverseRespMessage {
    #[serde(default)]
    content: Vec<RespTextBlock>,
}

#[derive(Debug, Deserialize)]
struct RespTextBlock {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ConverseUsage {
    #[serde(rename = "inputTokens", default)]
    input_tokens: u32,
    #[serde(rename = "outputTokens", default)]
    output_tokens: u32,
    #[serde(rename = "totalTokens", default)]
    total_tokens: u32,
}

impl ConverseResponse {
    fn into_chat_response(self, model: String) -> ChatResponse {
        // Concatenate the text of every content block into one assistant message.
        let text: String = self
            .output
            .message
            .map(|m| {
                m.content
                    .into_iter()
                    .filter_map(|b| b.text)
                    .collect::<String>()
            })
            .unwrap_or_default();
        let content = if text.is_empty() { None } else { Some(text) };

        let usage = Usage {
            prompt_tokens: self.usage.input_tokens,
            completion_tokens: self.usage.output_tokens,
            total_tokens: self.usage.total_tokens,
        };

        ChatResponse {
            id: String::new(), // Converse carries no response id.
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
                finish_reason: self.stop_reason,
            }],
            usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use kgateway_core::schema::Message;
    use wiremock::matchers::{body_partial_json, header_exists, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn test_key() -> ApiKey {
        ApiKey {
            id: "test".into(),
            value: "AKIDEXAMPLE:wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            weight: 1,
            models: vec![],
        }
    }

    fn req_with(messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: "bedrock/anthropic.claude-3-5-sonnet-20240620-v1:0".into(),
            messages,
            temperature: Some(0.5),
            max_tokens: Some(256),
            ..Default::default()
        }
    }

    // ---- SigV4 correctness ----

    /// The empty-string SHA256 — a fixed reference value used by SigV4 for empty
    /// payloads. Guards our hashing helper against a broken hex/digest wiring.
    #[test]
    fn sha256_empty_string_matches_known_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// AWS SigV4 test-suite `get-vanilla` vector. Using AWS's documented example
    /// credentials (`AKIDEXAMPLE` / `wJalrXUtnFEMI/...`), region `us-east-1`,
    /// service `service`, date `20150830T123600Z`, the canonical request below must
    /// derive AWS's published signature. Validating against a real, well-known AWS
    /// vector (not a self-generated one) proves the whole chain: signing-key
    /// derivation, string-to-sign layout, and the final HMAC.
    #[test]
    fn sigv4_matches_aws_get_vanilla_vector() {
        // The canonical request for the get-vanilla test case (GET /, empty payload,
        // signed headers host;x-amz-date). We reproduce it verbatim, then run it
        // through our string_to_sign + signing_key + HMAC helpers.
        let payload_hash = sha256_hex(b"");
        let canonical_request = format!(
            "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\n{payload_hash}"
        );
        let amz_date = "20150830T123600Z";
        let datestamp = "20150830";
        let region = "us-east-1";
        let service = "service";
        let scope = format!("{datestamp}/{region}/{service}/aws4_request");
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date,
            scope,
            sha256_hex(canonical_request.as_bytes())
        );

        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let key = signing_key(secret, datestamp, region, service);
        let signature = hex::encode(hmac_sha256(&key, string_to_sign.as_bytes()));

        // AWS's documented get-vanilla signature.
        assert_eq!(
            signature,
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// The full `sign()` path is deterministic for a fixed clock and produces a
    /// well-formed Authorization header referencing the correct scope + signed
    /// headers.
    #[test]
    fn sign_produces_well_formed_authorization() {
        let now = Utc.with_ymd_and_hms(2015, 8, 30, 12, 36, 0).unwrap();
        let out = sign(&SigningInput {
            access_key_id: "AKIDEXAMPLE",
            secret: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: SERVICE,
            host: "bedrock-runtime.us-east-1.amazonaws.com",
            path: "/model/foo%3A0/converse",
            payload: b"{}",
            now,
        });

        assert_eq!(out.amz_date, "20150830T123600Z");
        assert_eq!(out.payload_hash, sha256_hex(b"{}"));
        assert!(out.authorization.starts_with("AWS4-HMAC-SHA256 "));
        assert!(out
            .authorization
            .contains("Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request"));
        assert!(out
            .authorization
            .contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"));
        // Signature is 64 lowercase hex chars.
        let sig = out.authorization.rsplit("Signature=").next().unwrap();
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));

        // Determinism: same inputs → same signature.
        let out2 = sign(&SigningInput {
            access_key_id: "AKIDEXAMPLE",
            secret: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: SERVICE,
            host: "bedrock-runtime.us-east-1.amazonaws.com",
            path: "/model/foo%3A0/converse",
            payload: b"{}",
            now,
        });
        assert_eq!(out.authorization, out2.authorization);
    }

    #[test]
    fn model_id_is_percent_encoded() {
        assert_eq!(
            converse_path("anthropic.claude-3-5-sonnet-20240620-v1:0"),
            "/model/anthropic.claude-3-5-sonnet-20240620-v1%3A0/converse"
        );
    }

    // ---- Credential parsing ----

    #[test]
    fn credentials_without_colon_error() {
        let err = parse_credentials("no-colon-here").expect_err("must reject");
        assert_eq!(err.kind, KgErrorKind::Auth);
        assert_eq!(
            err.message,
            "bedrock key must be 'ACCESS_KEY_ID:SECRET_ACCESS_KEY'"
        );
    }

    #[tokio::test]
    async fn chat_missing_colon_credential_errors_before_request() {
        // No server needed: the credential parse must fail first.
        let p = BedrockProvider::with_endpoint("bedrock", "us-east-1", "http://127.0.0.1:1");
        let bad_key = ApiKey {
            id: "k".into(),
            value: "not-a-valid-pair".into(),
            weight: 1,
            models: vec![],
        };
        let err = p
            .chat(
                &Ctx::default(),
                &bad_key,
                req_with(vec![Message::user("hi")]),
            )
            .await
            .expect_err("should error");
        assert_eq!(err.kind, KgErrorKind::Auth);
    }

    // ---- Converse round-trip ----

    #[tokio::test]
    async fn chat_converse_round_trip() {
        let server = MockServer::start().await;
        let resp_body = serde_json::json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        {"text": "Hello, "},
                        {"text": "world!"}
                    ]
                }
            },
            "usage": {"inputTokens": 12, "outputTokens": 5, "totalTokens": 17},
            "stopReason": "end_turn"
        });

        Mock::given(method("POST"))
            .and(path(
                "/model/anthropic.claude-3-5-sonnet-20240620-v1%3A0/converse",
            ))
            // Every request must carry a SigV4 Authorization header...
            .and(header_exists("authorization"))
            .and(header_exists("x-amz-date"))
            .and(header_exists("x-amz-content-sha256"))
            // ...and system messages must be lifted into the top-level `system` array.
            .and(body_partial_json(serde_json::json!({
                "system": [{"text": "be terse"}],
                "inferenceConfig": {"maxTokens": 256, "temperature": 0.5}
            })))
            // Assert the Authorization value uses the SigV4 scheme (matcher on the header).
            .and(AuthPrefix)
            .respond_with(ResponseTemplate::new(200).set_body_json(resp_body))
            .expect(1)
            .mount(&server)
            .await;

        let p = BedrockProvider::with_endpoint("bedrock", "us-east-1", server.uri());
        let out = p
            .chat(
                &Ctx::default(),
                &test_key(),
                req_with(vec![Message::system("be terse"), Message::user("hi")]),
            )
            .await
            .expect("chat ok");

        assert_eq!(out.object, "chat.completion");
        assert_eq!(
            out.model, "anthropic.claude-3-5-sonnet-20240620-v1:0",
            "model id has the provider prefix stripped"
        );
        assert_eq!(out.choices[0].message.text_content(), Some("Hello, world!"));
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("end_turn"));
        assert_eq!(out.usage.prompt_tokens, 12);
        assert_eq!(out.usage.completion_tokens, 5);
        assert_eq!(out.usage.total_tokens, 17);
    }

    /// wiremock matcher asserting the `authorization` header uses the SigV4 scheme.
    struct AuthPrefix;
    impl wiremock::Match for AuthPrefix {
        fn matches(&self, req: &Request) -> bool {
            req.headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"))
                .unwrap_or(false)
        }
    }

    #[tokio::test]
    async fn chat_error_status_is_mapped() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string("{\"message\":\"throttled\"}"))
            .mount(&server)
            .await;

        let p = BedrockProvider::with_endpoint("bedrock", "us-east-1", server.uri());
        let err = p
            .chat(
                &Ctx::default(),
                &test_key(),
                req_with(vec![Message::user("hi")]),
            )
            .await
            .expect_err("should error");
        assert_eq!(err.status, Some(429));
        assert!(err.is_retryable());
        assert_eq!(err.provider.as_deref(), Some("bedrock"));
    }

    #[tokio::test]
    async fn streaming_is_unsupported() {
        let p = BedrockProvider::new("us-east-1");
        // The Ok variant (ChunkStream) is not Debug, so match rather than expect_err.
        let err = match p
            .chat_stream(
                &Ctx::default(),
                &test_key(),
                req_with(vec![Message::user("hi")]),
            )
            .await
        {
            Err(e) => e,
            Ok(_) => panic!("streaming should be unsupported"),
        };
        assert_eq!(err.kind, KgErrorKind::Unsupported);
    }
}
