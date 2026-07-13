//! SQLite-backed [`LogStore`] implementation using `sqlx`'s runtime query API.
//!
//! We deliberately use `sqlx::query` / `sqlx::query_as` (runtime-checked) rather
//! than the compile-time `query!` macros so the crate builds offline without a
//! live database.

use async_trait::async_trait;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::FromRow;

use crate::{LogStore, RequestLog, StoreError};

/// Inline schema migration run on connect. SQLite integer columns are stored as
/// i64, so all numeric `RequestLog` fields live in `INTEGER` columns (bools as
/// 0/1) and are cast on the way in/out; `cost` uses `REAL`. `id` gives us a
/// stable ordering for `recent`.
const CREATE_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS request_logs (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id        TEXT    NOT NULL,
    created_at        INTEGER NOT NULL DEFAULT 0,
    virtual_key       TEXT,
    provider          TEXT    NOT NULL,
    model             TEXT    NOT NULL,
    status            INTEGER NOT NULL,
    prompt_tokens     INTEGER NOT NULL,
    completion_tokens INTEGER NOT NULL,
    latency_ms        INTEGER NOT NULL,
    cost              REAL,
    stream            INTEGER NOT NULL DEFAULT 0,
    cache_hit         INTEGER NOT NULL DEFAULT 0,
    stop_reason       TEXT,
    error_message     TEXT,
    request_body      TEXT,
    response_body     TEXT,
    redacted          INTEGER NOT NULL DEFAULT 0,
    redaction_mapping TEXT
)";

/// Best-effort column additions so a database created by an older KGateway build
/// gains the M10 columns. SQLite has no `ADD COLUMN IF NOT EXISTS`; re-adding an
/// existing column errors, which we swallow (see `connect`).
const MIGRATE_COLUMNS: &[&str] = &[
    "ALTER TABLE request_logs ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE request_logs ADD COLUMN virtual_key TEXT",
    "ALTER TABLE request_logs ADD COLUMN cost REAL",
    "ALTER TABLE request_logs ADD COLUMN stream INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE request_logs ADD COLUMN cache_hit INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE request_logs ADD COLUMN stop_reason TEXT",
    "ALTER TABLE request_logs ADD COLUMN error_message TEXT",
    "ALTER TABLE request_logs ADD COLUMN request_body TEXT",
    "ALTER TABLE request_logs ADD COLUMN response_body TEXT",
    "ALTER TABLE request_logs ADD COLUMN redacted INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE request_logs ADD COLUMN redaction_mapping TEXT",
];

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        StoreError::Backend(e.to_string())
    }
}

/// SQLite-backed append-only log store.
#[derive(Clone)]
pub struct SqliteLogStore {
    pool: SqlitePool,
}

impl SqliteLogStore {
    /// Open a connection pool against `url` and run the inline migration.
    ///
    /// Accepts standard sqlx SQLite URLs, e.g. `sqlite::memory:` or
    /// `sqlite://path.db?mode=rwc`.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await?;

        sqlx::query(CREATE_TABLE_DDL).execute(&pool).await?;

        // Bring pre-M10 tables up to the current shape. Adding a column that
        // already exists is an error we intentionally ignore.
        for stmt in MIGRATE_COLUMNS {
            let _ = sqlx::query(stmt).execute(&pool).await;
        }

        Ok(Self { pool })
    }

    /// Access the underlying pool (useful for advanced callers / tests).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

/// Row shape mirroring `request_logs`. SQLite hands back i64 for INTEGER columns;
/// we convert to the `RequestLog` widths (u16/u32/u64) in `From`.
#[derive(FromRow)]
struct RequestLogRow {
    request_id: String,
    created_at: i64,
    virtual_key: Option<String>,
    provider: String,
    model: String,
    status: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
    latency_ms: i64,
    cost: Option<f64>,
    stream: i64,
    cache_hit: i64,
    stop_reason: Option<String>,
    error_message: Option<String>,
    // Populated only by the detail (`get`) query; list queries select these as NULL.
    request_body: Option<String>,
    response_body: Option<String>,
    redacted: i64,
    // Populated only by the detail query; list selects NULL.
    redaction_mapping: Option<String>,
}

impl From<RequestLogRow> for RequestLog {
    fn from(r: RequestLogRow) -> Self {
        RequestLog {
            request_id: r.request_id,
            created_at: r.created_at,
            virtual_key: r.virtual_key,
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
            stream: r.stream != 0,
            cache_hit: r.cache_hit != 0,
            stop_reason: r.stop_reason,
            error_message: r.error_message,
            request_body: r.request_body,
            response_body: r.response_body,
            redacted: r.redacted != 0,
            redaction_mapping: r.redaction_mapping,
        }
    }
}

/// Column list for the lean list/`recent` query. Body columns are selected as typed NULL
/// literals so large captured payloads are never read on the hot list path.
const LIST_COLUMNS: &str = "request_id, created_at, virtual_key, provider, model, status, \
     prompt_tokens, completion_tokens, latency_ms, cost, stream, cache_hit, stop_reason, \
     error_message, CAST(NULL AS TEXT) AS request_body, CAST(NULL AS TEXT) AS response_body, \
     redacted, CAST(NULL AS TEXT) AS redaction_mapping";

/// Detail column list: captured bodies + `redacted`, but the encrypted mapping is NULLed
/// (loaded only by the reveal query, not ordinary detail reads).
const DETAIL_COLUMNS: &str = "request_id, created_at, virtual_key, provider, model, status, \
     prompt_tokens, completion_tokens, latency_ms, cost, stream, cache_hit, stop_reason, \
     error_message, request_body, response_body, redacted, CAST(NULL AS TEXT) AS redaction_mapping";

/// Reveal column list: everything including the encrypted mapping. Used ONLY by
/// `get_with_mapping` behind the `logs:reveal` gate.
const REVEAL_COLUMNS: &str = "request_id, created_at, virtual_key, provider, model, status, \
     prompt_tokens, completion_tokens, latency_ms, cost, stream, cache_hit, stop_reason, \
     error_message, request_body, response_body, redacted, redaction_mapping";

const INSERT_SQL: &str = "INSERT INTO request_logs \
     (request_id, created_at, virtual_key, provider, model, status, prompt_tokens, \
      completion_tokens, latency_ms, cost, stream, cache_hit, stop_reason, error_message, \
      request_body, response_body, redacted, redaction_mapping) \
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

/// Build the bound INSERT for one log. Shared by `append` (single) and `append_batch`
/// (transaction), so both stay in lock-step with the column list. Bound values are owned,
/// so the query is `'static` and can execute against a pool or a transaction.
fn insert_query(
    log: RequestLog,
) -> sqlx::query::Query<'static, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'static>> {
    // SQLite INTEGER is i64; widen every unsigned field to i64 for binding. u64 -> i64 is
    // reinterpreted bitwise for values > i64::MAX, but read back the same way, so it
    // round-trips exactly.
    sqlx::query(INSERT_SQL)
        .bind(log.request_id)
        .bind(log.created_at)
        .bind(log.virtual_key)
        .bind(log.provider)
        .bind(log.model)
        .bind(log.status as i64)
        .bind(log.prompt_tokens as i64)
        .bind(log.completion_tokens as i64)
        .bind(log.latency_ms as i64)
        .bind(log.cost)
        .bind(log.stream as i64)
        .bind(log.cache_hit as i64)
        .bind(log.stop_reason)
        .bind(log.error_message)
        .bind(log.request_body)
        .bind(log.response_body)
        .bind(log.redacted as i64)
        .bind(log.redaction_mapping)
}

#[async_trait]
impl LogStore for SqliteLogStore {
    async fn append(&self, log: RequestLog) -> Result<(), StoreError> {
        insert_query(log).execute(&self.pool).await?;
        Ok(())
    }

    async fn append_batch(&self, logs: Vec<RequestLog>) -> Result<(), StoreError> {
        if logs.is_empty() {
            return Ok(());
        }
        // One transaction per batch: far fewer fsyncs than N autocommitted inserts, and
        // atomic (the batch commits whole or rolls back whole; the caller logs failures).
        let mut tx = self.pool.begin().await?;
        for log in logs {
            insert_query(log).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn recent(&self, limit: usize) -> Result<Vec<RequestLog>, StoreError> {
        let rows = sqlx::query_as::<_, RequestLogRow>(&format!(
            "SELECT {LIST_COLUMNS} FROM request_logs ORDER BY id DESC LIMIT ?"
        ))
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(RequestLog::from).collect())
    }

    async fn get(&self, request_id: &str) -> Result<Option<RequestLog>, StoreError> {
        let row = sqlx::query_as::<_, RequestLogRow>(&format!(
            "SELECT {DETAIL_COLUMNS} FROM request_logs WHERE request_id = ? \
             ORDER BY id DESC LIMIT 1"
        ))
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(RequestLog::from))
    }

    async fn get_with_mapping(&self, request_id: &str) -> Result<Option<RequestLog>, StoreError> {
        let row = sqlx::query_as::<_, RequestLogRow>(&format!(
            "SELECT {REVEAL_COLUMNS} FROM request_logs WHERE request_id = ? \
             ORDER BY id DESC LIMIT 1"
        ))
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(RequestLog::from))
    }

    async fn purge_older_than(&self, cutoff_ms: i64) -> Result<u64, StoreError> {
        let res = sqlx::query("DELETE FROM request_logs WHERE created_at < ?")
            .bind(cutoff_ms)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }
}

// TODO(M4): Postgres impl — a `PostgresLogStore` behind the same `LogStore`
// trait using `PgPool`. Deferred to keep the default build sqlite-only (no
// libpq/postgres driver pulled in). The schema/DDL and casting approach here
// port directly; Postgres has native i16/i32/i64 but no u64, so `latency_ms`
// would still be stored as i64 and reinterpreted the same way.

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(request_id: &str, latency_ms: u64) -> RequestLog {
        RequestLog {
            request_id: request_id.to_string(),
            created_at: 1_700_000_000_000,
            virtual_key: Some("vk-test".to_string()),
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
            redacted: false,
            redaction_mapping: None,
        }
    }

    #[tokio::test]
    async fn append_and_recent_roundtrip_newest_first() {
        let store = SqliteLogStore::connect("sqlite::memory:")
            .await
            .expect("connect");

        // latency > u32::MAX proves the u64 survives the i64 column round-trip.
        let big_latency: u64 = 5_000_000_000;
        assert!(big_latency > u64::from(u32::MAX));

        store.append(sample("req-1", 42)).await.expect("append 1");
        store
            .append(sample("req-2", big_latency))
            .await
            .expect("append 2");

        let got = store.recent(10).await.expect("recent");
        assert_eq!(got.len(), 2);

        // Newest first.
        assert_eq!(got[0].request_id, "req-2");
        assert_eq!(got[1].request_id, "req-1");

        // All fields round-trip, including the wide u64 latency.
        let newest = &got[0];
        assert_eq!(newest.provider, "openai");
        assert_eq!(newest.model, "gpt-4o");
        assert_eq!(newest.status, 200u16);
        assert_eq!(newest.prompt_tokens, 1234u32);
        assert_eq!(newest.completion_tokens, 5678u32);
        assert_eq!(newest.latency_ms, big_latency);
        // M10 scalar fields round-trip.
        assert_eq!(newest.created_at, 1_700_000_000_000);
        assert_eq!(newest.virtual_key.as_deref(), Some("vk-test"));
        assert_eq!(newest.cost, Some(0.0125));
        assert!(!newest.stream);
        assert!(!newest.cache_hit);
        assert_eq!(newest.stop_reason.as_deref(), Some("stop"));
        assert_eq!(newest.error_message, None);

        assert_eq!(got[1].latency_ms, 42u64);
    }

    #[tokio::test]
    async fn recent_respects_limit() {
        let store = SqliteLogStore::connect("sqlite::memory:")
            .await
            .expect("connect");

        store.append(sample("req-1", 10)).await.expect("append 1");
        store.append(sample("req-2", 20)).await.expect("append 2");

        let got = store.recent(1).await.expect("recent");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].request_id, "req-2");
        assert_eq!(got[0].latency_ms, 20u64);
    }

    #[tokio::test]
    async fn purge_older_than_deletes_by_created_at() {
        let store = SqliteLogStore::connect("sqlite::memory:")
            .await
            .expect("connect");

        let mut old = sample("old", 1);
        old.created_at = 1_000;
        let mut new = sample("new", 1);
        new.created_at = 5_000;
        store.append(old).await.expect("append old");
        store.append(new).await.expect("append new");

        let removed = store.purge_older_than(2_000).await.expect("purge");
        assert_eq!(removed, 1);

        let remaining = store.recent(10).await.expect("recent");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].request_id, "new");
    }

    #[tokio::test]
    async fn append_batch_persists_all_rows() {
        let store = SqliteLogStore::connect("sqlite::memory:")
            .await
            .expect("connect");
        let batch = vec![sample("b1", 1), sample("b2", 2), sample("b3", 3)];
        store.append_batch(batch).await.expect("append_batch");

        let got = store.recent(10).await.expect("recent");
        assert_eq!(got.len(), 3);
        // Empty batch is a no-op, not an error.
        store.append_batch(vec![]).await.expect("empty batch");
    }

    #[tokio::test]
    async fn redaction_mapping_only_via_get_with_mapping() {
        let store = SqliteLogStore::connect("sqlite::memory:")
            .await
            .expect("connect");
        let mut s = sample("m1", 5);
        s.redacted = true;
        s.redaction_mapping = Some("ENCRYPTED_BLOB".to_string());
        store.append(s).await.expect("append");

        // Ordinary detail read: bodies present, `redacted` flag present, mapping stripped.
        let got = store.get("m1").await.expect("get").expect("some");
        assert!(got.request_body.is_some());
        assert!(got.redacted);
        assert!(
            got.redaction_mapping.is_none(),
            "detail must NOT load the encrypted mapping"
        );

        // Reveal-only read: mapping present.
        let full = store
            .get_with_mapping("m1")
            .await
            .expect("get_with_mapping")
            .expect("some");
        assert_eq!(full.redaction_mapping.as_deref(), Some("ENCRYPTED_BLOB"));
    }

    #[tokio::test]
    async fn bodies_returned_by_get_but_stripped_from_list() {
        let store = SqliteLogStore::connect("sqlite::memory:")
            .await
            .expect("connect");
        store.append(sample("req-body", 5)).await.expect("append");

        // List path must NOT include captured bodies.
        let listed = store.recent(10).await.expect("recent");
        assert_eq!(listed.len(), 1);
        assert!(listed[0].request_body.is_none());
        assert!(listed[0].response_body.is_none());

        // Detail path returns the full captured bodies.
        let got = store.get("req-body").await.expect("get").expect("some");
        assert_eq!(
            got.request_body.as_deref(),
            Some(r#"{"messages":[{"role":"user","content":"hi"}]}"#)
        );
        assert_eq!(got.response_body.as_deref(), Some(r#"{"content":"hello"}"#));
    }
}
