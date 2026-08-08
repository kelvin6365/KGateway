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

// ---- Per-virtual-key read scoping ----

/// Router with RBAC tokens (viewer / operator / admin) AND two virtual keys, so the data
/// plane is in strict mode and the read plane has both credential classes to exercise.
/// `config_path` must be unique per test — config writes persist there.
async fn build_scoped(upstream_uri: String, config_path: std::path::PathBuf) -> axum::Router {
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
        // Capture bodies so the tests can assert a vkey reads its OWN rows in full.
        content_logging: Some(ContentLoggingConfig {
            enabled: true,
            max_body_bytes: 16 * 1024,
            capture_streaming: false,
        }),
        api_tokens: vec![
            ApiTokenConfig {
                token: "viewer-tok".into(),
                role: Role::Viewer,
                name: "v".into(),
            },
            ApiTokenConfig {
                token: "operator-tok".into(),
                role: Role::Operator,
                name: "o".into(),
            },
            ApiTokenConfig {
                token: "admin-tok".into(),
                role: Role::Admin,
                name: "a".into(),
            },
        ],
        virtual_keys: vec![
            kgateway_server::config::VirtualKeyConfig {
                id: "vk_alpha_123".into(),
                name: "Team Alpha".into(),
                ..Default::default()
            },
            kgateway_server::config::VirtualKeyConfig {
                id: "vk_beta_1234".into(),
                ..Default::default()
            },
        ],
        ..Config::default()
    };
    config.port = 0;
    let state = app::build_state(config, config_path.to_string_lossy().into_owned()).await;
    app::build_router(state)
}

fn scoped_config_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("kgateway-e2e-{tag}-{}.json", std::process::id()))
}

/// A chat request authenticated as a virtual key, optionally tagged with a session id.
fn chat_as(vkey: &str, session: Option<&str>, content: &str) -> Request<Body> {
    let body = serde_json::json!({
        "model": "openai/gpt-4o",
        "messages": [{ "role": "user", "content": content }],
    });
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {vkey}"));
    if let Some(s) = session {
        b = b.header("x-session-id", s);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

/// Poll `/api/logs` (as admin) until the async writer has persisted `want` rows.
async fn wait_for_total(app: &axum::Router, want: u64) {
    for _ in 0..100 {
        let (_, page) = send(app, get("/api/logs", Some("admin-tok"))).await;
        if page["total"].as_u64().unwrap_or(0) >= want {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("logs never reached {want}");
}

#[tokio::test]
async fn virtual_key_reads_are_scoped_to_own_traffic() {
    let upstream = mock_upstream().await;
    let app = build_scoped(upstream.uri(), scoped_config_path("scoped")).await;

    // Seed: two alpha calls (one sharing a session with beta), one beta call.
    let (s, _) = send(
        &app,
        chat_as("vk_alpha_123", Some("sess-shared"), "alpha 1"),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _) = send(&app, chat_as("vk_alpha_123", Some("sess-alpha"), "alpha 2")).await;
    assert_eq!(s, 200);
    let (s, _) = send(&app, chat_as("vk_beta_1234", Some("sess-shared"), "beta 1")).await;
    assert_eq!(s, 200);
    wait_for_total(&app, 3).await;

    // Every control token — even a bare viewer — sees all keys' rows.
    let (s, page) = send(&app, get("/api/logs", Some("viewer-tok"))).await;
    assert_eq!((s, page["total"].as_u64()), (200, Some(3)));

    // A vkey sees only its own rows, and its own `virtual_key` param cannot widen that.
    for uri in ["/api/logs", "/api/logs?virtual_key=vk_beta_1234"] {
        let (s, page) = send(&app, get(uri, Some("vk_alpha_123"))).await;
        assert_eq!((s, page["total"].as_u64()), (200, Some(2)), "{uri}");
        for row in page["logs"].as_array().unwrap() {
            assert_eq!(row["virtual_key"], "vk_alpha_123", "{uri}");
        }
    }

    // Aggregates count only the caller's rows.
    let (_, stats) = send(&app, get("/api/logs/stats", Some("vk_alpha_123"))).await;
    assert_eq!(stats["total"].as_u64(), Some(2));
    let (_, ts) = send(&app, get("/api/logs/timeseries", Some("vk_alpha_123"))).await;
    let count: u64 = ts["points"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["count"].as_u64().unwrap())
        .sum();
    assert_eq!(count, 2);
    let (_, hist) = send(
        &app,
        get("/api/logs/histogram?metric=latency", Some("vk_alpha_123")),
    )
    .await;
    let hist_total: u64 = hist["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["count"].as_u64().unwrap())
        .sum();
    assert_eq!(hist_total, 2);
    let (_, ranks) = send(
        &app,
        get("/api/logs/rankings?by=virtual_key", Some("vk_alpha_123")),
    )
    .await;
    let keys: Vec<&str> = ranks["rankings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        keys,
        vec!["vk_alpha_123"],
        "no other key leaks into rankings"
    );

    // Filterdata: only own values; the global key roster is not enumerable.
    let (_, fd) = send(&app, get("/api/logs/filterdata", Some("vk_alpha_123"))).await;
    assert_eq!(fd["virtual_keys"], serde_json::json!(["vk_alpha_123"]));

    // whoami reports the scoped identity.
    let (_, who) = send(&app, get("/api/whoami", Some("vk_alpha_123"))).await;
    assert_eq!(who["kind"], "virtual_key");
    assert_eq!(who["scoped_to"], "vk_alpha_123");
    assert_eq!(who["name"], "Team Alpha");
    assert_eq!(who["permissions"], serde_json::json!(["logs:view"]));
    let (_, who) = send(&app, get("/api/whoami", Some("admin-tok"))).await;
    assert_eq!(
        (who["kind"].as_str(), who["role"].as_str()),
        (Some("token"), Some("admin"))
    );
}

#[tokio::test]
async fn virtual_key_detail_reads_own_rows_only_and_sessions_are_scoped() {
    let upstream = mock_upstream().await;
    let app = build_scoped(upstream.uri(), scoped_config_path("detail")).await;

    let (s, _) = send(&app, chat_as("vk_alpha_123", Some("sess-shared"), "alpha")).await;
    assert_eq!(s, 200);
    let (s, _) = send(&app, chat_as("vk_beta_1234", Some("sess-shared"), "beta")).await;
    assert_eq!(s, 200);
    let (s, _) = send(&app, chat_as("vk_beta_1234", Some("sess-beta"), "beta 2")).await;
    assert_eq!(s, 200);
    wait_for_total(&app, 3).await;

    // Resolve each key's row ids via the admin view.
    let (_, all) = send(&app, get("/api/logs", Some("admin-tok"))).await;
    let row_of = |vk: &str| {
        all["logs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["virtual_key"] == vk)
            .unwrap()["request_id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let alpha_id = row_of("vk_alpha_123");
    let beta_id = row_of("vk_beta_1234");

    // Own row: 200, with captured body (the caller authored that content).
    let (s, own) = send(
        &app,
        get(&format!("/api/logs/{alpha_id}"), Some("vk_alpha_123")),
    )
    .await;
    assert_eq!(s, 200);
    assert!(own["request_body"].is_string());
    // Foreign row: the same 404 as a missing id — no existence oracle.
    let (s, foreign) = send(
        &app,
        get(&format!("/api/logs/{beta_id}"), Some("vk_alpha_123")),
    )
    .await;
    assert_eq!(s, 404);
    assert_eq!(foreign["error"]["message"], "log not found");
    let (s, missing) = send(&app, get("/api/logs/does-not-exist", Some("vk_alpha_123"))).await;
    assert_eq!(s, 404);
    assert_eq!(missing["error"]["message"], foreign["error"]["message"]);

    // Session list: only sessions containing the caller's calls.
    let (_, sessions) = send(&app, get("/api/sessions", Some("vk_alpha_123"))).await;
    let ids: Vec<&str> = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["sess-shared"], "sess-beta must not appear");

    // A mixed session shows only the caller's own calls.
    let (s, journey) = send(&app, get("/api/sessions/sess-shared", Some("vk_alpha_123"))).await;
    assert_eq!(s, 200);
    let calls = journey["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["virtual_key"], "vk_alpha_123");
    // A session with none of the caller's calls is a 404.
    let (s, _) = send(&app, get("/api/sessions/sess-beta", Some("vk_alpha_123"))).await;
    assert_eq!(s, 404);
    // The admin sees the whole mixed session.
    let (_, full) = send(&app, get("/api/sessions/sess-shared", Some("admin-tok"))).await;
    assert_eq!(full["calls"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn virtual_key_is_rejected_on_token_only_and_privileged_endpoints() {
    let upstream = mock_upstream().await;
    let app = build_scoped(upstream.uri(), scoped_config_path("denied")).await;

    for uri in [
        "/api/logs/dropped",
        "/api/mcp/tools",
        "/api/providers",
        "/api/config/providers",
        "/api/config/virtual-keys",
        "/metrics",
        "/api/status",
        "/api/logs/nonexistent/reveal",
    ] {
        let (s, _) = send(&app, get(uri, Some("vk_alpha_123"))).await;
        assert_eq!(s, 401, "{uri} must reject a virtual key");
    }
    let put = Request::builder()
        .method("PUT")
        .uri("/api/config/virtual-keys/vk_new")
        .header("authorization", "Bearer vk_alpha_123")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (s, _) = send(&app, put).await;
    assert_eq!(s, 401, "a virtual key has no config:write");
}

#[tokio::test]
async fn config_vkeys_masked_for_viewer_full_for_operator() {
    let upstream = mock_upstream().await;
    let app = build_scoped(upstream.uri(), scoped_config_path("masking")).await;

    // Operator (config:write) gets the real ids — it could rewrite them anyway.
    let (s, full) = send(&app, get("/api/config/virtual-keys", Some("operator-tok"))).await;
    assert_eq!(s, 200);
    assert_eq!(full["virtual_keys"][0]["id"], "vk_alpha_123");
    assert!(full["virtual_keys"][0].get("id_masked").is_none());

    // Viewer gets masked ids (the id IS the bearer secret) with limits/names intact.
    let (s, masked) = send(&app, get("/api/config/virtual-keys", Some("viewer-tok"))).await;
    assert_eq!(s, 200);
    assert_eq!(masked["virtual_keys"][0]["id"], "vk_a…");
    assert_eq!(masked["virtual_keys"][0]["id_masked"], true);
    assert_eq!(masked["virtual_keys"][0]["name"], "Team Alpha");
}

#[tokio::test]
async fn vkey_read_access_revokes_immediately_on_delete() {
    let upstream = mock_upstream().await;
    let config_path = scoped_config_path("revoke");
    let app = build_scoped(upstream.uri(), config_path.clone()).await;

    let (s, _) = send(&app, get("/api/logs", Some("vk_alpha_123"))).await;
    assert_eq!(s, 200);

    // Admin deletes the key; the very next read with it must fail (live config set).
    let del = Request::builder()
        .method("DELETE")
        .uri("/api/config/virtual-keys/vk_alpha_123")
        .header("authorization", "Bearer admin-tok")
        .body(Body::empty())
        .unwrap();
    let (s, _) = send(&app, del).await;
    assert_eq!(s, 200);
    let (s, _) = send(&app, get("/api/logs", Some("vk_alpha_123"))).await;
    assert_eq!(s, 401, "a deleted key must stop authenticating immediately");

    let _ = std::fs::remove_file(config_path);
}

#[tokio::test]
async fn vkeys_without_tokens_locks_reveal_but_not_writes() {
    // Strict-read shape with no control tokens at all: reads need a key (scoped), reveal
    // is the most sensitive read and must be locked for EVERYONE (no admin can exist),
    // while config writes deliberately stay open (documented + warned) so the dashboard
    // that created the first key isn't locked out.
    let upstream = mock_upstream().await;
    let mut providers = HashMap::new();
    providers.insert(
        "openai".to_string(),
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
            id: "vk_only_12345".into(),
            ..Default::default()
        }],
        ..Config::default()
    };
    config.port = 0;
    let state = app::build_state(config, "test-config.json".into()).await;
    let app = app::build_router(state);

    // Reads: anonymous 401, the key gets its (scoped, empty) view.
    let (s, _) = send(&app, get("/api/logs", None)).await;
    assert_eq!(s, 401);
    let (s, _) = send(&app, get("/api/logs", Some("vk_only_12345"))).await;
    assert_eq!(s, 200);

    // Reveal: locked for anonymous AND for the key.
    let (s, _) = send(&app, get("/api/logs/some-id/reveal", None)).await;
    assert_eq!(s, 401, "anonymous reveal must lock in vkeys-only mode");
    let (s, _) = send(&app, get("/api/logs/some-id/reveal", Some("vk_only_12345"))).await;
    assert_eq!(s, 401, "a virtual key must never reveal");
}

#[tokio::test]
async fn open_mode_reads_stay_unauthenticated() {
    // Regression guard for requirement 1: nothing declared → everything stays open.
    let upstream = mock_upstream().await;
    let mut providers = HashMap::new();
    providers.insert(
        "openai".to_string(),
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
        ..Config::default()
    };
    config.port = 0;
    let state = app::build_state(config, "test-config.json".into()).await;
    let app = app::build_router(state);

    for uri in [
        "/api/logs",
        "/api/logs/stats",
        "/api/logs/filterdata",
        "/api/sessions",
        "/api/status",
        "/api/providers",
        "/api/config/virtual-keys",
        "/metrics",
    ] {
        let (s, _) = send(&app, get(uri, None)).await;
        assert_eq!(s, 200, "{uri} must stay open when nothing is declared");
    }
    let (_, who) = send(&app, get("/api/whoami", None)).await;
    assert_eq!(who["kind"], "open");
    assert_eq!(who["role"], "admin");
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
