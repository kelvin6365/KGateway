//! Hot-path micro-benchmarks for kgateway-core.
//!
//! Three groups measure the per-request overhead KGateway adds relative to raw
//! provider calls:
//!
//! * `keyselect`   — weighted key selection (`eligible_keys` + `weighted_pick`),
//!   targeting ~10ns here.
//! * `schema_serde` — JSON round-trip of the internal request/response schema.
//! * `engine_chat` — end-to-end `Kgateway::chat` against an instant mock provider,
//!   isolating the gateway's own pipeline/routing/isolation overhead (targeting
//!   µs-level overhead).

use async_trait::async_trait;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rand::rngs::StdRng;
use rand::SeedableRng;
use std::sync::Arc;

use kgateway_core::context::Ctx;
use kgateway_core::engine::Kgateway;
use kgateway_core::error::KgError;
use kgateway_core::keyselect::{eligible_keys, weighted_pick};
use kgateway_core::provider::{ApiKey, ChunkStream, Provider, ProviderKey};
use kgateway_core::router::Registry;
use kgateway_core::schema::{ChatRequest, ChatResponse, Choice, Message, Role, Usage};

/// ~8 keys with mixed weights and model allow-lists, representative of a real
/// provider fan-out.
fn sample_keys() -> Vec<ApiKey> {
    let mk = |id: &str, weight: u32, models: &[&str]| ApiKey {
        id: id.into(),
        value: "secret".into(),
        weight,
        models: models.iter().map(|s| s.to_string()).collect(),
    };
    vec![
        mk("k0", 1, &[]),
        mk("k1", 5, &["gpt-4o"]),
        mk("k2", 2, &["gpt-4o", "gpt-4o-mini"]),
        mk("k3", 3, &["claude-3-5-sonnet"]),
        mk("k4", 0, &[]),
        mk("k5", 4, &["gpt-4o", "o1"]),
        mk("k6", 1, &["gpt-4o-mini"]),
        mk("k7", 7, &[]),
    ]
}

fn bench_keyselect(c: &mut Criterion) {
    let keys = sample_keys();
    let mut rng = StdRng::seed_from_u64(0xC0FFEE);
    c.bench_function("keyselect/eligible+weighted_pick", |b| {
        b.iter(|| {
            let eligible = eligible_keys(black_box(&keys), black_box("gpt-4o"));
            let picked = weighted_pick(black_box(&eligible), &mut rng);
            black_box(picked);
        });
    });
}

fn sample_response() -> ChatResponse {
    ChatResponse {
        id: "chatcmpl-bench-0001".into(),
        object: "chat.completion".into(),
        model: "gpt-4o".into(),
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: Some(
                    "The quick brown fox jumps over the lazy dog. This is a \
                     representative assistant reply of moderate length."
                        .into(),
                ),
                name: None,
                tool_calls: vec![],
                tool_call_id: None,
            },
            finish_reason: Some("stop".into()),
        }],
        usage: Usage {
            prompt_tokens: 24,
            completion_tokens: 18,
            total_tokens: 42,
        },
    }
}

const SAMPLE_REQUEST_JSON: &str = r#"{
    "model": "gpt-4o",
    "messages": [
        {"role": "system", "content": "You are a helpful assistant."},
        {"role": "user", "content": "Explain the theory of relativity in one paragraph."}
    ],
    "temperature": 0.7,
    "max_tokens": 512,
    "top_p": 0.95
}"#;

fn bench_schema_serde(c: &mut Criterion) {
    let mut group = c.benchmark_group("schema_serde");

    let resp = sample_response();
    group.bench_function("serialize_response", |b| {
        b.iter(|| {
            let s = serde_json::to_string(black_box(&resp)).unwrap();
            black_box(s);
        });
    });

    group.bench_function("deserialize_request", |b| {
        b.iter(|| {
            let req: ChatRequest = serde_json::from_str(black_box(SAMPLE_REQUEST_JSON)).unwrap();
            black_box(req);
        });
    });

    group.finish();
}

/// A provider that returns a fixed response immediately (no network). Isolates the
/// gateway's own per-request overhead. `chat_stream` is unused by this bench.
struct BenchProvider;

#[async_trait]
impl Provider for BenchProvider {
    fn key(&self) -> ProviderKey {
        ProviderKey::new("openai")
    }

    async fn chat(
        &self,
        _ctx: &Ctx,
        _key: &ApiKey,
        _req: ChatRequest,
    ) -> Result<ChatResponse, KgError> {
        Ok(sample_response())
    }

    async fn chat_stream(
        &self,
        _ctx: &Ctx,
        _key: &ApiKey,
        _req: ChatRequest,
    ) -> Result<ChunkStream, KgError> {
        Err(KgError::internal("chat_stream unused in bench"))
    }
}

fn bench_engine_chat(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut registry = Registry::new();
    registry.register(
        Arc::new(BenchProvider),
        vec![ApiKey {
            id: "k".into(),
            value: "v".into(),
            weight: 1,
            models: vec![],
        }],
    );
    let engine = Kgateway::new(registry);

    let req = ChatRequest {
        model: "openai/gpt-4o".into(),
        messages: vec![Message::user("hello")],
        ..Default::default()
    };

    c.bench_function("engine_chat/end_to_end", |b| {
        b.iter(|| {
            let mut ctx = Ctx::new();
            let resp = rt
                .block_on(engine.chat(&mut ctx, black_box(req.clone())))
                .unwrap();
            black_box(resp);
        });
    });
}

criterion_group!(
    benches,
    bench_keyselect,
    bench_schema_serde,
    bench_engine_chat
);
criterion_main!(benches);
