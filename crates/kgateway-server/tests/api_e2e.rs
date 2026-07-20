//! End-to-end integration tests over the REAL router (`app::build_router` + `build_state`),
//! now that the server is a library crate. Covers the M10–M12 control-plane surface: logs,
//! RBAC tiers, redaction + reveal, and analytics — against a mocked upstream.

use std::collections::HashMap;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use kgateway_server::app;
use kgateway_server::config::{
    ApiTokenConfig, Config, ContentLoggingConfig, KeyConfig, ProviderConfig, RedactionConfig, Role,
};

/// A mock upstream whose assistant reply contains an email (so redaction has something to do).
async fn mock_upstream() -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "reach me at agent@example.com" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7 }
        })))
        .mount(&upstream)
        .await;
    upstream
}

/// Build the real app state + router against the mock upstream, with content capture,
/// redaction, and RBAC (viewer + admin tokens) all enabled.
async fn build(upstream_uri: String) -> axum::Router {
    let mut providers = HashMap::new();
    providers.insert(
        "openai".to_string(),
        ProviderConfig {
            kind: Some("openai".into()),
            base_url: Some(upstream_uri),
            keys: vec![KeyConfig {
                id: "default".into(),
                value: "test".into(),
                weight: 1,
                models: vec![],
            }],
        },
    );

    let mut config = Config {
        providers,
        content_logging: Some(ContentLoggingConfig {
            enabled: true,
            max_body_bytes: 16 * 1024,
            capture_streaming: false,
        }),
        redaction: Some(RedactionConfig {
            enabled: true,
            key: Some("test-redaction-key".into()),
            patterns: vec![],
        }),
        api_tokens: vec![
            ApiTokenConfig {
                token: "viewer-tok".into(),
                role: Role::Viewer,
                name: "v".into(),
            },
            ApiTokenConfig {
                token: "admin-tok".into(),
                role: Role::Admin,
                name: "a".into(),
            },
        ],
        ..Config::default()
    };
    config.port = 0;

    let state = app::build_state(config, "test-config.json".into()).await;
    app::build_router(state)
}

async fn send(app: &axum::Router, req: Request<Body>) -> (u16, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

fn chat(content: &str) -> Request<Body> {
    let body = serde_json::json!({
        "model": "openai/gpt-4o",
        "messages": [{ "role": "user", "content": content }],
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Fire a chat, then poll `/api/logs` until the async writer has persisted it; return its id.
async fn chat_and_wait_for_log(app: &axum::Router, content: &str) -> String {
    let (status, _) = send(app, chat(content)).await;
    assert_eq!(status, 200, "chat should succeed through the real router");
    for _ in 0..50 {
        let (_, page) = send(app, get("/api/logs", Some("admin-tok"))).await;
        if page["total"].as_u64().unwrap_or(0) > 0 {
            return page["logs"][0]["request_id"].as_str().unwrap().to_string();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("log never appeared");
}

#[tokio::test]
async fn chat_then_log_captured_and_redacted() {
    let upstream = mock_upstream().await;
    let app = build(upstream.uri()).await;

    let id = chat_and_wait_for_log(&app, "my email is user@example.com").await;

    // Detail shows redacted bodies (email masked, redacted flag set), no mapping to client.
    let (status, detail) = send(&app, get(&format!("/api/logs/{id}"), Some("admin-tok"))).await;
    assert_eq!(status, 200);
    assert_eq!(detail["redacted"], true);
    let req_body = detail["request_body"].as_str().unwrap();
    assert!(
        !req_body.contains("user@example.com"),
        "request email must be redacted"
    );
    assert!(req_body.contains("⟦REDACTED"));
    assert!(
        detail.get("redaction_mapping").is_none(),
        "mapping must not reach clients"
    );
    // The response body (agent@example.com from the mock) is redacted too.
    assert!(!detail["response_body"]
        .as_str()
        .unwrap()
        .contains("agent@example.com"));
}

#[tokio::test]
async fn reveal_restores_only_for_admin() {
    let upstream = mock_upstream().await;
    let app = build(upstream.uri()).await;
    let id = chat_and_wait_for_log(&app, "my email is user@example.com").await;

    // viewer may read logs but NOT reveal.
    let (list_status, _) = send(&app, get("/api/logs", Some("viewer-tok"))).await;
    assert_eq!(list_status, 200);
    let (reveal_forbidden, _) = send(
        &app,
        get(&format!("/api/logs/{id}/reveal"), Some("viewer-tok")),
    )
    .await;
    assert_eq!(reveal_forbidden, 403, "viewer lacks logs:reveal");

    // admin reveal restores the originals.
    let (reveal_ok, revealed) = send(
        &app,
        get(&format!("/api/logs/{id}/reveal"), Some("admin-tok")),
    )
    .await;
    assert_eq!(reveal_ok, 200);
    assert!(revealed["request_body"]
        .as_str()
        .unwrap()
        .contains("user@example.com"));
    assert!(revealed["response_body"]
        .as_str()
        .unwrap()
        .contains("agent@example.com"));
    assert_eq!(revealed["request_revealed"], true);
}

#[tokio::test]
async fn rbac_rejects_missing_and_unknown_tokens() {
    let upstream = mock_upstream().await;
    let app = build(upstream.uri()).await;

    let (no_token, _) = send(&app, get("/api/logs", None)).await;
    assert_eq!(no_token, 401);
    let (bad_token, _) = send(&app, get("/api/logs", Some("nope"))).await;
    assert_eq!(bad_token, 401);
    // A viewer hitting a config-write route is forbidden (config:write required).
    let put = Request::builder()
        .method("PUT")
        .uri("/api/config/providers/foo")
        .header("authorization", "Bearer viewer-tok")
        .header("content-type", "application/json")
        .body(Body::from("{\"keys\":[]}"))
        .unwrap();
    let (write_forbidden, _) = send(&app, put).await;
    assert_eq!(write_forbidden, 403, "viewer lacks config:write");
}

#[tokio::test]
async fn analytics_endpoints_respond() {
    let upstream = mock_upstream().await;
    let app = build(upstream.uri()).await;
    chat_and_wait_for_log(&app, "hello").await;

    for uri in [
        "/api/logs/stats",
        "/api/logs/histogram?metric=latency",
        "/api/logs/timeseries",
        "/api/logs/rankings?by=model",
        "/api/logs/filterdata",
        "/api/logs/dropped",
        "/api/status",
        "/api/whoami",
    ] {
        let (status, _) = send(&app, get(uri, Some("admin-tok"))).await;
        assert_eq!(status, 200, "{uri} should be 200 for admin");
        // …and unauthenticated.
        let (unauth, _) = send(&app, get(uri, None)).await;
        assert_eq!(unauth, 401, "{uri} should require auth");
    }
}

#[tokio::test]
async fn v1_models_aggregates_across_providers_and_skips_failures() {
    // OpenAI-compatible upstream: GET {base}/models.
    let openai_upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [
                { "id": "glm-5.2", "object": "model", "created": 1781625600, "owned_by": "z-ai" },
                { "id": "glm-4.7", "object": "model", "created": 1766332800, "owned_by": "z-ai" }
            ]
        })))
        .mount(&openai_upstream)
        .await;

    // Anthropic-compatible upstream: GET {base}/v1/models.
    let anthropic_upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                { "type": "model", "id": "glm-5.2", "display_name": "GLM-5.2",
                  "created_at": "2026-06-17T00:00:00Z" }
            ],
            "hasMore": false
        })))
        .mount(&anthropic_upstream)
        .await;

    // A dead upstream (no mock mounted → 404) must be skipped, not fail the listing.
    let dead_upstream = MockServer::start().await;

    let mut providers = HashMap::new();
    providers.insert(
        "zai-coding".to_string(),
        ProviderConfig {
            kind: Some("openai".into()),
            base_url: Some(openai_upstream.uri()),
            keys: vec![KeyConfig {
                id: "default".into(),
                value: "test".into(),
                weight: 1,
                models: vec![],
            }],
        },
    );
    providers.insert(
        "zai".to_string(),
        ProviderConfig {
            kind: Some("anthropic".into()),
            base_url: Some(anthropic_upstream.uri()),
            keys: vec![KeyConfig {
                id: "coding-plan".into(),
                value: "test".into(),
                weight: 1,
                models: vec![],
            }],
        },
    );
    providers.insert(
        "dead".to_string(),
        ProviderConfig {
            kind: Some("openai".into()),
            base_url: Some(dead_upstream.uri()),
            keys: vec![KeyConfig {
                id: "default".into(),
                value: "test".into(),
                weight: 1,
                models: vec![],
            }],
        },
    );
    // A provider whose key resolves empty (unset ${ENV}) is skipped without a fetch.
    providers.insert(
        "keyless".to_string(),
        ProviderConfig {
            kind: Some("openai".into()),
            base_url: Some(dead_upstream.uri()),
            keys: vec![KeyConfig {
                id: "default".into(),
                value: String::new(),
                weight: 1,
                models: vec![],
            }],
        },
    );

    let mut config = Config {
        providers,
        ..Config::default()
    };
    config.port = 0;
    let state = app::build_state(config, "test-config.json".into()).await;
    let app = app::build_router(state);

    let (status, body) = send(&app, get("/v1/models", None)).await;
    assert_eq!(status, 200);
    assert_eq!(body["object"], "list");
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    // Sorted, provider-prefixed, dead + keyless providers skipped.
    assert_eq!(
        ids,
        vec!["zai-coding/glm-4.7", "zai-coding/glm-5.2", "zai/glm-5.2"]
    );
    let first = &body["data"][0];
    assert_eq!(first["object"], "model");
    assert_eq!(first["owned_by"], "zai-coding");
    assert_eq!(first["created"], 1766332800);
}

#[tokio::test]
async fn v1_models_is_cached_and_vkey_gated_in_strict_mode() {
    // Upstream expects EXACTLY ONE list fetch — the second /v1/models must be a cache hit.
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [{ "id": "kimi-k3", "object": "model", "created": 1, "owned_by": "moonshot" }]
        })))
        .expect(1)
        .mount(&upstream)
        .await;

    let mut providers = HashMap::new();
    providers.insert(
        "moonshot".to_string(),
        ProviderConfig {
            kind: Some("openai".into()),
            base_url: Some(upstream.uri()),
            keys: vec![KeyConfig {
                id: "default".into(),
                value: "test".into(),
                weight: 1,
                models: vec![],
            }],
        },
    );
    let mut config = Config {
        providers,
        virtual_keys: vec![kgateway_server::config::VirtualKeyConfig {
            id: "vk_test".into(),
            ..Default::default()
        }],
        ..Config::default()
    };
    config.port = 0;
    let state = app::build_state(config, "test-config.json".into()).await;
    let app = app::build_router(state);

    // Strict mode: anonymous and wrong-key listings are rejected before any fetch.
    let (unauth, _) = send(&app, get("/v1/models", None)).await;
    assert_eq!(unauth, 401);
    let (wrong, _) = send(&app, get("/v1/models", Some("nope"))).await;
    assert_eq!(wrong, 401);

    // Two authorized calls: same body, only one upstream fetch (expect(1) verifies on drop).
    let (s1, b1) = send(&app, get("/v1/models", Some("vk_test"))).await;
    let (s2, b2) = send(&app, get("/v1/models", Some("vk_test"))).await;
    assert_eq!((s1, s2), (200, 200));
    assert_eq!(b1, b2);
    assert_eq!(b1["data"][0]["id"], "moonshot/kimi-k3");
}

#[tokio::test]
async fn trace_spans_are_detail_only_and_arrive_as_a_json_array() {
    // The waterfall UI consumes `spans` as an array; if it ever regressed to a
    // JSON-encoded string the dashboard would silently render nothing. And spans must
    // stay off the list path so a 200-row page doesn't drag every trace with it.
    let upstream = mock_upstream().await;
    let app = build(upstream.uri()).await;
    let id = chat_and_wait_for_log(&app, "trace me").await;

    let (_, list) = send(&app, get("/api/logs", Some("admin-tok"))).await;
    assert!(
        list["logs"][0].get("spans").is_none(),
        "list rows must not carry traces"
    );

    let (status, detail) = send(&app, get(&format!("/api/logs/{id}"), Some("admin-tok"))).await;
    assert_eq!(status, 200);
    let spans = detail["spans"]
        .as_array()
        .expect("detail returns spans as a real array, not a string");
    assert!(!spans.is_empty(), "a dispatched request records stages");

    // The dispatch attempt is the span the whole feature exists to show.
    let attempt = spans
        .iter()
        .find(|s| {
            s["name"]
                .as_str()
                .is_some_and(|n| n.starts_with("attempt ·"))
        })
        .expect("the upstream attempt is traced");
    assert_eq!(attempt["category"], "network");
    assert!(attempt["dur_us"].as_u64().is_some());
    assert!(attempt["start_us"].as_u64().is_some());
    assert_eq!(attempt["depth"], 1);
}
