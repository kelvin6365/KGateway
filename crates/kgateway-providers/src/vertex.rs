//! Google **Vertex AI** — Gemini and Claude on Google Cloud.
//!
//! Distinct from [`crate::gemini`] in three ways that matter:
//!
//! | | `gemini` | `vertex` |
//! |---|---|---|
//! | Auth | `x-goog-api-key` header | OAuth2 Bearer (ADC / service account), or `?key=` for Gemini models |
//! | Host | one global host | computed from the region |
//! | Path | `/v1beta/models/{m}:generateContent` | project- and publisher-scoped resource path |
//!
//! **Configuration.** Project and location travel in `base_url` as
//! `"{project}/{location}"` (e.g. `"my-proj/us-central1"`), because
//! `ProviderConfig` carries no provider-specific fields — the same convention
//! Bedrock uses for its region. A `base_url` starting with `http` is a full
//! endpoint override used by tests and private endpoints.
//!
//! **Credentials** are discriminated on the *shape* of the key value
//! ([`CredentialMode`]), mirroring Bedrock's `"ACCESS:SECRET"` precedent:
//!
//! | Key value | Mode |
//! |---|---|
//! | empty, `adc`, `metadata` | GCE/GKE metadata-server token |
//! | starts with `{` | inline service-account JSON → RS256 JWT → OAuth2 |
//! | starts with `/` or `~`, or ends `.json` | service-account file → same |
//! | anything else | Google API key, sent as `?key=` |
//!
//! The API-key mode only works for Gemini/Gemma publisher models; Google rejects
//! it on the Anthropic publisher path, so that combination returns a clear
//! `Unsupported` naming the required credential rather than an opaque 403.
//!
//! RS256 is signed with `ring`, which is already in the dependency tree via
//! rustls — deliberately not the `rsa` crate.
//!
//! **Verification status: mock-only.**

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine as _;
use kgateway_core::context::Ctx;
use kgateway_core::error::{KgError, KgErrorKind};
use kgateway_core::provider::{ApiKey, ChunkStream, Provider, ProviderKey};
use kgateway_core::schema::{ChatRequest, ChatResponse};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::gemini::{GeminiProvider, GeminiResponse};

const DEFAULT_LOCATION: &str = "us-central1";
const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const METADATA_TOKEN_URL: &str = "http://metadata.google.internal/computeMetadata/v1/\
     instance/service-accounts/default/token";

/// Refresh this long before a token's stated expiry, so an in-flight request
/// never races the boundary.
const TOKEN_REFRESH_SKEW: Duration = Duration::from_secs(60);

/// How a key value resolves to a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CredentialMode {
    /// Application Default Credentials via the GCE/GKE metadata server.
    Metadata,
    /// Inline service-account JSON.
    ServiceAccountJson,
    /// Path to a service-account JSON file.
    ServiceAccountFile,
    /// A plain Google API key, passed as `?key=`. Gemini/Gemma publishers only.
    ApiKey,
}

/// Classify a key value. Free function so the discrimination rule is testable
/// without any network or filesystem access.
pub(crate) fn credential_mode(value: &str) -> CredentialMode {
    let v = value.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("adc") || v.eq_ignore_ascii_case("metadata") {
        CredentialMode::Metadata
    } else if v.starts_with('{') {
        CredentialMode::ServiceAccountJson
    } else if v.starts_with('/') || v.starts_with('~') || v.ends_with(".json") {
        CredentialMode::ServiceAccountFile
    } else {
        CredentialMode::ApiKey
    }
}

/// Which Vertex publisher serves a model, which selects the URL shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Publisher {
    Google,
    Anthropic,
}

/// Map a model id onto its publisher. Free function — routing a Claude model to
/// the Google publisher path 404s in a way that reads like a bad model name.
pub(crate) fn publisher_for(model: &str) -> Publisher {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("claude") {
        Publisher::Anthropic
    } else {
        Publisher::Google
    }
}

/// Compute the regional Vertex host.
///
/// `global` and the `us`/`eu` multi-region pools have their own hostnames; every
/// other region is a plain `{region}-` prefix.
pub(crate) fn vertex_host(location: &str) -> String {
    match location {
        "global" => "https://aiplatform.googleapis.com".to_string(),
        "us" | "eu" => format!("https://aiplatform.{location}.rep.googleapis.com"),
        other => format!("https://{other}-aiplatform.googleapis.com"),
    }
}

/// Split the configured `base_url` into `(project, location)`, or recognise an
/// explicit endpoint override.
pub(crate) fn parse_target(base_url: &str) -> (Option<String>, String, String) {
    if let Some(rest) = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
    {
        // Endpoint override: `scheme://host[:port]` optionally followed by
        // `/project/location`. The scheme is preserved on the endpoint; anything
        // after the authority is the project/location pair.
        let scheme = if base_url.starts_with("https://") {
            "https://"
        } else {
            "http://"
        };
        let rest = rest.trim_end_matches('/');
        let (authority, tail) = match rest.split_once('/') {
            Some((a, t)) => (a, t),
            None => (rest, ""),
        };
        let (project, location) = match tail.split_once('/') {
            Some((p, l)) if !l.is_empty() => (p.to_string(), l.to_string()),
            // No project/location given — leave the project EMPTY rather than
            // inventing one. A bogus id would silently build a valid-looking path
            // that always 404s; an empty one is visible in the URL immediately.
            _ if tail.is_empty() => (String::new(), DEFAULT_LOCATION.to_string()),
            _ => (tail.to_string(), DEFAULT_LOCATION.to_string()),
        };
        return (Some(format!("{scheme}{authority}")), project, location);
    }
    match base_url.split_once('/') {
        Some((project, location)) if !location.is_empty() => (
            None,
            project.to_string(),
            location.trim_end_matches('/').to_string(),
        ),
        _ => (None, base_url.to_string(), DEFAULT_LOCATION.to_string()),
    }
}

/// A cached OAuth2 access token and the instant it stops being usable.
#[derive(Clone)]
struct CachedToken {
    value: String,
    expires_at: SystemTime,
}

impl CachedToken {
    fn is_fresh(&self) -> bool {
        SystemTime::now() + TOKEN_REFRESH_SKEW < self.expires_at
    }
}

pub struct VertexProvider {
    key: ProviderKey,
    /// Explicit endpoint override, when configured.
    endpoint: Option<String>,
    project: String,
    location: String,
    client: reqwest::Client,
    /// OAuth tokens cached **per credential**, keyed by `ApiKey::id`.
    ///
    /// A single shared slot would be wrong twice over: with two keys configured
    /// (say two service accounts, or one SA plus `adc`) a request dispatched under
    /// key B would reuse key A's token and authenticate as the wrong principal;
    /// and after a 401 the engine rotates to a sibling key, which would then be
    /// handed the same revoked token and fail identically.
    tokens: Arc<RwLock<HashMap<String, CachedToken>>>,
}

impl VertexProvider {
    pub fn new() -> Self {
        Self::with_base_url(String::new())
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self::with_identity("vertex", base_url)
    }

    pub fn with_identity(key: impl Into<String>, base_url: impl Into<String>) -> Self {
        let (endpoint, project, location) = parse_target(&base_url.into());
        Self {
            key: ProviderKey::new(key),
            endpoint,
            project,
            location,
            client: crate::http::default_client(),
            tokens: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn host(&self) -> String {
        self.endpoint
            .clone()
            .unwrap_or_else(|| vertex_host(&self.location))
    }

    /// Resource path for a chat call.
    pub(crate) fn chat_path(&self, model: &str) -> String {
        match publisher_for(model) {
            Publisher::Google => format!(
                "/v1/projects/{}/locations/{}/publishers/google/models/{}:generateContent",
                self.project, self.location, model
            ),
            Publisher::Anthropic => format!(
                "/v1/projects/{}/locations/{}/publishers/anthropic/models/{}:rawPredict",
                self.project, self.location, model
            ),
        }
    }

    /// Resolve an OAuth2 access token, using the cache when it is still fresh.
    async fn access_token(&self, key: &ApiKey) -> Result<String, KgError> {
        if let Some(t) = self.tokens.read().await.get(&key.id) {
            if t.is_fresh() {
                return Ok(t.value.clone());
            }
        }

        let fetched = match credential_mode(&key.value) {
            CredentialMode::Metadata => self.metadata_token().await?,
            CredentialMode::ServiceAccountJson => {
                self.service_account_token(key.value.trim()).await?
            }
            CredentialMode::ServiceAccountFile => {
                let path = expand_home(key.value.trim());
                let json = std::fs::read_to_string(&path).map_err(|e| {
                    KgError::new(
                        KgErrorKind::Auth,
                        format!("cannot read vertex service-account file: {e}"),
                    )
                })?;
                self.service_account_token(&json).await?
            }
            // Handled before this is called.
            CredentialMode::ApiKey => {
                return Err(KgError::new(
                    KgErrorKind::Internal,
                    "api-key mode does not use OAuth tokens",
                ))
            }
        };

        self.tokens
            .write()
            .await
            .insert(key.id.clone(), fetched.clone());
        Ok(fetched.value)
    }

    /// Fetch a token from the GCE/GKE metadata server.
    async fn metadata_token(&self) -> Result<CachedToken, KgError> {
        let resp = self
            .client
            .get(METADATA_TOKEN_URL)
            .header("Metadata-Flavor", "Google")
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| {
                KgError::new(
                    KgErrorKind::Auth,
                    format!("vertex metadata-server token fetch failed: {e}"),
                )
            })?;
        Self::decode_token(resp).await
    }

    /// Exchange a service-account JWT assertion for an access token.
    async fn service_account_token(&self, json: &str) -> Result<CachedToken, KgError> {
        let sa: ServiceAccount = serde_json::from_str(json).map_err(|e| {
            KgError::new(
                KgErrorKind::Auth,
                format!("vertex service-account JSON is malformed: {e}"),
            )
        })?;
        let assertion = build_assertion(&sa, now_secs())?;
        let resp = self
            .client
            .post(&sa.token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", &assertion),
            ])
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| {
                KgError::new(
                    KgErrorKind::Auth,
                    format!("vertex token exchange failed: {e}"),
                )
            })?;
        Self::decode_token(resp).await
    }

    async fn decode_token(resp: reqwest::Response) -> Result<CachedToken, KgError> {
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            // Token-endpoint failures are an auth problem, not a provider outage —
            // classifying them as Auth keeps key rotation (401-403) working.
            return Err(KgError::new(
                KgErrorKind::Auth,
                format!("vertex token endpoint returned {status}: {text}"),
            ));
        }
        let parsed: TokenResponse = resp.json().await.map_err(|e| {
            KgError::new(
                KgErrorKind::Auth,
                format!("vertex token response decode error: {e}"),
            )
        })?;
        Ok(CachedToken {
            value: parsed.access_token,
            expires_at: SystemTime::now() + Duration::from_secs(parsed.expires_in.max(1)),
        })
    }
}

impl Default for VertexProvider {
    fn default() -> Self {
        Self::new()
    }
}

fn expand_home(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => path.to_string(),
        },
        None => path.to_string(),
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: u64,
}

#[derive(Deserialize)]
pub(crate) struct ServiceAccount {
    client_email: String,
    private_key: String,
    #[serde(default = "default_token_uri")]
    token_uri: String,
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_string()
}

/// base64url without padding, as JWT requires.
pub(crate) fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// Strip PEM armour and decode the DER body of a PKCS#8 private key.
pub(crate) fn pem_to_der(pem: &str) -> Result<Vec<u8>, KgError> {
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .concat();
    let body: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    if body.is_empty() {
        return Err(KgError::new(
            KgErrorKind::Auth,
            "vertex service-account private_key contains no PEM body",
        ));
    }
    base64::engine::general_purpose::STANDARD
        .decode(body.as_bytes())
        .map_err(|e| {
            KgError::new(
                KgErrorKind::Auth,
                format!("vertex private_key is not valid base64: {e}"),
            )
        })
}

/// The signed JWT claim set, as the string that gets signed.
pub(crate) fn assertion_payload(sa_email: &str, token_uri: &str, now: u64) -> String {
    let header = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
    let claims = serde_json::json!({
        "iss": sa_email,
        "scope": CLOUD_PLATFORM_SCOPE,
        "aud": token_uri,
        "iat": now,
        "exp": now + 3600,
    });
    format!(
        "{}.{}",
        b64url(header.to_string().as_bytes()),
        b64url(claims.to_string().as_bytes())
    )
}

/// Build the full `header.claims.signature` JWT assertion.
fn build_assertion(sa: &ServiceAccount, now: u64) -> Result<String, KgError> {
    let payload = assertion_payload(&sa.client_email, &sa.token_uri, now);
    let der = pem_to_der(&sa.private_key)?;
    let keypair = ring::signature::RsaKeyPair::from_pkcs8(&der).map_err(|e| {
        KgError::new(
            KgErrorKind::Auth,
            format!("vertex service-account key is not a usable PKCS#8 RSA key: {e}"),
        )
    })?;
    let mut sig = vec![0u8; keypair.public().modulus_len()];
    keypair
        .sign(
            &ring::signature::RSA_PKCS1_SHA256,
            &ring::rand::SystemRandom::new(),
            payload.as_bytes(),
            &mut sig,
        )
        .map_err(|_| {
            KgError::new(
                KgErrorKind::Auth,
                "vertex service-account JWT signing failed",
            )
        })?;
    Ok(format!("{payload}.{}", b64url(&sig)))
}

#[async_trait]
impl Provider for VertexProvider {
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
        let publisher = publisher_for(&model);
        let mode = credential_mode(&key.value);

        // Google rejects API-key auth on the Anthropic publisher. Say so precisely
        // instead of letting the caller see an opaque 403.
        if mode == CredentialMode::ApiKey && publisher == Publisher::Anthropic {
            return Err(KgError::unsupported(
                "vertex API-key auth for Anthropic publisher models — configure a \
                 service-account JSON, a path to one, or `adc` for metadata-server \
                 credentials",
            ));
        }

        let mut url = format!("{}{}", self.host(), self.chat_path(&model));
        let mut rb = self.client.post(&url);

        if mode == CredentialMode::ApiKey {
            url = format!("{url}?key={}", key.value.trim());
            rb = self.client.post(&url);
        } else {
            let token = self.access_token(key).await?;
            rb = rb.bearer_auth(token);
        }

        // Vertex serves the Gemini wire on the Google publisher path, so the body
        // mapping is shared with the native Gemini connector rather than forked.
        let mapper = GeminiProvider::new();
        let body = mapper.body(&req);

        let resp = rb
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

        let parsed: GeminiResponse = resp
            .json()
            .await
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))?;
        Ok(parsed.into_chat_response(model))
    }

    async fn chat_stream(
        &self,
        _ctx: &Ctx,
        _key: &ApiKey,
        _req: ChatRequest,
    ) -> Result<ChunkStream, KgError> {
        // Vertex streams via `:streamGenerateContent` with a JSON-array framing
        // that differs from SSE. Deferred, matching the native `gemini` connector's
        // stance rather than shipping a half-working parser.
        Err(KgError::unsupported("vertex streaming"))
    }
}

/// Strip a `key=<secret>` query parameter out of a string.
///
/// `reqwest::Error`'s `Display` appends `" for url (...)"` verbatim, and in
/// API-key mode that URL carries the Google API key. That text becomes
/// `KgError::message`, which the engine copies into `CallRecord::error_message`
/// and the logging observer **persists** — so without this the credential lands
/// in the log store and is served by `GET /api/logs/{id}`.
pub(crate) fn redact_key_param(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("key=") {
        // Only treat it as the query parameter, not a substring of another word.
        let is_param = i == 0 || matches!(rest.as_bytes()[i - 1], b'?' | b'&');
        out.push_str(&rest[..i + 4]);
        rest = &rest[i + 4..];
        if !is_param {
            continue;
        }
        let end = rest.find(['&', ')', ' ', '"', '\'']).unwrap_or(rest.len());
        out.push_str("<redacted>");
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn net_err(e: reqwest::Error) -> KgError {
    KgError::new(KgErrorKind::Network, redact_key_param(&e.to_string())).with_retryable(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgateway_core::schema::Message;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn api_key() -> ApiKey {
        ApiKey {
            id: "k".into(),
            value: "AIzaSyTestKey".into(),
            weight: 1,
            models: vec![],
        }
    }

    fn req(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: vec![Message::system("be terse"), Message::user("hi")],
            ..Default::default()
        }
    }

    #[test]
    fn vertex_host_handles_global_pools_and_plain_regions() {
        assert_eq!(vertex_host("global"), "https://aiplatform.googleapis.com");
        assert_eq!(
            vertex_host("us"),
            "https://aiplatform.us.rep.googleapis.com"
        );
        assert_eq!(
            vertex_host("eu"),
            "https://aiplatform.eu.rep.googleapis.com"
        );
        assert_eq!(
            vertex_host("us-central1"),
            "https://us-central1-aiplatform.googleapis.com"
        );
        assert_eq!(
            vertex_host("europe-west4"),
            "https://europe-west4-aiplatform.googleapis.com"
        );
    }

    #[test]
    fn parse_target_splits_project_and_location() {
        let (ep, project, location) = parse_target("my-proj/us-central1");
        assert!(ep.is_none());
        assert_eq!(project, "my-proj");
        assert_eq!(location, "us-central1");

        // Bare project falls back to the default location rather than erroring.
        let (_, project, location) = parse_target("solo-proj");
        assert_eq!(project, "solo-proj");
        assert_eq!(location, DEFAULT_LOCATION);

        // An http value is an endpoint override.
        let (ep, _, _) = parse_target("http://127.0.0.1:9/");
        assert_eq!(ep.as_deref(), Some("http://127.0.0.1:9"));
    }

    #[test]
    fn credential_mode_discriminates_on_value_shape() {
        assert_eq!(credential_mode(""), CredentialMode::Metadata);
        assert_eq!(credential_mode("  "), CredentialMode::Metadata);
        assert_eq!(credential_mode("adc"), CredentialMode::Metadata);
        assert_eq!(credential_mode("METADATA"), CredentialMode::Metadata);
        assert_eq!(
            credential_mode(r#"{"type":"service_account"}"#),
            CredentialMode::ServiceAccountJson
        );
        assert_eq!(
            credential_mode("/etc/gcp/sa.json"),
            CredentialMode::ServiceAccountFile
        );
        assert_eq!(
            credential_mode("~/keys/sa.json"),
            CredentialMode::ServiceAccountFile
        );
        assert_eq!(
            credential_mode("relative/sa.json"),
            CredentialMode::ServiceAccountFile
        );
        assert_eq!(credential_mode("AIzaSyAbc123"), CredentialMode::ApiKey);
    }

    #[test]
    fn publisher_routing_separates_claude_from_gemini() {
        assert_eq!(publisher_for("claude-sonnet-4-5"), Publisher::Anthropic);
        assert_eq!(publisher_for("gemini-2.5-pro"), Publisher::Google);
        assert_eq!(publisher_for("gemma-3-27b"), Publisher::Google);
    }

    #[test]
    fn chat_path_is_publisher_and_project_scoped() {
        let p = VertexProvider::with_base_url("my-proj/europe-west4");
        assert_eq!(
            p.chat_path("gemini-2.5-pro"),
            "/v1/projects/my-proj/locations/europe-west4/publishers/google/models/gemini-2.5-pro:generateContent"
        );
        assert_eq!(
            p.chat_path("claude-sonnet-4-5"),
            "/v1/projects/my-proj/locations/europe-west4/publishers/anthropic/models/claude-sonnet-4-5:rawPredict"
        );
    }

    #[test]
    fn b64url_is_unpadded_and_url_safe() {
        // 0xFB 0xFF encodes to "+/8" in standard base64 with padding.
        assert_eq!(b64url(&[0xFB, 0xFF]), "-_8");
        assert!(!b64url(b"any").contains('='));
    }

    #[test]
    fn pem_to_der_strips_armour_and_whitespace() {
        let pem = "-----BEGIN PRIVATE KEY-----\nAQID\nBAUG\n-----END PRIVATE KEY-----\n";
        assert_eq!(pem_to_der(pem).unwrap(), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn pem_to_der_rejects_empty_and_non_base64_bodies() {
        let empty = "-----BEGIN PRIVATE KEY-----\n-----END PRIVATE KEY-----";
        assert_eq!(pem_to_der(empty).unwrap_err().kind, KgErrorKind::Auth);
        let junk = "-----BEGIN PRIVATE KEY-----\n!!!not base64!!!\n-----END PRIVATE KEY-----";
        assert_eq!(pem_to_der(junk).unwrap_err().kind, KgErrorKind::Auth);
    }

    #[test]
    fn assertion_payload_carries_the_required_claims() {
        let p = assertion_payload("svc@proj.iam.gserviceaccount.com", "https://tok/", 1_000);
        let parts: Vec<&str> = p.split('.').collect();
        assert_eq!(parts.len(), 2, "payload is header.claims, unsigned");

        let decode = |s: &str| {
            let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(s)
                .expect("segment must be base64url");
            serde_json::from_slice::<serde_json::Value>(&raw).expect("segment must be JSON")
        };
        let header = decode(parts[0]);
        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["typ"], "JWT");

        let claims = decode(parts[1]);
        assert_eq!(claims["iss"], "svc@proj.iam.gserviceaccount.com");
        assert_eq!(claims["aud"], "https://tok/");
        assert_eq!(claims["scope"], CLOUD_PLATFORM_SCOPE);
        assert_eq!(claims["iat"], 1_000);
        // Google caps assertion lifetime at one hour.
        assert_eq!(claims["exp"], 4_600);
    }

    #[test]
    fn garbage_private_key_is_rejected_rather_than_signing() {
        let sa = ServiceAccount {
            client_email: "svc@proj.iam.gserviceaccount.com".into(),
            private_key: "-----BEGIN PRIVATE KEY-----\nAQIDBAUG\n-----END PRIVATE KEY-----".into(),
            token_uri: default_token_uri(),
        };
        // `ring` must refuse to build a keypair from six arbitrary bytes.
        let err = build_assertion(&sa, 0).expect_err("garbage DER must not sign");
        assert_eq!(err.kind, KgErrorKind::Auth);
    }

    #[test]
    fn cached_token_expiry_accounts_for_the_refresh_skew() {
        let stale = CachedToken {
            value: "t".into(),
            // Inside the skew window — must be treated as NOT fresh.
            expires_at: SystemTime::now() + Duration::from_secs(30),
        };
        assert!(!stale.is_fresh());

        let good = CachedToken {
            value: "t".into(),
            expires_at: SystemTime::now() + Duration::from_secs(3600),
        };
        assert!(good.is_fresh());
    }

    #[tokio::test]
    async fn api_key_mode_uses_query_param_on_the_google_publisher_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1/projects/test-project/locations/us-central1/publishers/google/models/gemini-2.5-pro:generateContent",
            ))
            .and(query_param("key", "AIzaSyTestKey"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [{
                    "content": { "parts": [{ "text": "hello" }], "role": "model" },
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": 3,
                    "candidatesTokenCount": 1,
                    "totalTokenCount": 4
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        // Endpoint override plus the documented `/project/location` suffix.
        let p = VertexProvider::with_base_url(format!("{}/test-project/us-central1", server.uri()));
        let out = p
            .chat(&Ctx::new(), &api_key(), req("gemini-2.5-pro"))
            .await
            .expect("api-key chat should succeed");

        assert_eq!(out.choices[0].message.text_content(), Some("hello"));
        assert_eq!(out.usage.total_tokens, 4);
    }

    #[tokio::test]
    async fn api_key_with_a_claude_model_is_refused_with_a_clear_reason() {
        let p = VertexProvider::with_base_url("proj/us-central1");
        let err = p
            .chat(&Ctx::new(), &api_key(), req("claude-sonnet-4-5"))
            .await
            .expect_err("API keys do not work on the Anthropic publisher");

        assert_eq!(err.kind, KgErrorKind::Unsupported);
        assert!(
            err.message.contains("service-account"),
            "the error must name the credential that would work, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn error_429_is_retryable_and_tagged() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string("quota exceeded"))
            .mount(&server)
            .await;

        let p = VertexProvider::with_base_url(format!("{}/proj/us-central1", server.uri()));
        let err = p
            .chat(&Ctx::new(), &api_key(), req("gemini-2.5-pro"))
            .await
            .expect_err("429 should map to an error");
        assert!(err.is_retryable());
        assert_eq!(err.status, Some(429));
        assert_eq!(err.provider.as_deref(), Some("vertex"));
    }

    /// Regression: the token cache must be keyed per credential. A single shared
    /// slot would hand key B the token minted for key A — authenticating as the
    /// wrong principal — and would defeat key rotation, since after a 401 the
    /// engine picks a sibling key and would get the same revoked token back.
    #[tokio::test]
    async fn token_cache_is_scoped_per_key() {
        let p = VertexProvider::with_base_url("proj/us-central1");
        let a = CachedToken {
            value: "token-A".into(),
            expires_at: SystemTime::now() + Duration::from_secs(3600),
        };
        let b = CachedToken {
            value: "token-B".into(),
            expires_at: SystemTime::now() + Duration::from_secs(3600),
        };
        {
            let mut w = p.tokens.write().await;
            w.insert("key-a".to_string(), a);
            w.insert("key-b".to_string(), b);
        }
        let r = p.tokens.read().await;
        assert_eq!(r.get("key-a").map(|t| t.value.as_str()), Some("token-A"));
        assert_eq!(r.get("key-b").map(|t| t.value.as_str()), Some("token-B"));
        // A third key must miss rather than inherit either token.
        assert!(r.get("key-c").is_none());
    }

    /// Regression: `reqwest::Error`'s Display appends " for url (...)", which in
    /// API-key mode carries the credential. That text becomes `KgError::message`
    /// and IS persisted to the audit log, so it must be redacted first.
    #[test]
    fn network_errors_never_carry_the_api_key() {
        let raw = "error sending request for url \
(https://x-aiplatform.googleapis.com/v1/projects/p/locations/l/publishers/google/\
models/m:generateContent?key=AIzaSyREALSECRET)";
        let out = redact_key_param(raw);
        assert!(
            !out.contains("AIzaSyREALSECRET"),
            "the API key must not survive redaction: {out}"
        );
        assert!(out.contains("key=<redacted>"), "got {out}");
        // The rest of the message must survive so the error stays diagnosable.
        assert!(out.contains("generateContent"));
    }

    #[test]
    fn redaction_only_touches_the_key_query_parameter() {
        // `monkey=` ends in "key=" but is not the parameter; leave it alone.
        let s = redact_key_param("https://h/p?monkey=banana&key=SECRET&x=1");
        assert!(s.contains("monkey=banana"), "got {s}");
        assert!(s.contains("key=<redacted>"), "got {s}");
        assert!(s.contains("x=1"), "trailing params must survive: {s}");
        assert!(!s.contains("SECRET"));
        // Nothing to redact -> unchanged.
        assert_eq!(redact_key_param("no secrets here"), "no secrets here");
    }

    /// Regression: an endpoint override must not invent a project id. A fabricated
    /// one builds a valid-looking path that always 404s with nothing to point at.
    #[test]
    fn endpoint_override_does_not_fabricate_a_project() {
        let (ep, project, location) = parse_target("http://127.0.0.1:9");
        assert_eq!(ep.as_deref(), Some("http://127.0.0.1:9"));
        assert!(
            project.is_empty(),
            "a bare endpoint has no project; got {project:?}"
        );
        assert_eq!(location, DEFAULT_LOCATION);

        // The documented `host/project/location` form must actually be honoured.
        let (ep, project, location) = parse_target("https://vertex.internal/my-proj/us-central1");
        assert_eq!(ep.as_deref(), Some("https://vertex.internal"));
        assert_eq!(project, "my-proj");
        assert_eq!(location, "us-central1");
    }

    #[tokio::test]
    async fn streaming_is_unsupported_not_silently_broken() {
        let p = VertexProvider::new();
        let err = p
            .chat_stream(&Ctx::new(), &api_key(), req("gemini-2.5-pro"))
            .await
            .err()
            .expect("streaming is not implemented in this cut");
        assert_eq!(err.kind, KgErrorKind::Unsupported);
    }

    /// Signing a real assertion needs an RSA private key, and `ring` cannot
    /// generate one. Committing a key would violate the repo's no-secrets rule, so
    /// the end-to-end signature is covered only when an operator points
    /// `KGATEWAY_TEST_GCP_SA` at a service-account file — the same env-gating the
    /// Postgres tests use.
    #[test]
    #[ignore = "requires KGATEWAY_TEST_GCP_SA pointing at a service-account JSON file"]
    fn service_account_assertion_signs_end_to_end() {
        let path = std::env::var("KGATEWAY_TEST_GCP_SA")
            .expect("set KGATEWAY_TEST_GCP_SA to run this test");
        let json = std::fs::read_to_string(path).expect("service-account file must be readable");
        let sa: ServiceAccount = serde_json::from_str(&json).expect("valid service-account JSON");
        let assertion = build_assertion(&sa, now_secs()).expect("signing must succeed");
        assert_eq!(
            assertion.split('.').count(),
            3,
            "a signed JWT has three segments"
        );
    }
}
