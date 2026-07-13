//! End-to-end test: axum handler → engine → OpenAI provider → mocked upstream.
//! Proves the M1 vertical slice without a live API key.

use http_body_util::BodyExt;
use kgateway_core::engine::Kgateway;
use kgateway_core::provider::ApiKey;
use kgateway_core::router::Registry;
use kgateway_providers::OpenAiProvider;
use std::sync::Arc;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// The binary's handlers.rs is the real HTTP surface; a binary's private modules aren't
// importable from an integration test, so we reconstruct the equivalent tiny router here
// against a mock upstream. This still exercises the full engine → provider → HTTP path.
async fn build_app(base_url: String) -> axum::Router {
    let mut registry = Registry::new();
    registry.register(
        Arc::new(OpenAiProvider::with_base_url(base_url)),
        vec![ApiKey {
            id: "k".into(),
            value: "test".into(),
            weight: 1,
            models: vec![],
        }],
    );
    let engine = Kgateway::new(registry);
    let state = Arc::new(AppState { engine });
    axum::Router::new()
        .route("/v1/chat/completions", axum::routing::post(chat))
        .with_state(state)
}

// Inline copies of the handler bits under test (kept tiny; the binary's handlers.rs is
// the real one — this avoids exporting the binary's private modules).
struct AppState {
    engine: Kgateway,
}
async fn chat(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::Json(req): axum::Json<kgateway_core::schema::ChatRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut ctx = kgateway_core::context::Ctx::new();
    match state.engine.chat(&mut ctx, req).await {
        Ok(resp) => axum::Json(resp).into_response(),
        Err(e) => (axum::http::StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

#[tokio::test]
async fn chat_completion_round_trips_through_gateway() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hello from mock" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7 }
        })))
        .mount(&upstream)
        .await;

    let app = build_app(upstream.uri()).await;

    let body = serde_json::json!({
        "model": "openai/gpt-4o",
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), 200);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["choices"][0]["message"]["content"], "hello from mock");
    assert_eq!(json["usage"]["total_tokens"], 7);
}

#[tokio::test]
async fn upstream_error_maps_to_bad_gateway() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&upstream)
        .await;

    let app = build_app(upstream.uri()).await;
    let body = serde_json::json!({ "model": "openai/gpt-4o", "messages": [] });
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::BAD_GATEWAY);
}
