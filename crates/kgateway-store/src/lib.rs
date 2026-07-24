//! kgateway-store — persistence behind store traits. Default impl: SQLite; Postgres and
//! an in-memory store behind the same traits. See `docs/06-deployment.md`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

mod sqlite;
pub use sqlite::SqliteLogStore;

mod postgres;
pub use postgres::PostgresLogStore;

mod vector;
pub use vector::{cosine_similarity, InMemoryVectorStore, VectorHit, VectorStore};

mod pgvector;
pub use pgvector::PgVectorStore;

mod governance_store;
pub use governance_store::{GovernanceStore, InMemoryGovernanceStore, SqlGovernanceStore};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found")]
    NotFound,
    #[error("backend error: {0}")]
    Backend(String),
}

/// A persisted request log entry (control-plane / dashboard). M10 expanded this from a
/// 7-field metrics row to a richer audit record (timestamp, virtual key, cost, stream,
/// cache hit, stop reason, error detail). Request/response *content* is Phase 2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestLog {
    pub request_id: String,
    /// Event time, unix epoch milliseconds.
    pub created_at: i64,
    /// The virtual key that made the request, if any.
    pub virtual_key: Option<String>,
    /// Client-supplied session identifier used to group a working session's many calls
    /// into one journey. Resolved at ingress (`x-session-id` header, or the OpenAI `user`
    /// / Anthropic `metadata.user_id` body hint). `None` when the caller sent no hint.
    /// It's an opaque grouping label, not content — safe to expose and store by default.
    #[serde(default)]
    pub session_id: Option<String>,
    pub provider: String,
    pub model: String,
    pub status: u16,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub latency_ms: u64,
    /// Estimated cost in USD (from a static per-model pricing table), if known.
    pub cost: Option<f64>,
    pub stream: bool,
    pub cache_hit: bool,
    pub stop_reason: Option<String>,
    /// Upstream/gateway error detail (server-side audit only; clients get a scrubbed msg).
    pub error_message: Option<String>,
    /// Captured request payload (truncated JSON) — M10 Phase 2 content capture. Populated
    /// only when content capture is enabled AND only on `get`/detail reads; list queries
    /// (`recent`/`query`) leave this `None` to stay lean. Admin-only on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_body: Option<String>,
    /// Captured response payload (truncated JSON). Same population rules as `request_body`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
    /// Trace spans as a JSON array — the per-stage timings behind the dashboard's call
    /// waterfall (see `kgateway_core::trace`). Same population rules as `request_body`:
    /// detail reads only, so list/live-tail payloads stay lean. Unlike captured bodies
    /// these hold no request content — only stage names, timings, and outcomes — so they
    /// are recorded by default rather than opt-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spans: Option<String>,
    /// Whether the captured bodies had secrets/PII redacted (M11). Safe to expose — it's a
    /// flag, not content — so the UI can show a "redacted" badge and a reveal affordance.
    #[serde(default)]
    pub redacted: bool,
    /// Encrypted (base64) reversible redaction mapping. **Never** serialized to any client
    /// (`skip_serializing`); loaded on detail reads and consumed only by the `logs:reveal`
    /// endpoint to un-redact. `None` in list results and when nothing was redacted / no key.
    #[serde(skip_serializing, default)]
    pub redaction_mapping: Option<String>,
}

impl RequestLog {
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens as u64 + self.completion_tokens as u64
    }
}

/// Filter for querying logs. All fields are AND-ed; `None` = no constraint.
#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub status: Option<u16>,
    pub virtual_key: Option<String>,
    /// Exact session id (groups a session's calls). `None` = no constraint.
    pub session_id: Option<String>,
    /// created_at >= this (unix ms).
    pub since_ms: Option<i64>,
    /// Only cache hits (Some(true)) / only misses (Some(false)).
    pub cache_hit: Option<bool>,
    /// Case-insensitive substring over request_id / provider / model.
    pub search: Option<String>,
}

impl LogFilter {
    pub fn matches(&self, l: &RequestLog) -> bool {
        if let Some(p) = &self.provider {
            if &l.provider != p {
                return false;
            }
        }
        if let Some(m) = &self.model {
            if &l.model != m {
                return false;
            }
        }
        if let Some(s) = self.status {
            if l.status != s {
                return false;
            }
        }
        if let Some(vk) = &self.virtual_key {
            if l.virtual_key.as_deref() != Some(vk.as_str()) {
                return false;
            }
        }
        if let Some(sid) = &self.session_id {
            if l.session_id.as_deref() != Some(sid.as_str()) {
                return false;
            }
        }
        if let Some(since) = self.since_ms {
            if l.created_at < since {
                return false;
            }
        }
        if let Some(ch) = self.cache_hit {
            if l.cache_hit != ch {
                return false;
            }
        }
        if let Some(q) = &self.search {
            let q = q.to_lowercase();
            let hay = format!("{} {} {}", l.request_id, l.provider, l.model).to_lowercase();
            if !hay.contains(&q) {
                return false;
            }
        }
        true
    }
}

/// SQL placeholder dialect for [`filter_where`]: Postgres numbers params `$1, $2, …`;
/// SQLite uses positional `?`.
#[derive(Clone, Copy)]
pub(crate) enum PlaceholderStyle {
    Dollar,
    Question,
}

impl PlaceholderStyle {
    fn at(self, n: usize) -> String {
        match self {
            PlaceholderStyle::Dollar => format!("${n}"),
            PlaceholderStyle::Question => "?".to_string(),
        }
    }
}

/// One bound value for a pushed-down filter, kept dialect-neutral so both DB backends share
/// [`filter_where`]. The caller replays these onto its `sqlx` query in order.
pub(crate) enum FilterBind {
    Text(String),
    Int(i64),
    Bool(bool),
}

/// Build a `WHERE`-clause fragment (each condition led by ` AND `, so it appends after a
/// `WHERE 1=1` seed) plus the ordered bind list for a [`LogFilter`]. `start_at` is the first
/// placeholder number (Postgres `$N`; ignored for SQLite `?`). Returns the fragment, the
/// binds, and the next free placeholder number so the caller can keep numbering its own
/// params (LIMIT/OFFSET, bucket size, …). This is the SQL twin of [`LogFilter::matches`],
/// which stays behind for `MemoryLogStore`.
pub(crate) fn filter_where(
    filter: &LogFilter,
    style: PlaceholderStyle,
    start_at: usize,
) -> (String, Vec<FilterBind>, usize) {
    let mut frag = String::new();
    let mut binds: Vec<FilterBind> = Vec::new();
    let mut n = start_at;

    if let Some(v) = &filter.provider {
        frag.push_str(&format!(" AND provider = {}", style.at(n)));
        n += 1;
        binds.push(FilterBind::Text(v.clone()));
    }
    if let Some(v) = &filter.model {
        frag.push_str(&format!(" AND model = {}", style.at(n)));
        n += 1;
        binds.push(FilterBind::Text(v.clone()));
    }
    if let Some(s) = filter.status {
        frag.push_str(&format!(" AND status = {}", style.at(n)));
        n += 1;
        binds.push(FilterBind::Int(s as i64));
    }
    if let Some(v) = &filter.virtual_key {
        frag.push_str(&format!(" AND virtual_key = {}", style.at(n)));
        n += 1;
        binds.push(FilterBind::Text(v.clone()));
    }
    if let Some(v) = &filter.session_id {
        frag.push_str(&format!(" AND session_id = {}", style.at(n)));
        n += 1;
        binds.push(FilterBind::Text(v.clone()));
    }
    if let Some(since) = filter.since_ms {
        frag.push_str(&format!(" AND created_at >= {}", style.at(n)));
        n += 1;
        binds.push(FilterBind::Int(since));
    }
    if let Some(ch) = filter.cache_hit {
        frag.push_str(&format!(" AND cache_hit = {}", style.at(n)));
        n += 1;
        binds.push(FilterBind::Bool(ch));
    }
    if let Some(q) = &filter.search {
        // Case-insensitive substring over request_id / provider / model — the SQL twin of the
        // `format!("{} {} {}").to_lowercase().contains()` check in `matches`. The term is
        // bound three times, once per column.
        let (a, b, c) = (style.at(n), style.at(n + 1), style.at(n + 2));
        match style {
            PlaceholderStyle::Dollar => frag.push_str(&format!(
                " AND (request_id ILIKE '%' || {a} || '%' OR provider ILIKE '%' || {b} || '%' \
                 OR model ILIKE '%' || {c} || '%')"
            )),
            PlaceholderStyle::Question => frag.push_str(&format!(
                " AND (LOWER(request_id) LIKE '%' || LOWER({a}) || '%' \
                 OR LOWER(provider) LIKE '%' || LOWER({b}) || '%' \
                 OR LOWER(model) LIKE '%' || LOWER({c}) || '%')"
            )),
        }
        n += 3;
        binds.push(FilterBind::Text(q.clone()));
        binds.push(FilterBind::Text(q.clone()));
        binds.push(FilterBind::Text(q.clone()));
    }

    (frag, binds, n)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    #[default]
    CreatedAt,
    Latency,
    Tokens,
    Cost,
}

/// A query: filter + sort + pagination.
#[derive(Debug, Clone)]
pub struct LogQuery {
    pub filter: LogFilter,
    pub limit: usize,
    pub offset: usize,
    pub sort_by: SortBy,
    pub descending: bool,
}

impl Default for LogQuery {
    fn default() -> Self {
        Self {
            filter: LogFilter::default(),
            limit: 50,
            offset: 0,
            sort_by: SortBy::CreatedAt,
            descending: true,
        }
    }
}

impl LogQuery {
    fn sort(&self, logs: &mut [RequestLog]) {
        logs.sort_by(|a, b| {
            let ord = match self.sort_by {
                SortBy::CreatedAt => a.created_at.cmp(&b.created_at),
                SortBy::Latency => a.latency_ms.cmp(&b.latency_ms),
                SortBy::Tokens => a.total_tokens().cmp(&b.total_tokens()),
                SortBy::Cost => a
                    .cost
                    .unwrap_or(0.0)
                    .partial_cmp(&b.cost.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal),
            };
            if self.descending {
                ord.reverse()
            } else {
                ord
            }
        });
    }
}

/// A page of results plus the total count matching the filter (for pagination).
#[derive(Debug, Clone, Serialize)]
pub struct LogPage {
    pub logs: Vec<RequestLog>,
    pub total: usize,
}

/// Aggregate stats over a filtered set.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LogStats {
    pub total: u64,
    pub success: u64,
    pub errors: u64,
    pub avg_latency_ms: f64,
    pub total_tokens: u64,
    pub total_cost: f64,
    pub cache_hits: u64,
}

impl LogStats {
    pub fn from_logs(logs: &[RequestLog]) -> Self {
        let total = logs.len() as u64;
        let mut s = LogStats {
            total,
            ..Default::default()
        };
        let mut latency_sum = 0u64;
        for l in logs {
            if (200..300).contains(&l.status) {
                s.success += 1;
            } else {
                s.errors += 1;
            }
            latency_sum += l.latency_ms;
            s.total_tokens += l.total_tokens();
            s.total_cost += l.cost.unwrap_or(0.0);
            if l.cache_hit {
                s.cache_hits += 1;
            }
        }
        s.avg_latency_ms = if total > 0 {
            latency_sum as f64 / total as f64
        } else {
            0.0
        };
        s
    }
}

// ---- M12 analytics: histograms, timeseries, rankings, filter-data ----

/// Which scalar a histogram bins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistogramMetric {
    Latency,
    Cost,
    Tokens,
}

impl HistogramMetric {
    pub(crate) fn name(self) -> &'static str {
        match self {
            HistogramMetric::Latency => "latency",
            HistogramMetric::Cost => "cost",
            HistogramMetric::Tokens => "tokens",
        }
    }
    fn value(self, l: &RequestLog) -> f64 {
        match self {
            HistogramMetric::Latency => l.latency_ms as f64,
            HistogramMetric::Cost => l.cost.unwrap_or(0.0),
            HistogramMetric::Tokens => l.total_tokens() as f64,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Bucket {
    pub lo: f64,
    pub hi: f64,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Histogram {
    pub metric: String,
    pub buckets: Vec<Bucket>,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimePoint {
    /// Bucket start, unix ms.
    pub ts: i64,
    pub count: u64,
    pub errors: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankDimension {
    Model,
    Provider,
    VirtualKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankMetric {
    Count,
    Cost,
    Tokens,
    Errors,
}

#[derive(Debug, Clone, Serialize)]
pub struct Rank {
    pub key: String,
    pub count: u64,
    pub cost: f64,
    pub tokens: u64,
    pub errors: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct FilterData {
    pub providers: Vec<String>,
    pub models: Vec<String>,
    pub virtual_keys: Vec<String>,
}

fn is_error(status: u16) -> bool {
    !(200..300).contains(&status)
}

/// Bin a metric into `buckets` linear buckets between the observed min/max.
fn compute_histogram(logs: &[RequestLog], metric: HistogramMetric, buckets: usize) -> Histogram {
    let vals: Vec<f64> = logs.iter().map(|l| metric.value(l)).collect();
    histogram_from_values(metric.name(), vals, buckets)
}

/// Bin pre-extracted metric values into `buckets` linear buckets between the observed
/// min/max. Split out of [`compute_histogram`] so the DB backends can push the filter into
/// SQL, fetch only the scalar column, and reuse the identical binning — no behavior change
/// for the in-memory path.
pub(crate) fn histogram_from_values(name: &str, vals: Vec<f64>, buckets: usize) -> Histogram {
    let buckets = buckets.clamp(1, 100);
    let total = vals.len() as u64;
    let name = name.to_string();
    if vals.is_empty() {
        return Histogram {
            metric: name,
            buckets: Vec::new(),
            total: 0,
        };
    }
    let min = vals.iter().copied().fold(f64::INFINITY, f64::min);
    let max = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if (max - min).abs() < f64::EPSILON {
        // All values equal → a single bucket holding everything.
        return Histogram {
            metric: name,
            buckets: vec![Bucket {
                lo: min,
                hi: max,
                count: total,
            }],
            total,
        };
    }
    let width = (max - min) / buckets as f64;
    let mut counts = vec![0u64; buckets];
    for v in &vals {
        let mut idx = ((v - min) / width) as usize;
        if idx >= buckets {
            idx = buckets - 1; // the max value lands in the last bucket
        }
        counts[idx] += 1;
    }
    let bs = (0..buckets)
        .map(|i| Bucket {
            lo: min + width * i as f64,
            hi: min + width * (i + 1) as f64,
            count: counts[i],
        })
        .collect();
    Histogram {
        metric: name,
        buckets: bs,
        total,
    }
}

/// Bucket logs by `created_at` into fixed `bucket_ms` windows (ascending by time).
fn compute_timeseries(logs: &[RequestLog], bucket_ms: i64) -> Vec<TimePoint> {
    let bucket_ms = bucket_ms.max(1);
    let mut map: std::collections::BTreeMap<i64, (u64, u64)> = std::collections::BTreeMap::new();
    for l in logs {
        let ts = l.created_at.div_euclid(bucket_ms) * bucket_ms;
        let e = map.entry(ts).or_default();
        e.0 += 1;
        if is_error(l.status) {
            e.1 += 1;
        }
    }
    map.into_iter()
        .map(|(ts, (count, errors))| TimePoint { ts, count, errors })
        .collect()
}

/// Top-N groups by a dimension, sorted descending by the chosen metric.
fn compute_rankings(
    logs: &[RequestLog],
    dimension: RankDimension,
    metric: RankMetric,
    limit: usize,
) -> Vec<Rank> {
    let mut map: std::collections::HashMap<String, Rank> = std::collections::HashMap::new();
    for l in logs {
        let key = match dimension {
            RankDimension::Model => l.model.clone(),
            RankDimension::Provider => l.provider.clone(),
            RankDimension::VirtualKey => l.virtual_key.clone().unwrap_or_else(|| "(none)".into()),
        };
        let r = map.entry(key.clone()).or_insert_with(|| Rank {
            key,
            count: 0,
            cost: 0.0,
            tokens: 0,
            errors: 0,
        });
        r.count += 1;
        r.cost += l.cost.unwrap_or(0.0);
        r.tokens += l.total_tokens();
        if is_error(l.status) {
            r.errors += 1;
        }
    }
    let score = |r: &Rank| -> f64 {
        match metric {
            RankMetric::Count => r.count as f64,
            RankMetric::Cost => r.cost,
            RankMetric::Tokens => r.tokens as f64,
            RankMetric::Errors => r.errors as f64,
        }
    };
    let mut v: Vec<Rank> = map.into_values().collect();
    v.sort_by(|a, b| {
        score(b)
            .partial_cmp(&score(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            // Stable tiebreak by key so output is deterministic.
            .then_with(|| a.key.cmp(&b.key))
    });
    v.truncate(limit);
    v
}

/// Distinct provider / model / virtual-key values present in the logs (sorted).
fn compute_filter_values(logs: &[RequestLog]) -> FilterData {
    use std::collections::BTreeSet;
    let mut providers = BTreeSet::new();
    let mut models = BTreeSet::new();
    let mut vkeys = BTreeSet::new();
    for l in logs {
        providers.insert(l.provider.clone());
        models.insert(l.model.clone());
        if let Some(vk) = &l.virtual_key {
            vkeys.insert(vk.clone());
        }
    }
    FilterData {
        providers: providers.into_iter().collect(),
        models: models.into_iter().collect(),
        virtual_keys: vkeys.into_iter().collect(),
    }
}

/// Aggregate view of one session — a group of calls sharing a `session_id`. Powers the
/// dashboard's Sessions list and the per-session journey header. Computed over the recent
/// scan window (see [`DEFAULT_SCAN_LIMIT`]), like the other analytics.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    /// First and last call time in the session (unix ms) — the journey's span.
    pub first_ts: i64,
    pub last_ts: i64,
    pub call_count: u64,
    pub total_tokens: u64,
    pub total_cost: f64,
    pub error_count: u64,
    pub cache_hits: u64,
    /// Distinct providers and models the session touched (sorted).
    pub providers: Vec<String>,
    pub models: Vec<String>,
    /// The most recently seen virtual key for the session (callers usually reuse one).
    pub virtual_key: Option<String>,
}

/// How the session list is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionSort {
    /// Most recently active first (default).
    #[default]
    LastActivity,
    /// Highest estimated cost first.
    Cost,
    /// Most tokens first.
    Tokens,
    /// Most calls first.
    Calls,
}

/// A page of session summaries plus the pre-pagination total.
#[derive(Debug, Clone, Serialize)]
pub struct SessionPage {
    pub sessions: Vec<SessionSummary>,
    pub total: usize,
}

/// Group logs by `session_id` into per-session aggregates. Logs with no session id are
/// skipped (they aren't part of any journey).
fn compute_sessions(logs: &[RequestLog]) -> Vec<SessionSummary> {
    use std::collections::BTreeSet;
    struct Acc {
        first_ts: i64,
        last_ts: i64,
        call_count: u64,
        total_tokens: u64,
        total_cost: f64,
        error_count: u64,
        cache_hits: u64,
        providers: BTreeSet<String>,
        models: BTreeSet<String>,
        virtual_key: Option<String>,
        vk_at: i64,
    }
    let mut map: std::collections::HashMap<String, Acc> = std::collections::HashMap::new();
    for l in logs {
        let Some(sid) = &l.session_id else { continue };
        let a = map.entry(sid.clone()).or_insert_with(|| Acc {
            first_ts: l.created_at,
            last_ts: l.created_at,
            call_count: 0,
            total_tokens: 0,
            total_cost: 0.0,
            error_count: 0,
            cache_hits: 0,
            providers: BTreeSet::new(),
            models: BTreeSet::new(),
            virtual_key: None,
            vk_at: i64::MIN,
        });
        a.first_ts = a.first_ts.min(l.created_at);
        a.last_ts = a.last_ts.max(l.created_at);
        a.call_count += 1;
        a.total_tokens += l.total_tokens();
        a.total_cost += l.cost.unwrap_or(0.0);
        if is_error(l.status) {
            a.error_count += 1;
        }
        if l.cache_hit {
            a.cache_hits += 1;
        }
        a.providers.insert(l.provider.clone());
        a.models.insert(l.model.clone());
        // Keep the most recently seen virtual key.
        if l.created_at >= a.vk_at {
            a.vk_at = l.created_at;
            a.virtual_key = l.virtual_key.clone();
        }
    }
    map.into_iter()
        .map(|(session_id, a)| SessionSummary {
            session_id,
            first_ts: a.first_ts,
            last_ts: a.last_ts,
            call_count: a.call_count,
            total_tokens: a.total_tokens,
            total_cost: a.total_cost,
            error_count: a.error_count,
            cache_hits: a.cache_hits,
            providers: a.providers.into_iter().collect(),
            models: a.models.into_iter().collect(),
            virtual_key: a.virtual_key,
        })
        .collect()
}

/// Sort session summaries in place by `sort` (descending), tiebroken by session id for
/// deterministic output.
fn sort_sessions(sessions: &mut [SessionSummary], sort: SessionSort) {
    let score = |s: &SessionSummary| -> f64 {
        match sort {
            SessionSort::LastActivity => s.last_ts as f64,
            SessionSort::Cost => s.total_cost,
            SessionSort::Tokens => s.total_tokens as f64,
            SessionSort::Calls => s.call_count as f64,
        }
    };
    sessions.sort_by(|a, b| {
        score(b)
            .partial_cmp(&score(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
}

/// Max rows a default (non-pushed-down) query scans. Backends that implement `query`
/// with real SQL should override to avoid this bound.
const DEFAULT_SCAN_LIMIT: usize = 10_000;

/// Request-log store. `append` + `recent` are the primitives; `query`/`get`/`stats` have
/// default impls (fetch-recent + filter in Rust) so every backend works out of the box —
/// SQLite/Postgres can override them to push filters/pagination into SQL.
#[async_trait]
pub trait LogStore: Send + Sync {
    async fn append(&self, log: RequestLog) -> Result<(), StoreError>;

    /// Append many logs at once. The default loops `append`; DB backends override with a
    /// single transaction. Used by the async batch writer. On a per-row error it does NOT
    /// stop early — it attempts every log so one bad row can't silently discard the rest —
    /// and surfaces the last error to the caller.
    async fn append_batch(&self, logs: Vec<RequestLog>) -> Result<(), StoreError> {
        let mut last_err = None;
        for log in logs {
            if let Err(e) = self.append(log).await {
                last_err = Some(e);
            }
        }
        match last_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Recent logs, newest first. Implementations MUST NOT populate `request_body`/
    /// `response_body`/`spans` here — list/live-tail paths stay lean; content and traces
    /// are fetched only by [`LogStore::get`].
    async fn recent(&self, limit: usize) -> Result<Vec<RequestLog>, StoreError>;

    /// Filtered, sorted, paginated query. The default scans the recent window and filters in
    /// Rust (fine for the in-memory store); the SQLite/Postgres backends override this to push
    /// the filter, sort, and pagination into SQL — and, crucially, return the *true* filtered
    /// `total` rather than one capped at [`DEFAULT_SCAN_LIMIT`].
    async fn query(&self, q: &LogQuery) -> Result<LogPage, StoreError> {
        let mut all = self.recent(DEFAULT_SCAN_LIMIT).await?;
        all.retain(|l| q.filter.matches(l));
        let total = all.len();
        q.sort(&mut all);
        let logs = all.into_iter().skip(q.offset).take(q.limit).collect();
        Ok(LogPage { logs, total })
    }

    /// A single log by request id. Implementations MUST NOT populate `redaction_mapping`
    /// here — the encrypted mapping is loaded only by [`LogStore::get_with_mapping`], used
    /// by the `logs:reveal` endpoint. (`request_body`/`response_body`/`spans` ARE loaded
    /// here.)
    async fn get(&self, request_id: &str) -> Result<Option<RequestLog>, StoreError> {
        Ok(self
            .recent(DEFAULT_SCAN_LIMIT)
            .await?
            .into_iter()
            .find(|l| l.request_id == request_id))
    }

    /// Like [`LogStore::get`] but ALSO loads the encrypted `redaction_mapping`. Only the
    /// reveal path calls this, so the ciphertext never loads on ordinary detail views —
    /// defense-in-depth against a future change that removes the `skip_serializing` guard.
    /// The default falls back to `get` (no mapping); DB backends override it.
    async fn get_with_mapping(&self, request_id: &str) -> Result<Option<RequestLog>, StoreError> {
        self.get(request_id).await
    }

    /// Aggregate stats over the filtered set.
    async fn stats(&self, filter: &LogFilter) -> Result<LogStats, StoreError> {
        let logs: Vec<RequestLog> = self
            .recent(DEFAULT_SCAN_LIMIT)
            .await?
            .into_iter()
            .filter(|l| filter.matches(l))
            .collect();
        Ok(LogStats::from_logs(&logs))
    }

    /// Delete logs whose `created_at` is strictly older than `cutoff_ms` (unix ms) and
    /// return how many were removed. Backs the retention job. The default is a no-op
    /// (returns 0) for stores that don't support pruning; the built-in stores override it.
    async fn purge_older_than(&self, _cutoff_ms: i64) -> Result<u64, StoreError> {
        Ok(0)
    }

    // ---- M12 analytics (default impls: scan + filter + fold in Rust) ----

    /// Distribution of a metric over the filtered set.
    async fn histogram(
        &self,
        filter: &LogFilter,
        metric: HistogramMetric,
        buckets: usize,
    ) -> Result<Histogram, StoreError> {
        let logs = self.scan_filtered(filter).await?;
        Ok(compute_histogram(&logs, metric, buckets))
    }

    /// Requests/errors bucketed over time (`created_at`).
    async fn timeseries(
        &self,
        filter: &LogFilter,
        bucket_ms: i64,
    ) -> Result<Vec<TimePoint>, StoreError> {
        let logs = self.scan_filtered(filter).await?;
        Ok(compute_timeseries(&logs, bucket_ms))
    }

    /// Top-N groups by dimension, scored by metric.
    async fn rankings(
        &self,
        filter: &LogFilter,
        dimension: RankDimension,
        metric: RankMetric,
        limit: usize,
    ) -> Result<Vec<Rank>, StoreError> {
        let logs = self.scan_filtered(filter).await?;
        Ok(compute_rankings(&logs, dimension, metric, limit))
    }

    /// Distinct filter values for the UI dropdowns.
    async fn filter_values(&self) -> Result<FilterData, StoreError> {
        let logs = self.recent(DEFAULT_SCAN_LIMIT).await?;
        Ok(compute_filter_values(&logs))
    }

    /// Sessions (grouped calls) matching `filter`, ordered by `sort`, paginated. Computed
    /// over the recent scan window in Rust, like the other analytics — a session that
    /// scrolled out of the window is not resurrected here.
    async fn sessions(
        &self,
        filter: &LogFilter,
        sort: SessionSort,
        limit: usize,
        offset: usize,
    ) -> Result<SessionPage, StoreError> {
        let logs = self.scan_filtered(filter).await?;
        let mut sessions = compute_sessions(&logs);
        sort_sessions(&mut sessions, sort);
        let total = sessions.len();
        let sessions = sessions.into_iter().skip(offset).take(limit).collect();
        Ok(SessionPage { sessions, total })
    }

    /// Shared helper: fetch the recent window and apply a filter. (Bodies are already
    /// stripped by `recent`, which is fine — analytics only reads scalars.)
    async fn scan_filtered(&self, filter: &LogFilter) -> Result<Vec<RequestLog>, StoreError> {
        Ok(self
            .recent(DEFAULT_SCAN_LIMIT)
            .await?
            .into_iter()
            .filter(|l| filter.matches(l))
            .collect())
    }
}

/// In-memory log store — default when no `database` is configured. Useful for tests/dev.
#[derive(Default)]
pub struct MemoryLogStore {
    inner: std::sync::Mutex<Vec<RequestLog>>,
}

#[async_trait]
impl LogStore for MemoryLogStore {
    async fn append(&self, log: RequestLog) -> Result<(), StoreError> {
        self.inner.lock().unwrap().push(log);
        Ok(())
    }
    async fn recent(&self, limit: usize) -> Result<Vec<RequestLog>, StoreError> {
        let g = self.inner.lock().unwrap();
        // Strip bodies + spans + mapping from list results to match the DB backends'
        // lean-list contract (the `redacted` flag is kept — it's a safe badge).
        Ok(g.iter()
            .rev()
            .take(limit)
            .map(|l| RequestLog {
                request_body: None,
                response_body: None,
                spans: None,
                redaction_mapping: None,
                ..l.clone()
            })
            .collect())
    }
    async fn get(&self, request_id: &str) -> Result<Option<RequestLog>, StoreError> {
        let g = self.inner.lock().unwrap();
        // Detail read: bodies included, but strip the encrypted mapping (reveal-only).
        Ok(g.iter()
            .rev()
            .find(|l| l.request_id == request_id)
            .map(|l| RequestLog {
                redaction_mapping: None,
                ..l.clone()
            }))
    }
    async fn get_with_mapping(&self, request_id: &str) -> Result<Option<RequestLog>, StoreError> {
        let g = self.inner.lock().unwrap();
        Ok(g.iter().rev().find(|l| l.request_id == request_id).cloned())
    }
    async fn purge_older_than(&self, cutoff_ms: i64) -> Result<u64, StoreError> {
        let mut g = self.inner.lock().unwrap();
        let before = g.len();
        g.retain(|l| l.created_at >= cutoff_ms);
        Ok((before - g.len()) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store whose `append` fails for one specific request_id — to prove the default
    /// `append_batch` doesn't discard the rest of the batch after an error.
    struct FlakyStore {
        fail_on: String,
        appended: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl LogStore for FlakyStore {
        async fn append(&self, log: RequestLog) -> Result<(), StoreError> {
            if log.request_id == self.fail_on {
                return Err(StoreError::Backend("boom".into()));
            }
            self.appended.lock().unwrap().push(log.request_id);
            Ok(())
        }
        async fn recent(&self, _limit: usize) -> Result<Vec<RequestLog>, StoreError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn default_append_batch_continues_past_errors_and_surfaces_one() {
        let store = FlakyStore {
            fail_on: "b".into(),
            appended: std::sync::Mutex::new(Vec::new()),
        };
        let batch = vec![log_at("a", 1), log_at("b", 2), log_at("c", 3)];

        let res = store.append_batch(batch).await;
        assert!(res.is_err(), "a per-row failure must surface as an error");

        // "a" and "c" persisted despite "b" failing — no silent tail loss.
        let done = store.appended.lock().unwrap().clone();
        assert_eq!(done, vec!["a".to_string(), "c".to_string()]);
    }

    fn log_at(id: &str, created_at: i64) -> RequestLog {
        RequestLog {
            request_id: id.to_string(),
            created_at,
            virtual_key: None,
            session_id: None,
            provider: "openai".into(),
            model: "gpt-4o".into(),
            status: 200,
            prompt_tokens: 0,
            completion_tokens: 0,
            latency_ms: 0,
            cost: None,
            stream: false,
            cache_hit: false,
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
    async fn purge_removes_only_older_than_cutoff() {
        let store = MemoryLogStore::default();
        store.append(log_at("old", 1_000)).await.unwrap();
        store.append(log_at("edge", 2_000)).await.unwrap();
        store.append(log_at("new", 3_000)).await.unwrap();

        // Cutoff 2000: "old" (<2000) goes; "edge" (==2000) and "new" stay.
        let removed = store.purge_older_than(2_000).await.unwrap();
        assert_eq!(removed, 1);

        let ids: Vec<_> = store
            .recent(10)
            .await
            .unwrap()
            .into_iter()
            .map(|l| l.request_id)
            .collect();
        assert_eq!(ids, vec!["new", "edge"]);
    }

    // ---- M12 analytics compute functions ----

    fn with_latency(id: &str, latency_ms: u64) -> RequestLog {
        RequestLog {
            latency_ms,
            ..log_at(id, 0)
        }
    }

    #[test]
    fn histogram_empty_and_single_and_normal() {
        // Empty.
        let h = compute_histogram(&[], HistogramMetric::Cost, 5);
        assert_eq!(h.total, 0);
        assert!(h.buckets.is_empty());

        // All equal → one bucket.
        let equal: Vec<_> = (0..3).map(|i| with_latency(&format!("e{i}"), 5)).collect();
        let h = compute_histogram(&equal, HistogramMetric::Latency, 10);
        assert_eq!(h.buckets.len(), 1);
        assert_eq!(h.buckets[0].count, 3);

        // Spread: counts sum to total, max value in last bucket.
        let spread: Vec<_> = [10u64, 20, 30, 40]
            .iter()
            .enumerate()
            .map(|(i, l)| with_latency(&format!("s{i}"), *l))
            .collect();
        let h = compute_histogram(&spread, HistogramMetric::Latency, 2);
        assert_eq!(h.total, 4);
        assert_eq!(h.buckets.len(), 2);
        assert_eq!(h.buckets.iter().map(|b| b.count).sum::<u64>(), 4);
    }

    #[test]
    fn rankings_sorted_desc_by_metric() {
        let mut logs: Vec<_> = (0..3)
            .map(|i| RequestLog {
                model: "A".into(),
                ..log_at(&format!("a{i}"), 0)
            })
            .collect();
        logs.push(RequestLog {
            model: "B".into(),
            ..log_at("b0", 0)
        });
        let r = compute_rankings(&logs, RankDimension::Model, RankMetric::Count, 10);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].key, "A");
        assert_eq!(r[0].count, 3);
        assert_eq!(r[1].key, "B");
    }

    #[test]
    fn timeseries_buckets_and_counts_errors() {
        let logs = vec![
            log_at("a", 1000),
            log_at("b", 1500),
            RequestLog {
                status: 500,
                ..log_at("c", 2500)
            },
        ];
        let ts = compute_timeseries(&logs, 1000);
        assert_eq!(ts.len(), 2);
        assert_eq!((ts[0].ts, ts[0].count, ts[0].errors), (1000, 2, 0));
        assert_eq!((ts[1].ts, ts[1].count, ts[1].errors), (2000, 1, 1));
    }

    #[test]
    fn filter_values_are_distinct_and_sorted() {
        let logs = vec![
            RequestLog {
                provider: "openai".into(),
                model: "gpt-4o".into(),
                virtual_key: Some("vk1".into()),
                ..log_at("a", 0)
            },
            RequestLog {
                provider: "anthropic".into(),
                model: "claude".into(),
                virtual_key: Some("vk1".into()),
                ..log_at("b", 0)
            },
        ];
        let fd = compute_filter_values(&logs);
        assert_eq!(fd.providers, vec!["anthropic", "openai"]);
        assert_eq!(fd.models, vec!["claude", "gpt-4o"]);
        assert_eq!(fd.virtual_keys, vec!["vk1"]); // distinct
    }

    /// A status-200 log with a session id + chosen provider/model/tokens/cost. Tests that
    /// need a non-200 status or a cache hit override those via struct-update syntax.
    fn sess_log(
        id: &str,
        ts: i64,
        session: Option<&str>,
        provider: &str,
        model: &str,
        tokens: u32,
        cost: f64,
    ) -> RequestLog {
        RequestLog {
            created_at: ts,
            session_id: session.map(str::to_string),
            provider: provider.into(),
            model: model.into(),
            status: 200,
            prompt_tokens: tokens,
            cost: Some(cost),
            ..log_at(id, ts)
        }
    }

    #[test]
    fn compute_sessions_groups_and_aggregates() {
        let logs = vec![
            sess_log("a", 100, Some("s1"), "openai", "gpt-4o", 10, 0.01),
            sess_log("b", 300, Some("s1"), "openai", "gpt-4o-mini", 20, 0.02),
            sess_log("c", 200, Some("s1"), "anthropic", "claude", 5, 0.05),
            sess_log("d", 150, Some("s2"), "openai", "gpt-4o", 7, 0.03),
            // No session id → excluded from grouping entirely.
            sess_log("e", 400, None, "openai", "gpt-4o", 99, 9.9),
        ];
        let mut sessions = compute_sessions(&logs);
        sort_sessions(&mut sessions, SessionSort::LastActivity);

        assert_eq!(
            sessions.len(),
            2,
            "only s1 and s2; the sessionless call is dropped"
        );

        // s1 was last active at ts=300 → sorts first by LastActivity.
        let s1 = &sessions[0];
        assert_eq!(s1.session_id, "s1");
        assert_eq!(s1.call_count, 3);
        assert_eq!(s1.first_ts, 100);
        assert_eq!(s1.last_ts, 300);
        assert_eq!(s1.total_tokens, 35); // 10 + 20 + 5, completion is 0 in the helper
        assert!((s1.total_cost - 0.08).abs() < 1e-9);
        assert_eq!(s1.error_count, 0);
        assert_eq!(s1.providers, vec!["anthropic", "openai"]); // distinct, sorted
        assert_eq!(s1.models, vec!["claude", "gpt-4o", "gpt-4o-mini"]);

        let s2 = &sessions[1];
        assert_eq!(s2.session_id, "s2");
        assert_eq!(s2.call_count, 1);
    }

    #[test]
    fn compute_sessions_counts_errors_and_cache_hits() {
        let logs = vec![
            RequestLog {
                status: 500,
                ..sess_log("a", 1, Some("s1"), "openai", "gpt-4o", 0, 0.0)
            },
            RequestLog {
                cache_hit: true,
                ..sess_log("b", 2, Some("s1"), "openai", "gpt-4o", 0, 0.0)
            },
        ];
        let sessions = compute_sessions(&logs);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].error_count, 1);
        assert_eq!(sessions[0].cache_hits, 1);
    }

    #[test]
    fn sort_sessions_orders_descending_by_metric() {
        let mut sessions = [
            sess_log("a", 10, Some("cheap"), "openai", "m", 1, 0.01),
            sess_log("b", 20, Some("pricey"), "openai", "m", 1, 5.0),
        ]
        .iter()
        .map(|l| compute_sessions(std::slice::from_ref(l)).pop().unwrap())
        .collect::<Vec<_>>();
        sort_sessions(&mut sessions, SessionSort::Cost);
        assert_eq!(sessions[0].session_id, "pricey");
        assert_eq!(sessions[1].session_id, "cheap");
    }

    #[tokio::test]
    async fn store_sessions_filter_and_paginate() {
        let store = MemoryLogStore::default();
        for l in [
            sess_log("a", 100, Some("s1"), "openai", "gpt-4o", 10, 0.01),
            sess_log("b", 200, Some("s1"), "openai", "gpt-4o", 10, 0.01),
            sess_log("c", 300, Some("s2"), "openai", "gpt-4o", 10, 0.01),
        ] {
            store.append(l).await.unwrap();
        }

        // Unfiltered: two sessions.
        let page = store
            .sessions(&LogFilter::default(), SessionSort::LastActivity, 50, 0)
            .await
            .unwrap();
        assert_eq!(page.total, 2);

        // Filter to one session id → the journey's calls come back via `query`.
        let one = LogFilter {
            session_id: Some("s1".into()),
            ..Default::default()
        };
        let calls = store
            .query(&LogQuery {
                filter: one,
                limit: 50,
                offset: 0,
                sort_by: SortBy::CreatedAt,
                descending: false,
            })
            .await
            .unwrap();
        assert_eq!(calls.total, 2);
        assert_eq!(calls.logs[0].request_id, "a"); // oldest first
        assert_eq!(calls.logs[1].request_id, "b");
    }
}
