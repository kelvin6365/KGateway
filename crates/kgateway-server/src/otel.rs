//! OTLP / OpenTelemetry export (M15). Initializes OTel trace + metric providers with an
//! OTLP-over-HTTP exporter and exposes an [`OtelObserver`] (a `RequestObserver`) that emits
//! a span + metrics per request. Lives in the server crate so the OTel deps don't reach
//! core/plugins. See `docs/17-otlp-plan.md`.

use std::time::SystemTime;

use async_trait::async_trait;
use axum::http::HeaderMap;
use kgateway_core::context::Ctx;
use kgateway_core::observer::{CallRecord, RequestObserver};

use opentelemetry::metrics::{Counter, Histogram, MeterProvider as _};
use opentelemetry::trace::{
    Span, SpanBuilder, SpanContext, SpanId, SpanKind, TraceContextExt, TraceFlags, TraceId,
    TraceState, Tracer as _, TracerProvider as _,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_otlp::{MetricExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::runtime;
use opentelemetry_sdk::trace::{Tracer, TracerProvider};
use opentelemetry_sdk::Resource;

use crate::config::OtlpConfig;

/// Parse a W3C `traceparent` header value into a remote [`SpanContext`], or `None` if it's
/// absent/malformed. Format: `00-<32-hex trace-id>-<16-hex span-id>-<2-hex flags>`.
pub fn parse_traceparent(value: &str) -> Option<SpanContext> {
    let parts: Vec<&str> = value.trim().split('-').collect();
    if parts.len() != 4
        || parts[0] != "00"
        || parts[1].len() != 32
        || parts[2].len() != 16
        || parts[3].len() != 2
    {
        return None;
    }
    let trace = u128::from_str_radix(parts[1], 16).ok()?;
    let span = u64::from_str_radix(parts[2], 16).ok()?;
    let flags = u8::from_str_radix(parts[3], 16).ok()?;
    // All-zero trace/span ids are invalid per the spec.
    if trace == 0 || span == 0 {
        return None;
    }
    Some(SpanContext::new(
        TraceId::from_bytes(trace.to_be_bytes()),
        SpanId::from_bytes(span.to_be_bytes()),
        TraceFlags::new(flags),
        true, // remote
        TraceState::default(),
    ))
}

/// If the request carries a valid `traceparent`, stash the remote parent [`SpanContext`] in
/// the request context so the OTLP span nests under the caller's distributed trace. No-op
/// otherwise. Stored as a server-crate type in `Ctx.ext` — core stays OTel-free.
pub fn apply_trace_context(ctx: &mut Ctx, headers: &HeaderMap) {
    if let Some(sc) = headers
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_traceparent)
    {
        ctx.insert(sc);
    }
}

/// Owns the SDK providers so they stay alive (batch/periodic export) and can be flushed +
/// shut down on graceful exit.
pub struct OtelProviders {
    tracer_provider: Option<TracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
}

impl OtelProviders {
    /// Flush pending spans/metrics and shut the exporters down. Called on graceful shutdown.
    pub fn shutdown(&self) {
        if let Some(tp) = &self.tracer_provider {
            if let Err(e) = tp.shutdown() {
                tracing::warn!(error = %e, "otlp tracer provider shutdown error");
            }
        }
        if let Some(mp) = &self.meter_provider {
            if let Err(e) = mp.shutdown() {
                tracing::warn!(error = %e, "otlp meter provider shutdown error");
            }
        }
    }
}

/// A `RequestObserver` that exports a span + metrics per request over OTLP.
pub struct OtelObserver {
    tracer: Option<Tracer>,
    req_counter: Option<Counter<u64>>,
    latency_hist: Option<Histogram<f64>>,
    token_counter: Option<Counter<u64>>,
}

/// Build the OTel providers + observer from config. Returns a disabled (no-op) pair when
/// OTLP is absent or `enabled == false`. Exporter init failures are logged and downgrade to
/// disabled rather than failing startup.
pub fn init(cfg: Option<&OtlpConfig>) -> (Option<OtelObserver>, OtelProviders) {
    let disabled = || {
        (
            None,
            OtelProviders {
                tracer_provider: None,
                meter_provider: None,
            },
        )
    };
    let Some(cfg) = cfg.filter(|c| c.enabled) else {
        return disabled();
    };

    let resource = Resource::new(vec![KeyValue::new(
        "service.name",
        cfg.service_name.clone(),
    )]);

    // --- traces ---
    let (tracer, tracer_provider) = if cfg.traces {
        match SpanExporter::builder()
            .with_http()
            .with_endpoint(format!("{}/v1/traces", cfg.endpoint.trim_end_matches('/')))
            .build()
        {
            Ok(exporter) => {
                let provider = TracerProvider::builder()
                    .with_batch_exporter(exporter, runtime::Tokio)
                    .with_resource(resource.clone())
                    .build();
                let tracer = provider.tracer("kgateway");
                (Some(tracer), Some(provider))
            }
            Err(e) => {
                tracing::error!(error = %e, "otlp trace exporter init failed; traces disabled");
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    // --- metrics ---
    let (instruments, meter_provider) = if cfg.metrics {
        match MetricExporter::builder()
            .with_http()
            .with_endpoint(format!("{}/v1/metrics", cfg.endpoint.trim_end_matches('/')))
            .build()
        {
            Ok(exporter) => {
                let reader = PeriodicReader::builder(exporter, runtime::Tokio).build();
                let provider = SdkMeterProvider::builder()
                    .with_reader(reader)
                    .with_resource(resource)
                    .build();
                let meter = provider.meter("kgateway");
                let req_counter = meter
                    .u64_counter("kgateway.requests")
                    .with_description("Total LLM requests")
                    .build();
                let latency_hist = meter
                    .f64_histogram("kgateway.request.latency_ms")
                    .with_description("Request latency in milliseconds")
                    .build();
                let token_counter = meter
                    .u64_counter("kgateway.tokens")
                    .with_description("Total tokens (prompt + completion)")
                    .build();
                (
                    (Some(req_counter), Some(latency_hist), Some(token_counter)),
                    Some(provider),
                )
            }
            Err(e) => {
                tracing::error!(error = %e, "otlp metric exporter init failed; metrics disabled");
                ((None, None, None), None)
            }
        }
    } else {
        ((None, None, None), None)
    };

    let (req_counter, latency_hist, token_counter) = instruments;
    tracing::info!(
        endpoint = %cfg.endpoint,
        traces = tracer.is_some(),
        metrics = req_counter.is_some(),
        "OTLP export enabled"
    );

    let observer = OtelObserver {
        tracer,
        req_counter,
        latency_hist,
        token_counter,
    };
    (
        Some(observer),
        OtelProviders {
            tracer_provider,
            meter_provider,
        },
    )
}

#[async_trait]
impl RequestObserver for OtelObserver {
    fn name(&self) -> &str {
        "otlp"
    }

    async fn on_response(&self, ctx: &Ctx, record: &CallRecord) {
        let elapsed = ctx.started_at.elapsed();
        let end = SystemTime::now();
        let start = end.checked_sub(elapsed).unwrap_or(end);

        if let Some(tracer) = &self.tracer {
            let mut attrs = vec![
                KeyValue::new("provider", record.provider.clone()),
                KeyValue::new("model", record.model.clone()),
                KeyValue::new("http.status", record.status as i64),
                KeyValue::new("stream", record.stream),
                KeyValue::new("cache_hit", record.cache_hit),
            ];
            if let Some(vk) = &ctx.virtual_key {
                attrs.push(KeyValue::new("virtual_key", vk.clone()));
            }
            if let Some(err) = &record.error_message {
                attrs.push(KeyValue::new("error", err.clone()));
            }
            let builder = SpanBuilder::from_name("llm.request")
                .with_kind(SpanKind::Client)
                .with_start_time(start)
                .with_attributes(attrs);
            // Nest under the caller's trace when an inbound `traceparent` was propagated.
            let mut span = match ctx.get::<SpanContext>() {
                Some(parent) if parent.is_valid() => {
                    let cx = Context::new().with_remote_span_context(parent.clone());
                    tracer.build_with_context(builder, &cx)
                }
                _ => tracer.build(builder),
            };
            span.end_with_timestamp(end);
        }

        // Metrics carry only low-cardinality attributes.
        let m_attrs = [
            KeyValue::new("provider", record.provider.clone()),
            KeyValue::new("model", record.model.clone()),
            KeyValue::new("status", record.status as i64),
        ];
        if let Some(c) = &self.req_counter {
            c.add(1, &m_attrs);
        }
        if let Some(h) = &self.latency_hist {
            h.record(elapsed.as_millis() as f64, &m_attrs);
        }
        if let Some(t) = &self.token_counter {
            let tt = record.total_tokens();
            if tt > 0 {
                t.add(tt, &m_attrs);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool) -> OtlpConfig {
        OtlpConfig {
            enabled,
            endpoint: "http://127.0.0.1:4318".into(),
            service_name: "kgateway-test".into(),
            traces: true,
            metrics: true,
        }
    }

    #[test]
    fn disabled_when_absent_or_off() {
        assert!(init(None).0.is_none());
        assert!(init(Some(&cfg(false))).0.is_none());
    }

    #[test]
    fn parse_traceparent_valid() {
        let sc = parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
            .expect("valid traceparent");
        assert!(sc.is_valid());
        assert!(sc.is_remote());
        assert!(sc.trace_flags().is_sampled());
        assert_eq!(
            format!("{:032x}", u128::from_be_bytes(sc.trace_id().to_bytes())),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
    }

    #[test]
    fn parse_traceparent_rejects_malformed() {
        let bad = [
            "",
            "garbage",
            // unsupported version
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            // wrong field lengths
            "00-short-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-short-01",
            // all-zero ids are invalid
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            // non-hex
            "00-zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-00f067aa0ba902b7-01",
        ];
        for v in bad {
            assert!(parse_traceparent(v).is_none(), "should reject: {v:?}");
        }
    }

    // Multi-thread runtime: `providers.shutdown()` blocks its thread waiting for the batch
    // exporter task, so it must run on a different worker than that task (production uses
    // the multi-threaded `#[tokio::main]` runtime, so this matches reality).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enabled_builds_observer_and_records_without_panic() {
        // Exporters build lazily (no connection needed); emitting to a dead endpoint just
        // queues/errors in the background — it must never panic the request path.
        let (obs, providers) = init(Some(&cfg(true)));
        let obs = obs.expect("observer built when enabled");
        let ctx = Ctx::new();
        let rec = CallRecord::new("openai".into(), "gpt-4o".into(), 200, 10, 5);
        obs.on_response(&ctx, &rec).await;
        providers.shutdown();
    }
}
