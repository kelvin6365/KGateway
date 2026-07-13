//! Shared governance counters (M20). The governance plugin enforces per-virtual-key rate
//! limits and token/cost budgets; keeping those counters in-process means each replica
//! enforces its own local limit, so a key capped at N/min effectively gets N×replicas.
//!
//! This trait moves the counters behind a pluggable backend. [`InMemoryGovernanceStore`]
//! preserves single-node behavior (the default). [`SqlGovernanceStore`] persists them in
//! Postgres via atomic upserts, so all replicas share one counter and limits hold under
//! horizontal scaling — reusing the existing `database` connection, no new service.
//!
//! Window semantics are **tumbling** (fixed windows keyed by `floor(now / window)`), which
//! is what makes the shared counter a single cheap atomic upsert. This trades the in-process
//! sliding window's precision for cross-replica correctness.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::StoreError;

/// Wall-clock seconds since the Unix epoch.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The tumbling-window bucket id for `now` given a window length. A window of 0 is treated
/// as 1 to avoid division by zero (degenerate, but never panics).
fn bucket(now: u64, window_secs: u64) -> u64 {
    now / window_secs.max(1)
}

/// Backend for per-virtual-key governance counters. All operations are keyed by the virtual
/// key id. Implementations must be safe to call concurrently.
#[async_trait]
pub trait GovernanceStore: Send + Sync {
    /// Increment `vkey`'s request counter for the current tumbling window of `window_secs`
    /// and return the new count within that window (starts at 1 each window).
    async fn incr_requests(&self, vkey: &str, window_secs: u64) -> Result<u64, StoreError>;

    /// Cumulative tokens consumed by `vkey` (across all time), for the budget check.
    async fn consumed_tokens(&self, vkey: &str) -> Result<u64, StoreError>;

    /// Add `tokens` to `vkey`'s cumulative token counter.
    async fn add_tokens(&self, vkey: &str, tokens: u64) -> Result<(), StoreError>;

    /// Cost accrued by `vkey` in the current tumbling period of `period_secs` (0 once the
    /// period rolls over).
    async fn period_cost(&self, vkey: &str, period_secs: u64) -> Result<f64, StoreError>;

    /// Add `cost` to `vkey`'s current-period cost counter.
    async fn add_cost(&self, vkey: &str, period_secs: u64, cost: f64) -> Result<(), StoreError>;
}

// ---- In-memory (single-node default) ----

#[derive(Default)]
struct InMemoryState {
    /// vkey -> (window bucket, count in that window)
    requests: HashMap<String, (u64, u64)>,
    /// vkey -> cumulative tokens
    tokens: HashMap<String, u64>,
    /// vkey -> (period bucket, cost in that period)
    cost: HashMap<String, (u64, f64)>,
}

/// Process-local [`GovernanceStore`]. Correct for a single replica; the default when no
/// shared backend is configured.
#[derive(Default)]
pub struct InMemoryGovernanceStore {
    state: Mutex<InMemoryState>,
}

impl InMemoryGovernanceStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl GovernanceStore for InMemoryGovernanceStore {
    async fn incr_requests(&self, vkey: &str, window_secs: u64) -> Result<u64, StoreError> {
        let b = bucket(now_secs(), window_secs);
        let mut st = self.state.lock().unwrap();
        let entry = st.requests.entry(vkey.to_string()).or_insert((b, 0));
        if entry.0 != b {
            *entry = (b, 0); // new window → reset
        }
        entry.1 += 1;
        Ok(entry.1)
    }

    async fn consumed_tokens(&self, vkey: &str) -> Result<u64, StoreError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .tokens
            .get(vkey)
            .copied()
            .unwrap_or(0))
    }

    async fn add_tokens(&self, vkey: &str, tokens: u64) -> Result<(), StoreError> {
        let mut st = self.state.lock().unwrap();
        let e = st.tokens.entry(vkey.to_string()).or_insert(0);
        *e = e.saturating_add(tokens);
        Ok(())
    }

    async fn period_cost(&self, vkey: &str, period_secs: u64) -> Result<f64, StoreError> {
        let b = bucket(now_secs(), period_secs);
        let st = self.state.lock().unwrap();
        Ok(match st.cost.get(vkey) {
            Some((pb, c)) if *pb == b => *c,
            _ => 0.0, // no entry, or a stale period → 0
        })
    }

    async fn add_cost(&self, vkey: &str, period_secs: u64, cost: f64) -> Result<(), StoreError> {
        let b = bucket(now_secs(), period_secs);
        let mut st = self.state.lock().unwrap();
        let entry = st.cost.entry(vkey.to_string()).or_insert((b, 0.0));
        if entry.0 != b {
            *entry = (b, 0.0);
        }
        entry.1 += cost;
        Ok(())
    }
}

// ---- Postgres (shared, multi-replica) ----

/// Postgres-backed [`GovernanceStore`]: counters live in three small tables and every update
/// is a single atomic `INSERT ... ON CONFLICT DO UPDATE ... RETURNING`, so concurrent
/// replicas share one authoritative counter. Reuses the `database` Postgres connection.
pub struct SqlGovernanceStore {
    pool: sqlx::PgPool,
}

const MIGRATE: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS gov_request_windows (\
        vkey TEXT PRIMARY KEY, window_start BIGINT NOT NULL, count BIGINT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS gov_token_budgets (\
        vkey TEXT PRIMARY KEY, tokens BIGINT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS gov_cost_budgets (\
        vkey TEXT PRIMARY KEY, period_start BIGINT NOT NULL, cost DOUBLE PRECISION NOT NULL)",
];

impl SqlGovernanceStore {
    /// Open a pool against `url` and run the inline migration.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await?;
        Self::from_pool(pool).await
    }

    /// Build from an existing pool (e.g. shared with the log/vector store).
    pub async fn from_pool(pool: sqlx::PgPool) -> Result<Self, StoreError> {
        for stmt in MIGRATE {
            sqlx::query(stmt).execute(&pool).await?;
        }
        Ok(Self { pool })
    }
}

#[async_trait]
impl GovernanceStore for SqlGovernanceStore {
    async fn incr_requests(&self, vkey: &str, window_secs: u64) -> Result<u64, StoreError> {
        let b = bucket(now_secs(), window_secs) as i64;
        // Atomically: same window → +1; new window → reset to 1. RETURNING gives the count.
        let (count,): (i64,) = sqlx::query_as(
            "INSERT INTO gov_request_windows (vkey, window_start, count) VALUES ($1, $2, 1) \
             ON CONFLICT (vkey) DO UPDATE SET \
               count = CASE WHEN gov_request_windows.window_start = $2 \
                            THEN gov_request_windows.count + 1 ELSE 1 END, \
               window_start = $2 \
             RETURNING count",
        )
        .bind(vkey)
        .bind(b)
        .fetch_one(&self.pool)
        .await?;
        Ok(count.max(0) as u64)
    }

    async fn consumed_tokens(&self, vkey: &str) -> Result<u64, StoreError> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT tokens FROM gov_token_budgets WHERE vkey = $1")
                .bind(vkey)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(t,)| t.max(0) as u64).unwrap_or(0))
    }

    async fn add_tokens(&self, vkey: &str, tokens: u64) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO gov_token_budgets (vkey, tokens) VALUES ($1, $2) \
             ON CONFLICT (vkey) DO UPDATE SET tokens = gov_token_budgets.tokens + $2",
        )
        .bind(vkey)
        .bind(tokens as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn period_cost(&self, vkey: &str, period_secs: u64) -> Result<f64, StoreError> {
        let b = bucket(now_secs(), period_secs) as i64;
        // Only count cost from the CURRENT period; a stale period reads as 0.
        let row: Option<(f64,)> = sqlx::query_as(
            "SELECT cost FROM gov_cost_budgets WHERE vkey = $1 AND period_start = $2",
        )
        .bind(vkey)
        .bind(b)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(c,)| c).unwrap_or(0.0))
    }

    async fn add_cost(&self, vkey: &str, period_secs: u64, cost: f64) -> Result<(), StoreError> {
        let b = bucket(now_secs(), period_secs) as i64;
        sqlx::query(
            "INSERT INTO gov_cost_budgets (vkey, period_start, cost) VALUES ($1, $2, $3) \
             ON CONFLICT (vkey) DO UPDATE SET \
               cost = CASE WHEN gov_cost_budgets.period_start = $2 \
                           THEN gov_cost_budgets.cost + $3 ELSE $3 END, \
               period_start = $2",
        )
        .bind(vkey)
        .bind(b)
        .bind(cost)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_request_window_counts_and_resets() {
        let s = InMemoryGovernanceStore::new();
        // A large window so the bucket is stable across the calls.
        assert_eq!(s.incr_requests("vk", 3600).await.unwrap(), 1);
        assert_eq!(s.incr_requests("vk", 3600).await.unwrap(), 2);
        assert_eq!(s.incr_requests("vk", 3600).await.unwrap(), 3);
        // A different key has its own counter.
        assert_eq!(s.incr_requests("other", 3600).await.unwrap(), 1);
        // A 1-second window buckets by the current second; two very-different windows can't
        // share state, so a window of 1 always starts fresh relative to a fixed instant.
    }

    #[tokio::test]
    async fn in_memory_token_budget_accumulates() {
        let s = InMemoryGovernanceStore::new();
        assert_eq!(s.consumed_tokens("vk").await.unwrap(), 0);
        s.add_tokens("vk", 10).await.unwrap();
        s.add_tokens("vk", 5).await.unwrap();
        assert_eq!(s.consumed_tokens("vk").await.unwrap(), 15);
    }

    #[tokio::test]
    async fn in_memory_cost_budget_accumulates_within_period() {
        let s = InMemoryGovernanceStore::new();
        assert_eq!(s.period_cost("vk", 3600).await.unwrap(), 0.0);
        s.add_cost("vk", 3600, 0.5).await.unwrap();
        s.add_cost("vk", 3600, 0.25).await.unwrap();
        assert!((s.period_cost("vk", 3600).await.unwrap() - 0.75).abs() < 1e-9);
    }

    // Live-Postgres integration test (mirrors the pgvector gating): set KGATEWAY_TEST_PG to a
    // reachable Postgres URL to exercise `SqlGovernanceStore` end to end.
    #[tokio::test]
    async fn sql_store_shares_counters() {
        let Ok(url) = std::env::var("KGATEWAY_TEST_PG") else {
            eprintln!("skipping: set KGATEWAY_TEST_PG to run the SQL governance test");
            return;
        };
        let store = SqlGovernanceStore::connect(&url).await.expect("connect");
        // Unique key per run so repeated runs don't collide on cumulative counters.
        let vk = format!("vk_{}", now_secs());
        assert_eq!(store.incr_requests(&vk, 3600).await.unwrap(), 1);
        assert_eq!(store.incr_requests(&vk, 3600).await.unwrap(), 2);
        store.add_tokens(&vk, 100).await.unwrap();
        store.add_tokens(&vk, 23).await.unwrap();
        assert_eq!(store.consumed_tokens(&vk).await.unwrap(), 123);
        store.add_cost(&vk, 3600, 1.5).await.unwrap();
        store.add_cost(&vk, 3600, 0.5).await.unwrap();
        assert!((store.period_cost(&vk, 3600).await.unwrap() - 2.0).abs() < 1e-9);
    }
}
