//! Vector store for the semantic cache (M5). Stores request embeddings mapped to a
//! serialized response payload and looks up the nearest neighbor by cosine similarity.
//!
//! `InMemoryVectorStore` is a brute-force baseline: every `search` scans all stored
//! embeddings. That is fine for M5; a proper ANN index is a later swap-in behind the
//! same `VectorStore` trait.

use async_trait::async_trait;

use crate::StoreError;

/// A single nearest-neighbor match returned by [`VectorStore::search`].
#[derive(Debug, Clone)]
pub struct VectorHit {
    /// The serialized response payload stored alongside the matched embedding.
    pub payload: String,
    /// Cosine similarity of the matched embedding to the query (in `[-1.0, 1.0]`).
    pub similarity: f32,
}

/// Cosine similarity between two vectors: `dot(a, b) / (‖a‖·‖b‖)`.
///
/// Returns `0.0` when either vector has zero norm or when the lengths differ, so
/// callers never see `NaN` from a degenerate input.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// Approximate-nearest-neighbor store for semantic caching.
// `len` is a metrics/test convenience on an async trait; an `is_empty` counterpart
// would be redundant here, so silence the clippy lint that pairs them.
#[allow(clippy::len_without_is_empty)]
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Store an entry with two keys + an embedding + serialized `payload`:
    /// - `scope_key`: hash of model + sampling params (NOT the messages) — the semantic
    ///   tier only matches within the same scope, so a hit is never served across different
    ///   models/params (a metadata-filter approach).
    /// - `exact_key`: hash of the FULL request (scope + messages) — powers the O(1)
    ///   exact-match tier.
    async fn insert(
        &self,
        scope_key: &str,
        exact_key: &str,
        embedding: Vec<f32>,
        payload: String,
    ) -> Result<(), StoreError>;

    /// Exact-match tier: O(1) fetch of a payload by its `exact_key`, or `None`. Lets an
    /// identical repeat request hit the cache without embedding or a similarity scan.
    /// Default returns `None` (a store may support only the semantic tier).
    async fn get_exact(&self, _exact_key: &str) -> Result<Option<String>, StoreError> {
        Ok(None)
    }

    /// Semantic tier, SCOPED to `scope_key`: the best match within the same model+params
    /// whose cosine similarity is `>= min_similarity`, or `None`.
    async fn search(
        &self,
        scope_key: &str,
        embedding: &[f32],
        min_similarity: f32,
    ) -> Result<Option<VectorHit>, StoreError>;

    /// Number of stored entries (handy for tests and metrics).
    async fn len(&self) -> usize;
}

/// Brute-force in-memory [`VectorStore`]. Semantic entries live in a `Vec`; the exact-match
/// tier is a `HashMap<exact_key, payload>`.
#[derive(Default)]
pub struct InMemoryVectorStore {
    inner: std::sync::Mutex<Vec<(String, Vec<f32>, String)>>, // (scope_key, embedding, payload)
    exact: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl InMemoryVectorStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl VectorStore for InMemoryVectorStore {
    async fn insert(
        &self,
        scope_key: &str,
        exact_key: &str,
        embedding: Vec<f32>,
        payload: String,
    ) -> Result<(), StoreError> {
        self.exact
            .lock()
            .unwrap()
            .insert(exact_key.to_string(), payload.clone());
        self.inner
            .lock()
            .unwrap()
            .push((scope_key.to_string(), embedding, payload));
        Ok(())
    }

    async fn get_exact(&self, exact_key: &str) -> Result<Option<String>, StoreError> {
        Ok(self.exact.lock().unwrap().get(exact_key).cloned())
    }

    async fn search(
        &self,
        scope_key: &str,
        embedding: &[f32],
        min_similarity: f32,
    ) -> Result<Option<VectorHit>, StoreError> {
        let guard = self.inner.lock().unwrap();
        let mut best: Option<VectorHit> = None;
        for (scope, stored, payload) in guard.iter() {
            // Only match within the same scope (model + params) and matching dimension.
            if scope != scope_key || stored.len() != embedding.len() {
                continue;
            }
            let similarity = cosine_similarity(stored, embedding);
            if similarity >= min_similarity
                && best.as_ref().is_none_or(|b| similarity > b.similarity)
            {
                best = Some(VectorHit {
                    payload: payload.clone(),
                    similarity,
                });
            }
        }
        Ok(best)
    }

    async fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-6;

    #[test]
    fn cosine_identical_is_one() {
        let v = [1.0, 2.0, 3.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < EPS);
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < EPS);
    }

    #[test]
    fn cosine_opposite_is_minus_one() {
        assert!((cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]) - (-1.0)).abs() < EPS);
    }

    #[test]
    fn cosine_mismatched_lengths_is_zero() {
        assert_eq!(cosine_similarity(&[1.0, 0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn cosine_zero_vector_is_zero() {
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 2.0]), 0.0);
    }

    #[tokio::test]
    async fn search_returns_nearest_neighbor() {
        let store = InMemoryVectorStore::new();
        store
            .insert("s", "first", vec![1.0, 0.0], "first".into())
            .await
            .unwrap();
        store
            .insert("s", "second", vec![0.0, 1.0], "second".into())
            .await
            .unwrap();

        // Query pointing almost along the first entry.
        let hit = store
            .search("s", &[0.99, 0.01], 0.5)
            .await
            .unwrap()
            .expect("expected a match");
        assert_eq!(hit.payload, "first");
        assert!(hit.similarity > 0.9);
    }

    #[tokio::test]
    async fn search_high_threshold_yields_none() {
        let store = InMemoryVectorStore::new();
        store
            .insert("s", "first", vec![1.0, 0.0], "first".into())
            .await
            .unwrap();
        store
            .insert("s", "second", vec![0.0, 1.0], "second".into())
            .await
            .unwrap();

        // Nothing is within 0.99 of a 45-degree query.
        let hit = store.search("s", &[0.7, 0.7], 0.99).await.unwrap();
        assert!(hit.is_none());
    }

    #[tokio::test]
    async fn search_empty_store_yields_none() {
        let store = InMemoryVectorStore::new();
        assert!(store.search("s", &[1.0, 0.0], 0.0).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn len_reflects_inserts() {
        let store = InMemoryVectorStore::new();
        assert_eq!(store.len().await, 0);
        store
            .insert("s", "a", vec![1.0, 0.0], "a".into())
            .await
            .unwrap();
        store
            .insert("s", "b", vec![0.0, 1.0], "b".into())
            .await
            .unwrap();
        assert_eq!(store.len().await, 2);
    }

    #[tokio::test]
    async fn get_exact_is_o1_exact_match_tier() {
        let store = InMemoryVectorStore::new();
        assert!(store.get_exact("k").await.unwrap().is_none());
        store
            .insert("s", "k", vec![1.0, 0.0], "cached".into())
            .await
            .unwrap();
        assert_eq!(
            store.get_exact("k").await.unwrap().as_deref(),
            Some("cached")
        );
        assert!(store.get_exact("other").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn search_skips_mismatched_dimensions() {
        let store = InMemoryVectorStore::new();
        // Wrong dimension entry must be skipped, not panic.
        store
            .insert("s", "wrong_dim", vec![1.0, 0.0, 0.0], "wrong_dim".into())
            .await
            .unwrap();
        store
            .insert("s", "right_dim", vec![1.0, 0.0], "right_dim".into())
            .await
            .unwrap();

        let hit = store
            .search("s", &[1.0, 0.0], 0.5)
            .await
            .unwrap()
            .expect("valid entry should still match");
        assert_eq!(hit.payload, "right_dim");
    }

    #[tokio::test]
    async fn search_only_mismatched_dimension_yields_none() {
        let store = InMemoryVectorStore::new();
        store
            .insert("s", "wrong_dim", vec![1.0, 0.0, 0.0], "wrong_dim".into())
            .await
            .unwrap();
        assert!(store.search("s", &[1.0, 0.0], 0.0).await.unwrap().is_none());
    }
}
