//! Per-request trace spans — the data behind the dashboard's call waterfall.
//!
//! A span is one stage of a request: a governance check, a cache lookup, one
//! dispatch attempt, a backoff sleep, the upstream call. Each records its offset
//! from the request start and its duration, so the UI can lay them on a shared
//! timeline and show *where* a slow request actually spent its time — including
//! the attempts that failed before the successful one.
//!
//! Spans carry **no request content** — only stage names, timings, and outcome.
//! That is what lets them be recorded unconditionally (unlike content capture,
//! which is opt-in twice because it stores payloads).
//!
//! Timings are microseconds so sub-millisecond stages (a cache hash hit, a
//! governance check) don't all collapse to `0` on the timeline.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// What kind of work a span represents. Drives the colour band in the UI and
/// keeps the vocabulary stable across the engine, plugins, and connectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanCategory {
    /// Gateway-internal work: ingress translation, routing, schema mapping.
    Gateway,
    /// Governance / policy: virtual-key auth, rate limits, budgets.
    Policy,
    /// Semantic-cache lookups (exact hash + vector search).
    Cache,
    /// An outbound network call to a provider.
    Network,
    /// Time spent waiting rather than working: backoff sleeps, semaphore queueing.
    Wait,
    /// A dispatch attempt that failed (rate-limited, dead key, upstream error).
    Failed,
    /// MCP tool execution.
    Tools,
    /// Post-response write-back: redaction, audit-log write.
    Write,
}

/// One recorded stage of a request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Span {
    pub name: String,
    pub category: SpanCategory,
    /// Offset from the request start, microseconds.
    pub start_us: u64,
    /// Duration, microseconds.
    pub dur_us: u64,
    /// Nesting level for the waterfall's indentation (0 = request root).
    pub depth: u8,
    /// Short outcome label shown as a chip — `"429"`, `"hit"`, `"tool_use"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// One-line human explanation shown when the span is selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Collected spans for one request.
///
/// Behind a `Mutex` because most of the pipeline sees `&Ctx`, not `&mut Ctx`
/// (providers and observers are given a shared reference). Contention is nil:
/// pushes are a few per request and never held across an await.
#[derive(Debug, Default)]
pub struct SpanCollector {
    spans: std::sync::Mutex<Vec<Span>>,
}

impl SpanCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a finished span. `start` is the request's `started_at`, `at` when
    /// this stage began, so the offset stays relative to the request root.
    pub fn record(
        &self,
        request_start: Instant,
        at: Instant,
        dur: Duration,
        name: impl Into<String>,
        category: SpanCategory,
        depth: u8,
    ) {
        self.push(Span {
            name: name.into(),
            category,
            start_us: at.saturating_duration_since(request_start).as_micros() as u64,
            dur_us: dur.as_micros() as u64,
            depth,
            outcome: None,
            detail: None,
        });
    }

    /// Record a span with an outcome chip and/or explanatory detail.
    #[allow(clippy::too_many_arguments)]
    pub fn record_detailed(
        &self,
        request_start: Instant,
        at: Instant,
        dur: Duration,
        name: impl Into<String>,
        category: SpanCategory,
        depth: u8,
        outcome: Option<String>,
        detail: Option<String>,
    ) {
        self.push(Span {
            name: name.into(),
            category,
            start_us: at.saturating_duration_since(request_start).as_micros() as u64,
            dur_us: dur.as_micros() as u64,
            depth,
            outcome,
            detail,
        });
    }

    /// Push a pre-built span. A poisoned lock is ignored rather than panicking —
    /// losing a trace row must never fail the request it is describing.
    pub fn push(&self, span: Span) {
        if let Ok(mut v) = self.spans.lock() {
            v.push(span);
        }
    }

    /// Snapshot the spans, ordered by start time so the waterfall renders in
    /// timeline order regardless of completion order (a stage that starts first
    /// but finishes last is still recorded last).
    pub fn snapshot(&self) -> Vec<Span> {
        let mut v = self.spans.lock().map(|g| g.clone()).unwrap_or_default();
        v.sort_by_key(|s| (s.start_us, s.depth));
        v
    }

    pub fn is_empty(&self) -> bool {
        self.spans.lock().map(|v| v.is_empty()).unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_offsets_relative_to_request_start() {
        let c = SpanCollector::new();
        let start = Instant::now();
        let later = start + Duration::from_millis(120);
        c.record(
            start,
            later,
            Duration::from_millis(30),
            "http.ttfb",
            SpanCategory::Network,
            2,
        );
        let spans = c.snapshot();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].start_us, 120_000);
        assert_eq!(spans[0].dur_us, 30_000);
        assert_eq!(spans[0].depth, 2);
        assert_eq!(spans[0].category, SpanCategory::Network);
    }

    #[test]
    fn snapshot_is_ordered_by_start_not_completion() {
        let c = SpanCollector::new();
        let start = Instant::now();
        // A long stage that STARTS first but is recorded last (it finished later).
        c.record(
            start,
            start + Duration::from_millis(50),
            Duration::from_millis(1),
            "second",
            SpanCategory::Gateway,
            1,
        );
        c.record(
            start,
            start + Duration::from_millis(10),
            Duration::from_millis(500),
            "first",
            SpanCategory::Network,
            1,
        );
        let names: Vec<_> = c.snapshot().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["first", "second"]);
    }

    #[test]
    fn sub_millisecond_stages_keep_their_duration() {
        let c = SpanCollector::new();
        let start = Instant::now();
        c.record(
            start,
            start,
            Duration::from_micros(400),
            "governance.check",
            SpanCategory::Policy,
            1,
        );
        assert_eq!(c.snapshot()[0].dur_us, 400, "must not round to 0 ms");
    }

    #[test]
    fn detail_and_outcome_round_trip_through_json() {
        let c = SpanCollector::new();
        let start = Instant::now();
        c.record_detailed(
            start,
            start,
            Duration::from_millis(440),
            "attempt 1 · zai",
            SpanCategory::Failed,
            1,
            Some("429".into()),
            Some("Rate limited upstream.".into()),
        );
        let json = serde_json::to_string(&c.snapshot()).unwrap();
        let back: Vec<Span> = serde_json::from_str(&json).unwrap();
        assert_eq!(back[0].outcome.as_deref(), Some("429"));
        assert_eq!(back[0].detail.as_deref(), Some("Rate limited upstream."));
        assert_eq!(back[0].category, SpanCategory::Failed);
        // Category serializes snake_case for a stable UI contract.
        assert!(json.contains("\"category\":\"failed\""));
    }

    #[test]
    fn empty_collector_serializes_to_empty_list() {
        let c = SpanCollector::new();
        assert!(c.is_empty());
        assert_eq!(serde_json::to_string(&c.snapshot()).unwrap(), "[]");
    }
}
