//! Lightweight Prometheus metrics collected via an axum middleware over every request.
//! Hand-rolled (atomics + a small map) to avoid a metrics-crate dependency at this stage;
//! swappable for the `metrics` facade + OTLP export later.

use crate::app::SharedState;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// axum middleware: time each request and record its status + latency — but ONLY for the
/// LLM data-plane (`/v1/*`). Liveness (`/health`), the metrics scrape (`/metrics`), and
/// the control-plane (`/api/*`, which the dashboard polls on a timer) are excluded so the
/// counters reflect real provider traffic, not health checks or the dashboard watching
/// itself.
pub async fn track_metrics(State(state): State<SharedState>, req: Request, next: Next) -> Response {
    let counted = req.uri().path().starts_with("/v1/");
    let start = Instant::now();
    let resp = next.run(req).await;
    if counted {
        let status = resp.status().as_u16();
        let latency_ms = start.elapsed().as_millis() as u64;
        state.metrics.record(status, latency_ms);
    }
    resp
}

#[derive(Default)]
pub struct Metrics {
    requests_total: AtomicU64,
    latency_ms_sum: AtomicU64,
    by_status: Mutex<BTreeMap<u16, u64>>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one completed request.
    pub fn record(&self, status: u16, latency_ms: u64) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.latency_ms_sum.fetch_add(latency_ms, Ordering::Relaxed);
        if let Ok(mut m) = self.by_status.lock() {
            *m.entry(status).or_insert(0) += 1;
        }
    }

    /// Render the metrics in Prometheus text exposition format.
    pub fn render_prometheus(&self) -> String {
        let total = self.requests_total.load(Ordering::Relaxed);
        let latency_sum = self.latency_ms_sum.load(Ordering::Relaxed);
        let mut out = String::new();

        out.push_str("# HELP kgateway_requests_total LLM data-plane requests (/v1/*) handled.\n");
        out.push_str("# TYPE kgateway_requests_total counter\n");
        out.push_str(&format!("kgateway_requests_total {total}\n"));

        out.push_str("# HELP kgateway_requests_by_status LLM requests by HTTP status code.\n");
        out.push_str("# TYPE kgateway_requests_by_status counter\n");
        if let Ok(m) = self.by_status.lock() {
            for (status, count) in m.iter() {
                out.push_str(&format!(
                    "kgateway_requests_by_status{{status=\"{status}\"}} {count}\n"
                ));
            }
        }

        out.push_str("# HELP kgateway_request_latency_ms Aggregate request latency (ms).\n");
        out.push_str("# TYPE kgateway_request_latency_ms summary\n");
        out.push_str(&format!("kgateway_request_latency_ms_sum {latency_sum}\n"));
        out.push_str(&format!("kgateway_request_latency_ms_count {total}\n"));

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_renders() {
        let m = Metrics::new();
        m.record(200, 10);
        m.record(200, 20);
        m.record(429, 1);
        let text = m.render_prometheus();
        assert!(text.contains("kgateway_requests_total 3"));
        assert!(text.contains("kgateway_requests_by_status{status=\"200\"} 2"));
        assert!(text.contains("kgateway_requests_by_status{status=\"429\"} 1"));
        assert!(text.contains("kgateway_request_latency_ms_sum 31"));
        assert!(text.contains("kgateway_request_latency_ms_count 3"));
    }
}
