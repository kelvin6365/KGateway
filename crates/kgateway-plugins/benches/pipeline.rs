//! Pipeline overhead micro-benchmarks (M13). Measures the per-request cost each
//! observability/security layer adds on top of the bare engine hot path, against an
//! instant mock provider (no network) — so the numbers isolate KGateway's own overhead.
//!
//! Layers measured incrementally:
//! * `bare`             — engine only (reproduces the core `engine_chat` number).
//! * `logging`          — + audit logging observer (in-memory store, inline append).
//! * `governance`       — + virtual-key governance observer.
//! * `full`             — + logging + governance (typical production observability).
//! * `capture`          — full + request/response content capture (JSON serialize bodies).
//! * `capture+redaction`— full + capture + redaction (regex scan + AES-GCM mapping).

use std::sync::Arc;

use async_trait::async_trait;
use criterion::{black_box, criterion_group, criterion_main, Criterion};

use kgateway_core::context::Ctx;
use kgateway_core::engine::{ContentCapture, Kgateway};
use kgateway_core::error::KgError;
use kgateway_core::provider::{ApiKey, ChunkStream, Provider, ProviderKey};
use kgateway_core::router::Registry;
use kgateway_core::schema::{ChatRequest, ChatResponse, Choice, Message, Role, Usage};

use kgateway_plugins::governance::{GovernancePlugin, VirtualKey};
use kgateway_plugins::logging::LoggingPlugin;
use kgateway_plugins::redaction::Redactor;
use kgateway_store::MemoryLogStore;

/// A provider that returns a fixed, moderately-sized response immediately (no network).
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
        Ok(ChatResponse {
            id: "chatcmpl-bench".into(),
            object: "chat.completion".into(),
            model: "gpt-4o".into(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: Some(
                        "Sure — you can reach the team at support@example.com. \
                         The quick brown fox jumps over the lazy dog."
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
        })
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

fn registry() -> Registry {
    let mut r = Registry::new();
    r.register(
        Arc::new(BenchProvider),
        vec![ApiKey {
            id: "k".into(),
            value: "v".into(),
            weight: 1,
            models: vec![],
        }],
    );
    r
}

fn sample_request() -> ChatRequest {
    ChatRequest {
        model: "openai/gpt-4o".into(),
        messages: vec![
            Message::system("You are a helpful assistant."),
            Message::user("My email is user@example.com — explain relativity briefly."),
        ],
        temperature: Some(0.7),
        max_tokens: Some(512),
        ..Default::default()
    }
}

fn governance() -> GovernancePlugin {
    // A generous virtual key: allow-list only, no rate/budget limits — measures the
    // governance dispatch + lookup + token-accounting path without rejections.
    GovernancePlugin::new(
        vec![VirtualKey {
            id: "vk".into(),
            name: "bench".into(),
            max_total_tokens: Some(u64::MAX),
            ..Default::default()
        }],
        true,
    )
}

fn capture() -> ContentCapture {
    ContentCapture {
        enabled: true,
        max_body_bytes: 16 * 1024,
        capture_streaming: false,
    }
}

fn bench_pipeline(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let req = sample_request();
    let mut group = c.benchmark_group("pipeline");

    // Each variant runs the SAME request through an engine with progressively more layers.
    // `with_governance` requires ctx.virtual_key, so we set it per-iter for those variants.
    macro_rules! bench_variant {
        ($name:expr, $engine:expr, $vkey:expr) => {{
            let engine = $engine;
            group.bench_function($name, |b| {
                b.iter(|| {
                    let mut ctx = Ctx::new();
                    if $vkey {
                        ctx.virtual_key = Some("vk".into());
                    }
                    let resp = rt
                        .block_on(engine.chat(&mut ctx, black_box(req.clone())))
                        .unwrap();
                    black_box(resp);
                });
            });
        }};
    }

    bench_variant!("bare", Kgateway::new(registry()), false);
    bench_variant!(
        "logging",
        Kgateway::new(registry()).with_observer(Arc::new(LoggingPlugin::new(Arc::new(
            MemoryLogStore::default()
        )))),
        false
    );
    bench_variant!(
        "governance",
        Kgateway::new(registry()).with_observer(Arc::new(governance())),
        true
    );
    bench_variant!(
        "full",
        Kgateway::new(registry())
            .with_observer(Arc::new(LoggingPlugin::new(Arc::new(
                MemoryLogStore::default()
            ))))
            .with_observer(Arc::new(governance())),
        true
    );
    bench_variant!(
        "capture",
        Kgateway::new(registry())
            .with_observer(Arc::new(LoggingPlugin::new(Arc::new(
                MemoryLogStore::default()
            ))))
            .with_observer(Arc::new(governance()))
            .with_content_capture(capture()),
        true
    );
    bench_variant!(
        "capture+redaction",
        Kgateway::new(registry())
            .with_observer(Arc::new(
                LoggingPlugin::new(Arc::new(MemoryLogStore::default()))
                    .with_redactor(Arc::new(Redactor::new(&[], Some("bench-key")).unwrap()))
            ))
            .with_observer(Arc::new(governance()))
            .with_content_capture(capture()),
        true
    );

    group.finish();
}

/// Focused, low-variance micro-bench of the redactor itself (no engine/runtime), on a
/// representative body — a mix of prose with an email + an API key.
fn bench_redaction(c: &mut Criterion) {
    let redactor = Redactor::new(&[], Some("bench-key")).unwrap();
    let body = r#"{"messages":[{"role":"user","content":"My email is user@example.com and my key is sk-abcdefghijklmnopqrstuvwxyz012345 — please summarize the attached quarterly report in three concise bullet points for the leadership team."}]}"#;
    let clean = r#"{"messages":[{"role":"user","content":"Please summarize the attached quarterly report in three concise bullet points for the leadership team, focusing on revenue and churn."}]}"#;

    // Mask-only (no key) isolates the regex + string-building cost from AES-GCM crypto.
    let masker = Redactor::new(&[], None).unwrap();

    let mut group = c.benchmark_group("redaction");
    group.bench_function("regex_only_no_match", |b| {
        b.iter(|| black_box(masker.redact(black_box(clean))));
    });
    group.bench_function("regex_only_with_match", |b| {
        b.iter(|| black_box(masker.redact(black_box(body))));
    });
    group.bench_function("with_match_and_crypto", |b| {
        b.iter(|| black_box(redactor.redact(black_box(body))));
    });
    group.finish();
}

criterion_group!(benches, bench_pipeline, bench_redaction);
criterion_main!(benches);
