//! Cohere provider — registered for **embeddings** and **rerank** only (Cohere's
//! strengths). Chat is intentionally unsupported in this cut: Cohere's chat API
//! (`/v2/chat`) has a distinct message/tool shape that does not map onto the
//! gateway's OpenAI-wire schema, so wiring it is a deliberate follow-on.

use async_trait::async_trait;
use kgateway_core::context::Ctx;
use kgateway_core::error::{KgError, KgErrorKind};
use kgateway_core::provider::{
    ApiKey, EmbeddingRequest, EmbeddingResponse, Embeddings, Provider, ProviderKey, Rerank,
    RerankRequest, RerankResponse, RerankResult,
};
use kgateway_core::schema::{ChatRequest, ChatResponse, Usage};
use serde::Deserialize;

const DEFAULT_BASE_URL: &str = "https://api.cohere.com";

pub struct CohereProvider {
    key: ProviderKey,
    base_url: String,
    client: reqwest::Client,
}

impl CohereProvider {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            key: ProviderKey::new("cohere"),
            base_url: base_url.into(),
            client: crate::http::default_client(),
        }
    }
}

impl Default for CohereProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for CohereProvider {
    fn key(&self) -> ProviderKey {
        self.key.clone()
    }

    async fn chat(
        &self,
        _ctx: &Ctx,
        _key: &ApiKey,
        _req: ChatRequest,
    ) -> Result<ChatResponse, KgError> {
        Err(KgError::unsupported("chat for cohere"))
    }

    async fn chat_stream(
        &self,
        _ctx: &Ctx,
        _key: &ApiKey,
        _req: ChatRequest,
    ) -> Result<kgateway_core::provider::ChunkStream, KgError> {
        Err(KgError::unsupported("chat for cohere"))
    }

    fn as_embeddings(&self) -> Option<&dyn Embeddings> {
        Some(self)
    }

    fn as_rerank(&self) -> Option<&dyn Rerank> {
        Some(self)
    }
}

// ---- Embeddings (Cohere v2 `/v2/embed`) ----

// Cohere returns embeddings grouped by requested type, e.g.
// `{ "embeddings": { "float": [[...], [...]] } }`.
#[derive(Deserialize)]
struct CohereEmbedResponse {
    embeddings: CohereEmbeddings,
}

#[derive(Deserialize)]
struct CohereEmbeddings {
    #[serde(default)]
    float: Vec<Vec<f32>>,
}

#[async_trait]
impl Embeddings for CohereProvider {
    async fn embed(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, KgError> {
        let url = format!("{}/v2/embed", self.base_url);
        let body = serde_json::json!({
            "model": req.model,
            "texts": req.input,
            "input_type": "search_document",
            "embedding_types": ["float"],
        });
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&key.value)
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

        let parsed: CohereEmbedResponse = resp
            .json()
            .await
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))?;
        // Cohere token usage lives under `meta.billed_units`; default usage is fine here.
        Ok(EmbeddingResponse {
            model: req.model,
            embeddings: parsed.embeddings.float,
            usage: Usage::default(),
        })
    }
}

// ---- Rerank (Cohere v2 `/v2/rerank`) ----

#[derive(Deserialize)]
struct CohereRerankResponse {
    results: Vec<CohereRerankResult>,
}

#[derive(Deserialize)]
struct CohereRerankResult {
    index: usize,
    relevance_score: f32,
}

#[async_trait]
impl Rerank for CohereProvider {
    async fn rerank(
        &self,
        _ctx: &Ctx,
        key: &ApiKey,
        req: RerankRequest,
    ) -> Result<RerankResponse, KgError> {
        let url = format!("{}/v2/rerank", self.base_url);
        let mut body = serde_json::json!({
            "model": req.model,
            "query": req.query,
            "documents": req.documents,
        });
        if let Some(top_n) = req.top_n {
            body["top_n"] = serde_json::Value::from(top_n);
        }

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&key.value)
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

        let parsed: CohereRerankResponse = resp
            .json()
            .await
            .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))?;
        Ok(RerankResponse {
            results: parsed
                .results
                .into_iter()
                .map(|r| RerankResult {
                    index: r.index,
                    relevance_score: r.relevance_score,
                })
                .collect(),
        })
    }
}

fn net_err(e: reqwest::Error) -> KgError {
    KgError::new(KgErrorKind::Network, e.to_string()).with_retryable(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_key() -> ApiKey {
        ApiKey {
            id: "k".into(),
            value: "co-key".into(),
            weight: 1,
            models: vec![],
        }
    }

    #[tokio::test]
    async fn embeddings_decode() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/embed"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "embeddings": { "float": [[0.1, 0.2], [0.3, 0.4]] }
            })))
            .mount(&server)
            .await;

        let provider = CohereProvider::with_base_url(server.uri());
        let ctx = Ctx::new();
        let resp = provider
            .embed(
                &ctx,
                &test_key(),
                EmbeddingRequest {
                    model: "embed-v4.0".into(),
                    input: vec!["a".into(), "b".into()],
                },
            )
            .await
            .expect("embeddings should decode");

        assert_eq!(resp.embeddings.len(), 2);
        assert_eq!(resp.embeddings[0], vec![0.1, 0.2]);
        assert_eq!(resp.embeddings[1], vec![0.3, 0.4]);
        assert_eq!(resp.model, "embed-v4.0");
    }

    #[tokio::test]
    async fn rerank_decode() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/rerank"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [
                    { "index": 2, "relevance_score": 0.9 },
                    { "index": 0, "relevance_score": 0.4 }
                ]
            })))
            .mount(&server)
            .await;

        let provider = CohereProvider::with_base_url(server.uri());
        let ctx = Ctx::new();
        let resp = provider
            .rerank(
                &ctx,
                &test_key(),
                RerankRequest {
                    model: "rerank-v3.5".into(),
                    query: "q".into(),
                    documents: vec!["d0".into(), "d1".into(), "d2".into()],
                    top_n: Some(2),
                },
            )
            .await
            .expect("rerank should decode");

        assert_eq!(resp.results.len(), 2);
        // Order preserved as returned by Cohere.
        assert_eq!(resp.results[0].index, 2);
        assert_eq!(resp.results[0].relevance_score, 0.9);
        assert_eq!(resp.results[1].index, 0);
        assert_eq!(resp.results[1].relevance_score, 0.4);
    }

    #[tokio::test]
    async fn rerank_maps_error_status_and_provider() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/rerank"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .mount(&server)
            .await;

        let provider = CohereProvider::with_base_url(server.uri());
        let ctx = Ctx::new();
        let err = provider
            .rerank(
                &ctx,
                &test_key(),
                RerankRequest {
                    model: "rerank-v3.5".into(),
                    query: "q".into(),
                    documents: vec!["d0".into()],
                    top_n: None,
                },
            )
            .await
            .expect_err("400 should map to an error");

        assert_eq!(err.status, Some(400));
        assert_eq!(err.provider.as_deref(), Some("cohere"));
    }
}
