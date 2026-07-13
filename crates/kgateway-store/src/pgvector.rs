//! Postgres + `pgvector`-backed [`VectorStore`] for the semantic cache (M5). Unlike
//! [`crate::InMemoryVectorStore`] (RAM-only, lost on restart), this persists cache entries
//! and lets multiple gateway replicas share one cache.
//!
//! Storage uses pgvector's `vector` column type; nearest-neighbor lookup uses the `<=>`
//! cosine-distance operator, where `1 - (a <=> b)` is the cosine similarity — matching
//! `cosine_similarity` used by the in-memory store. We deliberately use the runtime
//! `sqlx::query` API (not the compile-time macros) so the crate builds offline, and bind
//! embeddings via pgvector's text representation (`[v0,v1,...]`) to avoid an extra
//! `pgvector`-sqlx integration crate.

use async_trait::async_trait;
use sqlx::postgres::{PgPool, PgPoolOptions};

use crate::{StoreError, VectorHit, VectorStore};

/// Inline migration: enable the extension and create the table. `embedding` is an
/// unconstrained `vector` — the semantic cache uses a single embedding model, so all rows
/// share a dimension, and comparisons stay valid.
const MIGRATE: &[&str] = &[
    "CREATE EXTENSION IF NOT EXISTS vector",
    "CREATE TABLE IF NOT EXISTS cache_vectors (\
        id         BIGSERIAL PRIMARY KEY,\
        scope_key  TEXT   NOT NULL,\
        exact_key  TEXT   NOT NULL,\
        embedding  vector NOT NULL,\
        payload    TEXT   NOT NULL)",
    // Index the exact-match tier and the semantic-scope filter.
    "CREATE INDEX IF NOT EXISTS cache_vectors_exact_key ON cache_vectors (exact_key)",
    "CREATE INDEX IF NOT EXISTS cache_vectors_scope_key ON cache_vectors (scope_key)",
];

/// Postgres/pgvector-backed vector store.
#[derive(Clone)]
pub struct PgVectorStore {
    pool: PgPool,
}

impl PgVectorStore {
    /// Open a pool against `url` and run the inline migration. Fails (so the caller can fall
    /// back to in-memory) if the `vector` extension isn't installed/permitted.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new().max_connections(5).connect(url).await?;
        Self::from_pool(pool).await
    }

    /// Build from an existing pool (e.g. shared with the log store), running the migration.
    pub async fn from_pool(pool: PgPool) -> Result<Self, StoreError> {
        for stmt in MIGRATE {
            sqlx::query(stmt).execute(&pool).await?;
        }
        Ok(Self { pool })
    }
}

/// Format a vector as pgvector's text literal: `[v0,v1,...]`.
fn to_pgvector(embedding: &[f32]) -> String {
    let mut s = String::with_capacity(embedding.len() * 8 + 2);
    s.push('[');
    for (i, x) in embedding.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push(']');
    s
}

#[async_trait]
impl VectorStore for PgVectorStore {
    async fn insert(
        &self,
        scope_key: &str,
        exact_key: &str,
        embedding: Vec<f32>,
        payload: String,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO cache_vectors (scope_key, exact_key, embedding, payload) \
             VALUES ($1, $2, $3::vector, $4)",
        )
        .bind(scope_key)
        .bind(exact_key)
        .bind(to_pgvector(&embedding))
        .bind(payload)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_exact(&self, exact_key: &str) -> Result<Option<String>, StoreError> {
        // Newest match wins if the same key was cached more than once.
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT payload FROM cache_vectors WHERE exact_key = $1 ORDER BY id DESC LIMIT 1",
        )
        .bind(exact_key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(p,)| p))
    }

    async fn search(
        &self,
        scope_key: &str,
        embedding: &[f32],
        min_similarity: f32,
    ) -> Result<Option<VectorHit>, StoreError> {
        // `<=>` is cosine distance; `1 - distance` is cosine similarity. Scoped to the same
        // model+params, then ORDER BY distance ascending + LIMIT 1 = the nearest neighbor.
        let query = to_pgvector(embedding);
        let row: Option<(String, f64)> = sqlx::query_as(
            "SELECT payload, 1 - (embedding <=> $2::vector) AS similarity \
             FROM cache_vectors WHERE scope_key = $1 \
             ORDER BY embedding <=> $2::vector LIMIT 1",
        )
        .bind(scope_key)
        .bind(&query)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.and_then(|(payload, sim)| {
            let similarity = sim as f32;
            (similarity >= min_similarity).then_some(VectorHit {
                payload,
                similarity,
            })
        }))
    }

    async fn len(&self) -> usize {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM cache_vectors")
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips through a real Postgres with pgvector. Gated on `KGATEWAY_TEST_PGVECTOR`
    /// (a PG URL); a clean no-op skip when unset (as in CI / offline builds).
    #[tokio::test]
    async fn insert_search_roundtrip() {
        let Ok(url) = std::env::var("KGATEWAY_TEST_PGVECTOR") else {
            eprintln!("KGATEWAY_TEST_PGVECTOR unset; skipping pgvector round-trip test");
            return;
        };
        let store = PgVectorStore::connect(&url).await.expect("connect");
        // Unique payloads so repeat runs against a shared DB don't confuse the assertion.
        let tag = format!("pgv-{}", std::process::id());
        let scope = format!("{tag}-scope");
        store
            .insert(
                &scope,
                &format!("{tag}-kx"),
                vec![1.0, 0.0, 0.0],
                format!("{tag}-x"),
            )
            .await
            .expect("insert x");
        store
            .insert(
                &scope,
                &format!("{tag}-ky"),
                vec![0.0, 1.0, 0.0],
                format!("{tag}-y"),
            )
            .await
            .expect("insert y");

        // Exact-match tier.
        assert_eq!(
            store
                .get_exact(&format!("{tag}-kx"))
                .await
                .expect("get_exact"),
            Some(format!("{tag}-x"))
        );

        // Semantic tier (scoped).
        let hit = store
            .search(&scope, &[0.98, 0.02, 0.0], 0.5)
            .await
            .expect("search")
            .expect("a match");
        assert_eq!(hit.payload, format!("{tag}-x"));
        assert!(hit.similarity > 0.9, "similarity was {}", hit.similarity);

        // A different scope must not match.
        assert!(store
            .search("other-scope", &[0.98, 0.02, 0.0], 0.5)
            .await
            .expect("search")
            .is_none());

        // A high threshold clears nothing near a 45° query.
        assert!(store
            .search(&scope, &[0.7, 0.7, 0.0], 0.999)
            .await
            .expect("search")
            .is_none());
    }
}
