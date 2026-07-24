//! Postgres-backed [`LogStore`] implementation using `sqlx`'s runtime query API.
//!
//! This mirrors [`crate::SqliteLogStore`]: we deliberately use `sqlx::query` /
//! `sqlx::query_as` (runtime-checked) rather than the compile-time `query!`
//! macros so the crate builds offline without a live database.
//!
//! Differences from the SQLite flavor:
//! - DDL: `id BIGSERIAL PRIMARY KEY` (vs `INTEGER PRIMARY KEY AUTOINCREMENT`),
//!   and native signed integer widths (`INT` / `BIGINT`) instead of SQLite's
//!   single `INTEGER` affinity.
//! - Placeholders are positional `$1..$N`, not `?`.
//! - Postgres has native `i32`/`i64` but no unsigned types, so the unsigned
//!   `RequestLog` fields are stored as the appropriate signed width (`status`
//!   and the token counts as `INT`/i32, `latency_ms` as `BIGINT`/i64) and cast
//!   back on read. `latency_ms` uses the full 64-bit path so a `u64` beyond
//!   `u32::MAX` round-trips exactly.

use async_trait::async_trait;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::FromRow;

use crate::{LogStore, RequestLog, StoreError};

/// Inline schema migration run on connect. Postgres has native signed integer
/// widths, so numeric `RequestLog` fields map to `INT`/`BIGINT` and are cast on
/// the way in/out. `id` gives us a stable ordering for `recent`.
const CREATE_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS request_logs (
    id                BIGSERIAL PRIMARY KEY,
    request_id        TEXT    NOT NULL,
    created_at        BIGINT  NOT NULL DEFAULT 0,
    virtual_key       TEXT,
    session_id        TEXT,
    provider          TEXT    NOT NULL,
    model             TEXT    NOT NULL,
    status            INT     NOT NULL,
    prompt_tokens     INT     NOT NULL,
    completion_tokens INT     NOT NULL,
    latency_ms        BIGINT  NOT NULL,
    cost              DOUBLE PRECISION,
    stream            BOOLEAN NOT NULL DEFAULT FALSE,
    cache_hit         BOOLEAN NOT NULL DEFAULT FALSE,
    stop_reason       TEXT,
    error_message     TEXT,
    request_body      TEXT,
    response_body     TEXT,
    spans             TEXT,
    redacted          BOOLEAN NOT NULL DEFAULT FALSE,
    redaction_mapping TEXT
)";

/// Bring pre-M10 tables up to the current shape. Postgres supports
/// `ADD COLUMN IF NOT EXISTS`, so these are idempotent and safe to always run.
const MIGRATE_COLUMNS: &[&str] = &[
    "ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS created_at BIGINT NOT NULL DEFAULT 0",
    "ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS virtual_key TEXT",
    "ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS cost DOUBLE PRECISION",
    "ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS stream BOOLEAN NOT NULL DEFAULT FALSE",
    "ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS cache_hit BOOLEAN NOT NULL DEFAULT FALSE",
    "ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS stop_reason TEXT",
    "ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS error_message TEXT",
    "ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS request_body TEXT",
    "ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS response_body TEXT",
    "ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS redacted BOOLEAN NOT NULL DEFAULT FALSE",
    "ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS redaction_mapping TEXT",
    "ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS spans TEXT",
    "ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS session_id TEXT",
];

/// Postgres-backed append-only log store.
#[derive(Clone)]
pub struct PostgresLogStore {
    pool: PgPool,
}

impl PostgresLogStore {
    /// Open a connection pool against `url` and run the inline migration.
    ///
    /// Accepts standard sqlx Postgres URLs, e.g.
    /// `postgres://user:pass@localhost/dbname`.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new().max_connections(5).connect(url).await?;

        sqlx::query(CREATE_TABLE_DDL).execute(&pool).await?;

        for stmt in MIGRATE_COLUMNS {
            sqlx::query(stmt).execute(&pool).await?;
        }

        Ok(Self { pool })
    }

    /// Access the underlying pool (useful for advanced callers / tests).
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// Row shape mirroring `request_logs`. Postgres hands back native signed
/// integers; we convert to the `RequestLog` widths (u16/u32/u64) in `From`.
#[derive(FromRow)]
struct RequestLogRow {
    request_id: String,
    created_at: i64,
    virtual_key: Option<String>,
    session_id: Option<String>,
    provider: String,
    model: String,
    status: i32,
    prompt_tokens: i32,
    completion_tokens: i32,
    latency_ms: i64,
    cost: Option<f64>,
    stream: bool,
    cache_hit: bool,
    stop_reason: Option<String>,
    error_message: Option<String>,
    // Populated only by the detail (`get`) query; list queries select these as NULL.
    request_body: Option<String>,
    response_body: Option<String>,
    spans: Option<String>,
    redacted: bool,
    // Populated only by the detail query; list selects NULL.
    redaction_mapping: Option<String>,
}

impl From<RequestLogRow> for RequestLog {
    fn from(r: RequestLogRow) -> Self {
        RequestLog {
            request_id: r.request_id,
            created_at: r.created_at,
            virtual_key: r.virtual_key,
            session_id: r.session_id,
            provider: r.provider,
            model: r.model,
            // Values were written from u16/u32/u64 originals, so these casts are
            // lossless round-trips. `as` truncation only bites on out-of-range
            // input, which cannot occur for rows this store wrote.
            status: r.status as u16,
            prompt_tokens: r.prompt_tokens as u32,
            completion_tokens: r.completion_tokens as u32,
            latency_ms: r.latency_ms as u64,
            cost: r.cost,
            stream: r.stream,
            cache_hit: r.cache_hit,
            stop_reason: r.stop_reason,
            error_message: r.error_message,
            request_body: r.request_body,
            response_body: r.response_body,
            spans: r.spans,
            redacted: r.redacted,
            redaction_mapping: r.redaction_mapping,
        }
    }
}

/// Column list for the lean list/`recent` query. Body columns are selected as typed NULL
/// literals so large captured payloads are never read on the hot list path.
const LIST_COLUMNS: &str = "request_id, created_at, virtual_key, session_id, provider, model, status, \
     prompt_tokens, completion_tokens, latency_ms, cost, stream, cache_hit, stop_reason, \
     error_message, CAST(NULL AS TEXT) AS request_body, CAST(NULL AS TEXT) AS response_body, \
     CAST(NULL AS TEXT) AS spans, redacted, CAST(NULL AS TEXT) AS redaction_mapping";

/// Detail column list: captured bodies + `redacted`, but the encrypted mapping is NULLed
/// (loaded only by the reveal query, not ordinary detail reads).
const DETAIL_COLUMNS: &str = "request_id, created_at, virtual_key, session_id, provider, model, status, \
     prompt_tokens, completion_tokens, latency_ms, cost, stream, cache_hit, stop_reason, \
     error_message, request_body, response_body, spans, redacted, CAST(NULL AS TEXT) AS redaction_mapping";

/// Reveal column list: everything including the encrypted mapping. Used ONLY by
/// `get_with_mapping` behind the `logs:reveal` gate.
const REVEAL_COLUMNS: &str = "request_id, created_at, virtual_key, session_id, provider, model, status, \
     prompt_tokens, completion_tokens, latency_ms, cost, stream, cache_hit, stop_reason, \
     error_message, request_body, response_body, spans, redacted, redaction_mapping";

const INSERT_SQL: &str = "INSERT INTO request_logs \
     (request_id, created_at, virtual_key, provider, model, status, prompt_tokens, \
      completion_tokens, latency_ms, cost, stream, cache_hit, stop_reason, error_message, \
      request_body, response_body, spans, redacted, redaction_mapping, session_id) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)";

/// Build the bound INSERT for one log. Shared by `append` (single) and `append_batch`
/// (transaction). Bound values are owned, so the query is `'static` and can execute
/// against a pool or a transaction.
fn insert_query(
    log: RequestLog,
) -> sqlx::query::Query<'static, sqlx::Postgres, sqlx::postgres::PgArguments> {
    sqlx::query(INSERT_SQL)
        .bind(log.request_id)
        .bind(log.created_at)
        .bind(log.virtual_key)
        .bind(log.provider)
        .bind(log.model)
        .bind(log.status as i32)
        .bind(log.prompt_tokens as i32)
        .bind(log.completion_tokens as i32)
        .bind(log.latency_ms as i64)
        .bind(log.cost)
        .bind(log.stream)
        .bind(log.cache_hit)
        .bind(log.stop_reason)
        .bind(log.error_message)
        .bind(log.request_body)
        .bind(log.response_body)
        .bind(log.spans)
        .bind(log.redacted)
        .bind(log.redaction_mapping)
        .bind(log.session_id)
}

#[async_trait]
impl LogStore for PostgresLogStore {
    async fn append(&self, log: RequestLog) -> Result<(), StoreError> {
        insert_query(log).execute(&self.pool).await?;
        Ok(())
    }

    async fn append_batch(&self, logs: Vec<RequestLog>) -> Result<(), StoreError> {
        if logs.is_empty() {
            return Ok(());
        }
        // One transaction per batch: atomic, and one round-trip commit instead of N.
        let mut tx = self.pool.begin().await?;
        for log in logs {
            insert_query(log).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn recent(&self, limit: usize) -> Result<Vec<RequestLog>, StoreError> {
        let rows = sqlx::query_as::<_, RequestLogRow>(&format!(
            "SELECT {LIST_COLUMNS} FROM request_logs ORDER BY id DESC LIMIT $1"
        ))
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(RequestLog::from).collect())
    }

    async fn get(&self, request_id: &str) -> Result<Option<RequestLog>, StoreError> {
        let row = sqlx::query_as::<_, RequestLogRow>(&format!(
            "SELECT {DETAIL_COLUMNS} FROM request_logs WHERE request_id = $1 \
             ORDER BY id DESC LIMIT 1"
        ))
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(RequestLog::from))
    }

    async fn get_with_mapping(&self, request_id: &str) -> Result<Option<RequestLog>, StoreError> {
        let row = sqlx::query_as::<_, RequestLogRow>(&format!(
            "SELECT {REVEAL_COLUMNS} FROM request_logs WHERE request_id = $1 \
             ORDER BY id DESC LIMIT 1"
        ))
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(RequestLog::from))
    }

    async fn purge_older_than(&self, cutoff_ms: i64) -> Result<u64, StoreError> {
        let res = sqlx::query("DELETE FROM request_logs WHERE created_at < $1")
            .bind(cutoff_ms)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(request_id: &str, latency_ms: u64) -> RequestLog {
        RequestLog {
            request_id: request_id.to_string(),
            created_at: 1_700_000_000_000,
            virtual_key: Some("vk-test".to_string()),
            session_id: None,
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            status: 200,
            prompt_tokens: 1234,
            completion_tokens: 5678,
            latency_ms,
            cost: Some(0.0125),
            stream: false,
            cache_hit: false,
            stop_reason: Some("stop".to_string()),
            error_message: None,
            request_body: Some(r#"{"messages":[{"role":"user","content":"hi"}]}"#.to_string()),
            response_body: Some(r#"{"content":"hello"}"#.to_string()),
            spans: None,
            redacted: false,
            redaction_mapping: None,
        }
    }

    /// Round-trips two logs through a real Postgres instance.
    ///
    /// Gated on the `KGATEWAY_TEST_PG` env var (a PG connection URL). When it is
    /// unset — as in CI / this environment, where no Postgres is available — the
    /// test returns early and is a clean no-op skip rather than a failure.
    #[tokio::test]
    async fn append_and_recent_roundtrip_newest_first() {
        let Ok(url) = std::env::var("KGATEWAY_TEST_PG") else {
            eprintln!("KGATEWAY_TEST_PG unset; skipping Postgres round-trip test");
            return;
        };

        let store = PostgresLogStore::connect(&url).await.expect("connect");

        // Use a unique request_id prefix so repeated runs against a shared DB
        // don't collide with earlier rows.
        let prefix = format!("pg-test-{}", uuid_like());
        let id1 = format!("{prefix}-1");
        let id2 = format!("{prefix}-2");

        // latency > u32::MAX proves the u64 survives the i64 column round-trip.
        let big_latency: u64 = 5_000_000_000;
        assert!(big_latency > u64::from(u32::MAX));

        store.append(sample(&id1, 42)).await.expect("append 1");
        store
            .append(sample(&id2, big_latency))
            .await
            .expect("append 2");

        // Pull enough rows to include both of ours even on a shared table, then
        // filter to this run's prefix while preserving newest-first ordering.
        let all = store.recent(1000).await.expect("recent");
        let got: Vec<_> = all
            .into_iter()
            .filter(|l| l.request_id.starts_with(&prefix))
            .collect();
        assert_eq!(got.len(), 2);

        // Newest first.
        assert_eq!(got[0].request_id, id2);
        assert_eq!(got[1].request_id, id1);

        // All fields round-trip, including the wide u64 latency.
        let newest = &got[0];
        assert_eq!(newest.provider, "openai");
        assert_eq!(newest.model, "gpt-4o");
        assert_eq!(newest.status, 200u16);
        assert_eq!(newest.prompt_tokens, 1234u32);
        assert_eq!(newest.completion_tokens, 5678u32);
        assert_eq!(newest.latency_ms, big_latency);

        assert_eq!(got[1].latency_ms, 42u64);
    }

    /// Cheap unique-ish suffix so concurrent/repeated runs don't collide. Avoids
    /// pulling `uuid` into this crate just for a test helper.
    fn uuid_like() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }
}
