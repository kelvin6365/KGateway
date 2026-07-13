//! Semantic cache plugin: embeds the request, looks up a similar cached response above
//! a similarity threshold in `pre_llm` (short-circuiting on a hit), and stores the
//! response in `post_llm` on a miss. See `docs/04-plugins.md`.
//!
//! `pre_llm` receives the request but `post_llm` does not, so the query embedding for a
//! cache miss is parked in a pending map keyed by `ctx.request_id` and consumed in
//! `post_llm`. Cache/embedding failures are non-blocking — they never fail a request.

use async_trait::async_trait;
use kgateway_core::context::{Ctx, RequestId};
use kgateway_core::error::KgError;
use kgateway_core::plugin::{LlmPlugin, Plugin, PreOutcome};
use kgateway_core::provider::{ApiKey, EmbeddingRequest, Embeddings};
use kgateway_core::schema::{ChatRequest, ChatResponse, Message};
use kgateway_store::VectorStore;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Parked cache-miss state, keyed by `request_id`, consumed in `post_llm`.
struct Pending {
    scope_key: String,
    exact_key: String,
    embedding: Vec<f32>,
}

/// Produces an embedding vector for a piece of text. Abstracted so the cache is unit
/// testable without a live embeddings API.
#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, KgError>;
}

/// Production embedder: wraps a provider's [`Embeddings`] capability + a key + model.
pub struct ProviderEmbedder {
    provider: Arc<dyn Embeddings>,
    key: ApiKey,
    model: String,
}

impl ProviderEmbedder {
    pub fn new(provider: Arc<dyn Embeddings>, key: ApiKey, model: impl Into<String>) -> Self {
        Self {
            provider,
            key,
            model: model.into(),
        }
    }
}

#[async_trait]
impl Embedder for ProviderEmbedder {
    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, KgError> {
        let req = EmbeddingRequest {
            model: self.model.clone(),
            input: vec![text.to_string()],
        };
        let resp = self.provider.embed(&Ctx::new(), &self.key, req).await?;
        resp.embeddings
            .into_iter()
            .next()
            .ok_or_else(|| KgError::internal("embedder returned no vectors"))
    }
}

/// Embedding-similarity response cache.
pub struct SemanticCachePlugin {
    embedder: Arc<dyn Embedder>,
    store: Arc<dyn VectorStore>,
    /// Minimum cosine similarity (0..1) for a cache hit.
    threshold: f32,
    /// Parked miss state for in-flight requests, awaiting their response.
    pending: Mutex<HashMap<RequestId, Pending>>,
}

impl SemanticCachePlugin {
    pub fn new(embedder: Arc<dyn Embedder>, store: Arc<dyn VectorStore>, threshold: f32) -> Self {
        Self {
            embedder,
            store,
            threshold,
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Scope text: the request serialized MINUS the messages (the semantic signal) and
    /// routing-only fields. Two requests share a semantic-cache scope only if ALL params
    /// match, so a cached response is never served across different model/params/tools —
    /// including newly-modeled params (`seed`, `response_format`, penalties, …) and anything
    /// in the `extra` passthrough — without listing each field by hand. `serde_json::Map` is
    /// sorted, so the output is deterministic (a metadata-filter approach, folded into a key).
    fn scope_text(req: &ChatRequest) -> String {
        let mut v = serde_json::to_value(req).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.remove("messages"); // the semantic signal (embedded separately)
            o.remove("stream"); // stream vs non-stream shares the same content
            o.remove("fallbacks"); // routing-only
        }
        v.to_string()
    }

    /// The message content that carries the semantic signal (what actually gets embedded).
    fn messages_text(req: &ChatRequest) -> String {
        let mut s = String::new();
        for m in &req.messages {
            s.push_str(&Self::message_line(m));
            s.push('\n');
        }
        s
    }

    fn message_line(m: &Message) -> String {
        format!("{:?}:{}", m.role, m.content.as_deref().unwrap_or(""))
    }

    /// Stable (cross-restart) SHA-256 hex digest — used for both cache keys.
    fn hash(s: &str) -> String {
        format!("{:x}", Sha256::digest(s.as_bytes()))
    }
}

impl Plugin for SemanticCachePlugin {
    fn name(&self) -> &str {
        "semantic-cache"
    }
}

#[async_trait]
impl LlmPlugin for SemanticCachePlugin {
    async fn pre_llm(&self, ctx: &Ctx, req: ChatRequest) -> Result<PreOutcome, KgError> {
        let scope_key = Self::hash(&Self::scope_text(&req));
        let messages_text = Self::messages_text(&req);
        let exact_key = Self::hash(&format!("{scope_key}\n{messages_text}"));

        // Tier 1 — exact match: an identical repeat request hits without embedding at all.
        match self.store.get_exact(&exact_key).await {
            Ok(Some(payload)) => {
                if let Ok(cached) = serde_json::from_str::<ChatResponse>(&payload) {
                    tracing::debug!("semantic-cache exact hit");
                    return Ok(PreOutcome::ShortCircuit(cached));
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "semantic-cache exact lookup failed (bypassing cache)");
                return Ok(PreOutcome::Continue(req));
            }
        }

        // Tier 2 — semantic: embed the messages and search within the same scope.
        let embedding = match self.embedder.embed_text(&messages_text).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "semantic-cache embed failed (bypassing cache)");
                return Ok(PreOutcome::Continue(req));
            }
        };

        match self
            .store
            .search(&scope_key, &embedding, self.threshold)
            .await
        {
            Ok(Some(hit)) => {
                if let Ok(cached) = serde_json::from_str::<ChatResponse>(&hit.payload) {
                    tracing::debug!(similarity = hit.similarity, "semantic-cache semantic hit");
                    return Ok(PreOutcome::ShortCircuit(cached));
                }
                // Corrupt payload: fall through to a miss.
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "semantic-cache search failed (bypassing cache)");
                return Ok(PreOutcome::Continue(req));
            }
        }

        // Miss: remember the keys + embedding so post_llm can store the response.
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(
                ctx.request_id,
                Pending {
                    scope_key,
                    exact_key,
                    embedding,
                },
            );
        }
        Ok(PreOutcome::Continue(req))
    }

    async fn post_llm(
        &self,
        ctx: &Ctx,
        resp: Result<ChatResponse, KgError>,
    ) -> Result<ChatResponse, KgError> {
        // Consume the parked miss state (present only for misses we chose to cache).
        let pending = self
            .pending
            .lock()
            .ok()
            .and_then(|mut p| p.remove(&ctx.request_id));

        if let (Some(p), Ok(r)) = (pending, resp.as_ref()) {
            if let Ok(payload) = serde_json::to_string(r) {
                if let Err(e) = self
                    .store
                    .insert(&p.scope_key, &p.exact_key, p.embedding, payload)
                    .await
                {
                    tracing::warn!(error = %e, "semantic-cache store failed");
                }
            }
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgateway_core::schema::{Choice, Role, Usage};
    use kgateway_store::InMemoryVectorStore;

    // Deterministic bag-of-bytes embedder: identical text → identical vector (cosine 1.0);
    // different text → different distribution (lower similarity). 16 buckets.
    struct BagEmbedder;
    #[async_trait]
    impl Embedder for BagEmbedder {
        async fn embed_text(&self, text: &str) -> Result<Vec<f32>, KgError> {
            let mut v = vec![0.0f32; 16];
            for b in text.bytes() {
                v[(b % 16) as usize] += 1.0;
            }
            Ok(v)
        }
    }

    fn req(content: &str) -> ChatRequest {
        ChatRequest {
            model: "openai/gpt-4o".into(),
            messages: vec![Message::user(content)],
            ..Default::default()
        }
    }

    fn response(content: &str) -> ChatResponse {
        ChatResponse {
            id: "r".into(),
            object: "chat.completion".into(),
            model: "gpt-4o".into(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: Some(content.into()),
                    name: None,
                    tool_calls: vec![],
                    tool_call_id: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage::default(),
        }
    }

    fn plugin() -> SemanticCachePlugin {
        SemanticCachePlugin::new(
            Arc::new(BagEmbedder),
            Arc::new(InMemoryVectorStore::new()),
            0.98,
        )
    }

    #[tokio::test]
    async fn miss_then_hit_short_circuits() {
        let cache = plugin();

        // 1st request: miss → Continue, then store the response in post_llm.
        let ctx1 = Ctx::new();
        let out = cache.pre_llm(&ctx1, req("what is 2+2?")).await.unwrap();
        assert!(matches!(out, PreOutcome::Continue(_)), "first is a miss");
        let _ = cache
            .post_llm(&ctx1, Ok(response("4")))
            .await
            .expect("post_llm ok");

        // 2nd identical request: hit → ShortCircuit with the cached response.
        let ctx2 = Ctx::new();
        let out = cache.pre_llm(&ctx2, req("what is 2+2?")).await.unwrap();
        match out {
            PreOutcome::ShortCircuit(resp) => {
                assert_eq!(resp.choices[0].message.content.as_deref(), Some("4"));
            }
            _ => panic!("expected cache hit short-circuit"),
        }
    }

    #[tokio::test]
    async fn different_prompt_is_a_miss() {
        let cache = plugin();
        let ctx1 = Ctx::new();
        let _ = cache.pre_llm(&ctx1, req("aaaaaaaa")).await.unwrap();
        let _ = cache.post_llm(&ctx1, Ok(response("A"))).await.unwrap();

        // A very different prompt should not hit the cached one.
        let ctx2 = Ctx::new();
        let out = cache
            .pre_llm(&ctx2, req("zzzzzzzzzzzzzzzz different"))
            .await
            .unwrap();
        assert!(
            matches!(out, PreOutcome::Continue(_)),
            "dissimilar prompt must miss"
        );
    }

    #[tokio::test]
    async fn post_llm_returns_result_unchanged() {
        let cache = plugin();
        let ctx = Ctx::new();
        let _ = cache.pre_llm(&ctx, req("hello")).await.unwrap();
        let out = cache.post_llm(&ctx, Ok(response("hi"))).await.unwrap();
        assert_eq!(out.choices[0].message.content.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn different_params_do_not_collide() {
        // Same messages, different sampling params must NOT share a cache entry.
        let cache = plugin();
        let ctx1 = Ctx::new();
        let mut r1 = req("same prompt");
        r1.temperature = Some(0.0);
        let _ = cache.pre_llm(&ctx1, r1).await.unwrap();
        let _ = cache.post_llm(&ctx1, Ok(response("cold"))).await.unwrap();

        let ctx2 = Ctx::new();
        let mut r2 = req("same prompt");
        r2.temperature = Some(1.0);
        let out = cache.pre_llm(&ctx2, r2).await.unwrap();
        assert!(
            matches!(out, PreOutcome::Continue(_)),
            "different temperature must miss (no cross-params cache poisoning)"
        );
    }

    // Embedder that counts calls, to prove the exact tier skips embedding on repeats.
    struct CountingEmbedder(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait]
    impl Embedder for CountingEmbedder {
        async fn embed_text(&self, text: &str) -> Result<Vec<f32>, KgError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            BagEmbedder.embed_text(text).await
        }
    }

    #[tokio::test]
    async fn exact_repeat_skips_embedding() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let count = Arc::new(AtomicUsize::new(0));
        let cache = SemanticCachePlugin::new(
            Arc::new(CountingEmbedder(count.clone())),
            Arc::new(InMemoryVectorStore::new()),
            0.98,
        );

        // 1st request: miss → embeds once, stores in post_llm.
        let ctx1 = Ctx::new();
        let _ = cache.pre_llm(&ctx1, req("hi")).await.unwrap();
        let _ = cache.post_llm(&ctx1, Ok(response("yo"))).await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // 2nd identical request: exact-tier hit → NO embedding.
        let ctx2 = Ctx::new();
        let out = cache.pre_llm(&ctx2, req("hi")).await.unwrap();
        assert!(
            matches!(out, PreOutcome::ShortCircuit(_)),
            "exact repeat hits"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "exact repeat must not re-embed"
        );
    }
}
