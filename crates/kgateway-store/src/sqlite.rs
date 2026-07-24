//! SQLite-backed [`LogStore`] implementation using `sqlx`'s runtime query API.
//!
//! We deliberately use `sqlx::query` / `sqlx::query_as` (runtime-checked) rather
//! than the compile-time `query!` macros so the crate builds offline without a
//! live database.

use async_trait::async_trait;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::FromRow;

use crate::{
    filter_where, histogram_from_values, sort_sessions, FilterBind, FilterData, Histogram,
    HistogramMetric, LogFilter, LogPage, LogQuery, LogStats, LogStore, PlaceholderStyle, Rank,
    RankDimension, RankMetric, RequestLog, SessionPage, SessionSort, SessionSummary, SortBy,
    StoreError, TimePoint,
};

/// Replay a [`FilterBind`] list onto a `sqlx` query builder in order. Works for any builder
/// with `.bind()` (`query_as`, `query_scalar`, …); bound values are owned so the query stays
/// `'static` in its arguments.
macro_rules! bind_filter {
    ($q:expr, $binds:expr) => {{
        let mut q = $q;
        for b in $binds {
            q = match b {
                FilterBind::Text(s) => q.bind(s.clone()),
                FilterBind::Int(i) => q.bind(*i),
                FilterBind::Bool(x) => q.bind(*x),
            };
        }
        q
    }};
}

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
    session_id        TEXT,
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
    spans             TEXT,
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
    "ALTER TABLE request_logs ADD COLUMN spans TEXT",
    "ALTER TABLE request_logs ADD COLUMN session_id TEXT",
];

/// Secondary indexes backing the pushed-down filter/sort/aggregate queries. Without these
/// the SQL overrides would full-scan `request_logs`; with them the common filters (provider,
/// model, status, virtual_key, session_id) and the default newest-first sort are index-served.
/// All use `IF NOT EXISTS`, so they are idempotent on every connect.
const CREATE_INDEX_DDL: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS idx_logs_created_at  ON request_logs (created_at DESC)",
    "CREATE INDEX IF NOT EXISTS idx_logs_provider    ON request_logs (provider)",
    "CREATE INDEX IF NOT EXISTS idx_logs_model       ON request_logs (model)",
    "CREATE INDEX IF NOT EXISTS idx_logs_virtual_key ON request_logs (virtual_key)",
    "CREATE INDEX IF NOT EXISTS idx_logs_status      ON request_logs (status)",
    "CREATE INDEX IF NOT EXISTS idx_logs_session_id  ON request_logs (session_id)",
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

        // Indexes are `IF NOT EXISTS`, so failures here are real (propagate them).
        for stmt in CREATE_INDEX_DDL {
            sqlx::query(stmt).execute(&pool).await?;
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
    session_id: Option<String>,
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
    spans: Option<String>,
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
            stream: r.stream != 0,
            cache_hit: r.cache_hit != 0,
            stop_reason: r.stop_reason,
            error_message: r.error_message,
            request_body: r.request_body,
            response_body: r.response_body,
            spans: r.spans,
            redacted: r.redacted != 0,
            redaction_mapping: r.redaction_mapping,
        }
    }
}

/// Column list for the lean list/`recent` query. Body columns are selected as typed NULL
/// literals so large captured payloads are never read on the hot list path.
const LIST_COLUMNS: &str =
    "request_id, created_at, virtual_key, session_id, provider, model, status, \
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
const REVEAL_COLUMNS: &str =
    "request_id, created_at, virtual_key, session_id, provider, model, status, \
     prompt_tokens, completion_tokens, latency_ms, cost, stream, cache_hit, stop_reason, \
     error_message, request_body, response_body, spans, redacted, redaction_mapping";

const INSERT_SQL: &str = "INSERT INTO request_logs \
     (request_id, created_at, virtual_key, session_id, provider, model, status, prompt_tokens, \
      completion_tokens, latency_ms, cost, stream, cache_hit, stop_reason, error_message, \
      request_body, response_body, spans, redacted, redaction_mapping) \
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

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
        .bind(log.session_id)
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
        .bind(log.spans)
        .bind(log.redacted as i64)
        .bind(log.redaction_mapping)
}

/// Row for the aggregate `stats` query. SQLite returns `COUNT`/`SUM` as INTEGER (i64) and
/// `AVG`/`SUM(REAL)` as REAL (f64).
#[derive(FromRow)]
struct StatsRow {
    total: i64,
    success: i64,
    errors: i64,
    avg_latency_ms: f64,
    total_tokens: i64,
    total_cost: f64,
    cache_hits: i64,
}

/// Row for the bucketed `timeseries` query.
#[derive(FromRow)]
struct TimeRow {
    bucket_ts: i64,
    count: i64,
    errors: i64,
}

/// Row for the grouped `rankings` query.
#[derive(FromRow)]
struct RankRow {
    key: String,
    count: i64,
    cost: f64,
    tokens: i64,
    errors: i64,
}

/// Row for the per-session aggregate query (scalar fields only).
#[derive(FromRow)]
struct SessionAggRow {
    session_id: String,
    first_ts: i64,
    last_ts: i64,
    call_count: i64,
    total_tokens: i64,
    total_cost: f64,
    error_count: i64,
    cache_hits: i64,
}

/// Row for the second session query that fills in distinct providers/models + latest vkey.
#[derive(FromRow)]
struct SessionDetailRow {
    session_id: String,
    provider: String,
    model: String,
    virtual_key: Option<String>,
    created_at: i64,
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

    // ---- Pushed-down analytics: filter/sort/aggregate in SQL, no 10k scan ceiling ----

    async fn query(&self, q: &LogQuery) -> Result<LogPage, StoreError> {
        let (frag, binds, _) = filter_where(&q.filter, PlaceholderStyle::Question, 1);
        let sort_col = match q.sort_by {
            SortBy::CreatedAt => "created_at",
            SortBy::Latency => "latency_ms",
            SortBy::Tokens => "(prompt_tokens + completion_tokens)",
            // COALESCE mirrors the Rust sort's `cost.unwrap_or(0.0)`.
            SortBy::Cost => "COALESCE(cost, 0.0)",
        };
        let dir = if q.descending { "DESC" } else { "ASC" };
        // `id DESC` tiebreak keeps equal-key rows in a stable, newest-first order so
        // pagination doesn't shuffle between pages.
        let sql = format!(
            "SELECT {LIST_COLUMNS} FROM request_logs WHERE 1=1{frag} \
             ORDER BY {sort_col} {dir}, id DESC LIMIT ? OFFSET ?"
        );
        let rows = bind_filter!(sqlx::query_as::<_, RequestLogRow>(&sql), &binds)
            .bind(q.limit as i64)
            .bind(q.offset as i64)
            .fetch_all(&self.pool)
            .await?;
        let logs = rows.into_iter().map(RequestLog::from).collect();

        let count_sql = format!("SELECT COUNT(*) FROM request_logs WHERE 1=1{frag}");
        let total: i64 = bind_filter!(sqlx::query_scalar::<_, i64>(&count_sql), &binds)
            .fetch_one(&self.pool)
            .await?;
        Ok(LogPage {
            logs,
            total: total as usize,
        })
    }

    async fn stats(&self, filter: &LogFilter) -> Result<LogStats, StoreError> {
        let (frag, binds, _) = filter_where(filter, PlaceholderStyle::Question, 1);
        // FILTER / success-error split mirrors `LogStats::from_logs` exactly:
        // success == status in 200..300, everything else is an error.
        let sql = format!(
            "SELECT COUNT(*) AS total, \
             COUNT(*) FILTER (WHERE status >= 200 AND status < 300) AS success, \
             COUNT(*) FILTER (WHERE status < 200 OR status >= 300) AS errors, \
             COALESCE(AVG(latency_ms), 0.0) AS avg_latency_ms, \
             COALESCE(SUM(prompt_tokens + completion_tokens), 0) AS total_tokens, \
             COALESCE(SUM(cost), 0.0) AS total_cost, \
             COUNT(*) FILTER (WHERE cache_hit) AS cache_hits \
             FROM request_logs WHERE 1=1{frag}"
        );
        let r = bind_filter!(sqlx::query_as::<_, StatsRow>(&sql), &binds)
            .fetch_one(&self.pool)
            .await?;
        Ok(LogStats {
            total: r.total as u64,
            success: r.success as u64,
            errors: r.errors as u64,
            avg_latency_ms: r.avg_latency_ms,
            total_tokens: r.total_tokens as u64,
            total_cost: r.total_cost,
            cache_hits: r.cache_hits as u64,
        })
    }

    async fn histogram(
        &self,
        filter: &LogFilter,
        metric: HistogramMetric,
        buckets: usize,
    ) -> Result<Histogram, StoreError> {
        let (frag, binds, _) = filter_where(filter, PlaceholderStyle::Question, 1);
        // Push the filter down and fetch only the metric column, then bin in Rust with the
        // shared helper (identical output to the in-memory path).
        let expr = match metric {
            HistogramMetric::Latency => "CAST(latency_ms AS REAL)",
            HistogramMetric::Cost => "COALESCE(cost, 0.0)",
            HistogramMetric::Tokens => "CAST(prompt_tokens + completion_tokens AS REAL)",
        };
        let sql = format!("SELECT {expr} AS v FROM request_logs WHERE 1=1{frag}");
        let vals: Vec<f64> = bind_filter!(sqlx::query_scalar::<_, f64>(&sql), &binds)
            .fetch_all(&self.pool)
            .await?;
        Ok(histogram_from_values(metric.name(), vals, buckets))
    }

    async fn timeseries(
        &self,
        filter: &LogFilter,
        bucket_ms: i64,
    ) -> Result<Vec<TimePoint>, StoreError> {
        let bucket_ms = bucket_ms.max(1);
        let (frag, binds, _) = filter_where(filter, PlaceholderStyle::Question, 1);
        // Integer division floors for positive `created_at` (always true for unix ms),
        // matching the Rust `div_euclid`. The two `?` for the bucket come first in SQL text,
        // so bind them before the filter binds.
        let sql = format!(
            "SELECT (created_at / ?) * ? AS bucket_ts, COUNT(*) AS count, \
             COUNT(*) FILTER (WHERE status < 200 OR status >= 300) AS errors \
             FROM request_logs WHERE 1=1{frag} \
             GROUP BY bucket_ts ORDER BY bucket_ts ASC"
        );
        let rows = bind_filter!(
            sqlx::query_as::<_, TimeRow>(&sql)
                .bind(bucket_ms)
                .bind(bucket_ms),
            &binds
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| TimePoint {
                ts: r.bucket_ts,
                count: r.count as u64,
                errors: r.errors as u64,
            })
            .collect())
    }

    async fn rankings(
        &self,
        filter: &LogFilter,
        dimension: RankDimension,
        metric: RankMetric,
        limit: usize,
    ) -> Result<Vec<Rank>, StoreError> {
        let (frag, binds, _) = filter_where(filter, PlaceholderStyle::Question, 1);
        let dim = match dimension {
            RankDimension::Model => "model",
            RankDimension::Provider => "provider",
            RankDimension::VirtualKey => "COALESCE(virtual_key, '(none)')",
        };
        // `score` is an output alias; tiebreak by key ascending like `compute_rankings`.
        let score = match metric {
            RankMetric::Count => "count",
            RankMetric::Cost => "cost",
            RankMetric::Tokens => "tokens",
            RankMetric::Errors => "errors",
        };
        let sql = format!(
            "SELECT {dim} AS key, COUNT(*) AS count, COALESCE(SUM(cost), 0.0) AS cost, \
             COALESCE(SUM(prompt_tokens + completion_tokens), 0) AS tokens, \
             COUNT(*) FILTER (WHERE status < 200 OR status >= 300) AS errors \
             FROM request_logs WHERE 1=1{frag} \
             GROUP BY {dim} ORDER BY {score} DESC, key ASC LIMIT ?"
        );
        let rows = bind_filter!(sqlx::query_as::<_, RankRow>(&sql), &binds)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| Rank {
                key: r.key,
                count: r.count as u64,
                cost: r.cost,
                tokens: r.tokens as u64,
                errors: r.errors as u64,
            })
            .collect())
    }

    async fn filter_values(&self) -> Result<FilterData, StoreError> {
        let providers: Vec<String> = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT provider FROM request_logs WHERE provider IS NOT NULL ORDER BY provider",
        )
        .fetch_all(&self.pool)
        .await?;
        let models: Vec<String> = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT model FROM request_logs WHERE model IS NOT NULL ORDER BY model",
        )
        .fetch_all(&self.pool)
        .await?;
        let virtual_keys: Vec<String> = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT virtual_key FROM request_logs WHERE virtual_key IS NOT NULL ORDER BY virtual_key",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(FilterData {
            providers,
            models,
            virtual_keys,
        })
    }

    async fn sessions(
        &self,
        filter: &LogFilter,
        sort: SessionSort,
        limit: usize,
        offset: usize,
    ) -> Result<SessionPage, StoreError> {
        let (frag, binds, _) = filter_where(filter, PlaceholderStyle::Question, 1);
        // Aggregate every matching session (no cap), then sort + paginate in Rust so `total`
        // is the true session count — the whole point of the push-down.
        let agg_sql = format!(
            "SELECT session_id AS session_id, MIN(created_at) AS first_ts, \
             MAX(created_at) AS last_ts, COUNT(*) AS call_count, \
             COALESCE(SUM(prompt_tokens + completion_tokens), 0) AS total_tokens, \
             COALESCE(SUM(cost), 0.0) AS total_cost, \
             COUNT(*) FILTER (WHERE status < 200 OR status >= 300) AS error_count, \
             COUNT(*) FILTER (WHERE cache_hit) AS cache_hits \
             FROM request_logs WHERE 1=1{frag} AND session_id IS NOT NULL \
             GROUP BY session_id"
        );
        let agg = bind_filter!(sqlx::query_as::<_, SessionAggRow>(&agg_sql), &binds)
            .fetch_all(&self.pool)
            .await?;
        let mut sessions: Vec<SessionSummary> = agg
            .into_iter()
            .map(|a| SessionSummary {
                session_id: a.session_id,
                first_ts: a.first_ts,
                last_ts: a.last_ts,
                call_count: a.call_count as u64,
                total_tokens: a.total_tokens as u64,
                total_cost: a.total_cost,
                error_count: a.error_count as u64,
                cache_hits: a.cache_hits as u64,
                providers: Vec::new(),
                models: Vec::new(),
                virtual_key: None,
            })
            .collect();
        sort_sessions(&mut sessions, sort);
        let total = sessions.len();
        let mut page: Vec<SessionSummary> = sessions.into_iter().skip(offset).take(limit).collect();

        // Second query, scoped to just this page's sessions: distinct providers/models and
        // the latest virtual key, folded like `compute_sessions`.
        if !page.is_empty() {
            let ids: Vec<String> = page.iter().map(|s| s.session_id.clone()).collect();
            let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let detail_sql = format!(
                "SELECT session_id, provider, model, virtual_key, created_at \
                 FROM request_logs WHERE 1=1{frag} AND session_id IN ({placeholders}) \
                 ORDER BY created_at ASC"
            );
            let mut q = bind_filter!(sqlx::query_as::<_, SessionDetailRow>(&detail_sql), &binds);
            for id in &ids {
                q = q.bind(id.clone());
            }
            let rows = q.fetch_all(&self.pool).await?;

            use std::collections::{BTreeSet, HashMap};
            struct Extra {
                providers: BTreeSet<String>,
                models: BTreeSet<String>,
                vk: Option<String>,
                vk_at: i64,
            }
            let mut map: HashMap<String, Extra> = HashMap::new();
            for r in rows {
                let e = map.entry(r.session_id.clone()).or_insert_with(|| Extra {
                    providers: BTreeSet::new(),
                    models: BTreeSet::new(),
                    vk: None,
                    vk_at: i64::MIN,
                });
                e.providers.insert(r.provider);
                e.models.insert(r.model);
                // Most recent row wins the virtual key (even if that key is None), matching
                // `compute_sessions`.
                if r.created_at >= e.vk_at {
                    e.vk_at = r.created_at;
                    e.vk = r.virtual_key;
                }
            }
            for s in &mut page {
                if let Some(e) = map.remove(&s.session_id) {
                    s.providers = e.providers.into_iter().collect();
                    s.models = e.models.into_iter().collect();
                    s.virtual_key = e.vk;
                }
            }
        }

        Ok(SessionPage {
            sessions: page,
            total,
        })
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

    // ---- Push-down tests: prove the 10k scan ceiling is gone and SQL aggregates match ----

    use crate::{HistogramMetric, LogFilter, LogQuery, RankDimension, RankMetric, SessionSort};

    /// Minimal log builder for the analytics tests (full control over the aggregated fields).
    #[allow(clippy::too_many_arguments)]
    fn mk(
        id: &str,
        created_at: i64,
        provider: &str,
        model: &str,
        status: u16,
        prompt: u32,
        completion: u32,
        cost: Option<f64>,
        cache_hit: bool,
        session_id: Option<&str>,
    ) -> RequestLog {
        RequestLog {
            request_id: id.to_string(),
            created_at,
            virtual_key: Some("vk".to_string()),
            session_id: session_id.map(|s| s.to_string()),
            provider: provider.to_string(),
            model: model.to_string(),
            status,
            prompt_tokens: prompt,
            completion_tokens: completion,
            latency_ms: 100,
            cost,
            stream: false,
            cache_hit,
            stop_reason: None,
            error_message: None,
            request_body: None,
            response_body: None,
            spans: None,
            redacted: false,
            redaction_mapping: None,
        }
    }

    #[tokio::test]
    async fn query_returns_beyond_10k_with_real_count() {
        let store = SqliteLogStore::connect("sqlite::memory:")
            .await
            .expect("connect");
        // 10_500 rows — 500 past the old DEFAULT_SCAN_LIMIT window.
        let n = 10_500i64;
        let batch: Vec<RequestLog> = (0..n)
            .map(|i| {
                mk(
                    &format!("r{i}"),
                    i, // created_at ascending with i
                    "openai",
                    "gpt-4o",
                    200,
                    1,
                    1,
                    Some(0.001),
                    false,
                    None,
                )
            })
            .collect();
        store.append_batch(batch).await.expect("seed");

        // Default sort is created_at DESC. Skip past row 10_000 to reach rows the old
        // in-memory window could never return.
        let page = store
            .query(&LogQuery {
                offset: 10_495,
                limit: 10,
                ..Default::default()
            })
            .await
            .expect("query");
        // total reflects the FULL table, not a 10k cap.
        assert_eq!(page.total, 10_500);
        // Exactly the 5 oldest rows remain after the offset — reachable only via SQL push-down.
        assert_eq!(page.logs.len(), 5);
        let min_created = page.logs.iter().map(|l| l.created_at).min().unwrap();
        assert_eq!(min_created, 0, "the very oldest row is reachable past 10k");
    }

    #[tokio::test]
    async fn stats_count_beyond_10k_and_split_success_errors() {
        let store = SqliteLogStore::connect("sqlite::memory:")
            .await
            .expect("connect");
        let n = 10_500i64;
        let batch: Vec<RequestLog> = (0..n)
            .map(|i| {
                // Every 10th row is a 500 error; each row 2 prompt + 3 completion tokens.
                let status = if i % 10 == 0 { 500 } else { 200 };
                mk(
                    &format!("r{i}"),
                    i,
                    "openai",
                    "gpt-4o",
                    status,
                    2,
                    3,
                    Some(0.01),
                    i % 4 == 0,
                    None,
                )
            })
            .collect();
        store.append_batch(batch).await.expect("seed");

        let s = store.stats(&LogFilter::default()).await.expect("stats");
        assert_eq!(s.total, 10_500, "counts every row, not just 10k");
        let errors = (0..n).filter(|i| i % 10 == 0).count() as u64;
        assert_eq!(s.errors, errors);
        assert_eq!(s.success, 10_500 - errors);
        assert_eq!(s.total_tokens, 10_500 * 5);
        assert_eq!(s.cache_hits, (0..n).filter(|i| i % 4 == 0).count() as u64);
    }

    #[tokio::test]
    async fn filter_pushdown_scopes_count_and_rows() {
        let store = SqliteLogStore::connect("sqlite::memory:")
            .await
            .expect("connect");
        let mut batch = Vec::new();
        for i in 0..50 {
            batch.push(mk(
                &format!("o{i}"),
                i,
                "openai",
                "gpt-4o",
                200,
                1,
                1,
                None,
                false,
                None,
            ));
        }
        for i in 0..50 {
            batch.push(mk(
                &format!("a{i}"),
                1000 + i,
                "anthropic",
                "claude",
                200,
                1,
                1,
                None,
                false,
                None,
            ));
        }
        store.append_batch(batch).await.expect("seed");

        let page = store
            .query(&LogQuery {
                filter: LogFilter {
                    provider: Some("openai".to_string()),
                    ..Default::default()
                },
                limit: 1000,
                ..Default::default()
            })
            .await
            .expect("query");
        assert_eq!(page.total, 50);
        assert!(page.logs.iter().all(|l| l.provider == "openai"));
    }

    #[tokio::test]
    async fn rankings_group_and_order_by_metric() {
        let store = SqliteLogStore::connect("sqlite::memory:")
            .await
            .expect("connect");
        // gpt-4o: 3 calls; claude: 1 call; gemini: 2 calls.
        let mut batch = Vec::new();
        let spec = [("gpt-4o", 3), ("claude", 1), ("gemini", 2)];
        let mut i = 0;
        for (model, count) in spec {
            for _ in 0..count {
                batch.push(mk(
                    &format!("r{i}"),
                    i,
                    "openai",
                    model,
                    200,
                    1,
                    1,
                    Some(0.01),
                    false,
                    None,
                ));
                i += 1;
            }
        }
        store.append_batch(batch).await.expect("seed");

        let ranks = store
            .rankings(
                &LogFilter::default(),
                RankDimension::Model,
                RankMetric::Count,
                10,
            )
            .await
            .expect("rankings");
        let got: Vec<(String, u64)> = ranks.iter().map(|r| (r.key.clone(), r.count)).collect();
        assert_eq!(
            got,
            vec![
                ("gpt-4o".to_string(), 3),
                ("gemini".to_string(), 2),
                ("claude".to_string(), 1),
            ]
        );
    }

    #[tokio::test]
    async fn timeseries_buckets_in_sql() {
        let store = SqliteLogStore::connect("sqlite::memory:")
            .await
            .expect("connect");
        // Two rows in bucket [0,1000): ts 0 and 500; two in [1000,2000): ts 1000 and 1500.
        // One of the first bucket's rows is a 500 error.
        let batch = vec![
            mk("a", 0, "openai", "m", 500, 1, 1, None, false, None),
            mk("b", 500, "openai", "m", 200, 1, 1, None, false, None),
            mk("c", 1000, "openai", "m", 200, 1, 1, None, false, None),
            mk("d", 1500, "openai", "m", 200, 1, 1, None, false, None),
        ];
        store.append_batch(batch).await.expect("seed");

        let ts = store
            .timeseries(&LogFilter::default(), 1000)
            .await
            .expect("timeseries");
        assert_eq!(ts.len(), 2);
        assert_eq!((ts[0].ts, ts[0].count, ts[0].errors), (0, 2, 1));
        assert_eq!((ts[1].ts, ts[1].count, ts[1].errors), (1000, 2, 0));
    }

    #[tokio::test]
    async fn histogram_pushdown_matches_values() {
        let store = SqliteLogStore::connect("sqlite::memory:")
            .await
            .expect("connect");
        // Latencies are all 100 in `mk`, so a single-value histogram: one bucket, total = n.
        let batch: Vec<RequestLog> = (0..20)
            .map(|i| {
                mk(
                    &format!("r{i}"),
                    i,
                    "openai",
                    "m",
                    200,
                    1,
                    1,
                    None,
                    false,
                    None,
                )
            })
            .collect();
        store.append_batch(batch).await.expect("seed");

        let h = store
            .histogram(&LogFilter::default(), HistogramMetric::Latency, 10)
            .await
            .expect("histogram");
        assert_eq!(h.total, 20);
        assert_eq!(
            h.buckets.len(),
            1,
            "all-equal values collapse to one bucket"
        );
        assert_eq!(h.buckets[0].count, 20);
    }

    #[tokio::test]
    async fn sessions_aggregate_in_sql() {
        let store = SqliteLogStore::connect("sqlite::memory:")
            .await
            .expect("connect");
        // Session "A": 3 calls across 2 models/2 providers, 1 error, 1 cache hit.
        // Session "B": 1 call. A null-session row is excluded entirely.
        let batch = vec![
            mk(
                "a1",
                10,
                "openai",
                "gpt-4o",
                200,
                10,
                5,
                Some(0.02),
                false,
                Some("A"),
            ),
            mk(
                "a2",
                20,
                "openai",
                "gpt-4o",
                500,
                1,
                0,
                Some(0.00),
                true,
                Some("A"),
            ),
            mk(
                "a3",
                30,
                "anthropic",
                "claude",
                200,
                4,
                6,
                Some(0.03),
                false,
                Some("A"),
            ),
            mk(
                "b1",
                40,
                "openai",
                "gpt-4o",
                200,
                1,
                1,
                Some(0.01),
                false,
                Some("B"),
            ),
            mk(
                "n1",
                50,
                "openai",
                "gpt-4o",
                200,
                1,
                1,
                Some(0.01),
                false,
                None,
            ),
        ];
        store.append_batch(batch).await.expect("seed");

        let page = store
            .sessions(&LogFilter::default(), SessionSort::LastActivity, 50, 0)
            .await
            .expect("sessions");
        assert_eq!(page.total, 2, "null-session row is excluded");
        // LastActivity DESC → B (last_ts 40) before A (last_ts 30).
        assert_eq!(page.sessions[0].session_id, "B");
        let a = page
            .sessions
            .iter()
            .find(|s| s.session_id == "A")
            .expect("session A");
        assert_eq!(a.call_count, 3);
        assert_eq!(a.first_ts, 10);
        assert_eq!(a.last_ts, 30);
        // a1: 10+5, a2: 1+0, a3: 4+6.
        assert_eq!(a.total_tokens, 10 + 5 + 1 + 4 + 6);
        assert!((a.total_cost - 0.05).abs() < 1e-9);
        assert_eq!(a.error_count, 1);
        assert_eq!(a.cache_hits, 1);
        assert_eq!(
            a.providers,
            vec!["anthropic".to_string(), "openai".to_string()]
        );
        assert_eq!(a.models, vec!["claude".to_string(), "gpt-4o".to_string()]);
        // Latest row (a3) carried vk "vk".
        assert_eq!(a.virtual_key.as_deref(), Some("vk"));
    }

    #[tokio::test]
    async fn filter_values_are_distinct_from_sql() {
        let store = SqliteLogStore::connect("sqlite::memory:")
            .await
            .expect("connect");
        let batch = vec![
            mk("r1", 1, "openai", "gpt-4o", 200, 1, 1, None, false, None),
            mk(
                "r2",
                2,
                "openai",
                "gpt-4o-mini",
                200,
                1,
                1,
                None,
                false,
                None,
            ),
            mk("r3", 3, "anthropic", "claude", 200, 1, 1, None, false, None),
        ];
        store.append_batch(batch).await.expect("seed");

        let fv = store.filter_values().await.expect("filter_values");
        assert_eq!(
            fv.providers,
            vec!["anthropic".to_string(), "openai".to_string()]
        );
        assert_eq!(
            fv.models,
            vec![
                "claude".to_string(),
                "gpt-4o".to_string(),
                "gpt-4o-mini".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn session_id_roundtrips_through_the_sql_column() {
        let store = SqliteLogStore::connect("sqlite::memory:")
            .await
            .expect("connect");

        let mut with = sample("req-sess", 10);
        with.session_id = Some("sess-abc".to_string());
        store.append(with).await.expect("append with session");
        store
            .append(sample("req-none", 10))
            .await
            .expect("append without session");

        let got = store.recent(10).await.expect("recent");
        let by_id = |id: &str| got.iter().find(|l| l.request_id == id).unwrap();
        // Written into (and read back out of) the real `session_id` column, not some other.
        assert_eq!(by_id("req-sess").session_id.as_deref(), Some("sess-abc"));
        assert_eq!(by_id("req-none").session_id, None);
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

    #[tokio::test]
    async fn spans_round_trip_on_detail_but_are_stripped_from_list() {
        // Traces follow the captured-body contract: a list of 200 rows must not drag
        // every row's waterfall along with it.
        let store = SqliteLogStore::connect("sqlite::memory:")
            .await
            .expect("connect");
        let trace = r#"[{"name":"attempt · zai","category":"failed","start_us":128000,"dur_us":440000,"depth":1,"outcome":"429"}]"#;
        let mut log = sample("req-trace", 5);
        log.spans = Some(trace.to_string());
        store.append(log).await.expect("append");

        let listed = store.recent(10).await.expect("recent");
        assert!(
            listed[0].spans.is_none(),
            "spans must not ride along on the list path"
        );

        let got = store.get("req-trace").await.expect("get").expect("some");
        assert_eq!(
            got.spans.as_deref(),
            Some(trace),
            "detail read returns the trace verbatim"
        );
    }
}
