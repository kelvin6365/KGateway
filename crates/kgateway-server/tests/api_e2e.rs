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
