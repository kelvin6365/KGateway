# 17 — OTLP / OpenTelemetry export (M15)

Export **traces** (a span per LLM request) and **metrics** over OTLP so KGateway plugs into
Grafana/Tempo, Jaeger, Datadog, or any OTel collector. Completes the observability arc: the
in-gateway logs/analytics (M10–M12) cover the dashboard; OTLP covers external monitoring.

## Approach
- **Transport: OTLP over HTTP/protobuf** (`opentelemetry-otlp` with the reqwest HTTP client),
  not gRPC — avoids pulling in the tonic/grpc stack; reqwest + tokio are already deps. Default
  endpoint `http://localhost:4318`.
- **A `RequestObserver` (`OtelObserver`)** emits telemetry on every capability call — the same
  hook logging/governance use. On `on_response` it:
  - builds a completed **span** with explicit start (`ctx.started_at`) / end (now) and
    attributes: `provider`, `model`, `http.status`, `llm.prompt_tokens`,
    `llm.completion_tokens`, `stream`, `cache_hit`, `virtual_key`, `error`;
  - records **metrics**: a request counter, a latency histogram (ms), and a token counter —
    all with `provider`/`model`/`status` attributes.
- The OTel SDK (tracer + meter providers) is initialized once at startup when `otlp.enabled`,
  held in `AppState`, and **flushed/shutdown on graceful shutdown** (alongside the log writer).
- Lives in the **server crate only** (`otel.rs`) so the heavy OTel deps don't touch core/plugins;
  `OtelObserver` implements core's public `RequestObserver` trait.

## Config
```jsonc
"otlp": {
  "enabled": false,                       // off by default
  "endpoint": "http://localhost:4318",    // OTLP HTTP base
  "service_name": "kgateway",
  "traces": true,
  "metrics": true
}
```

## Work items
| # | Area | Files |
|---|------|-------|
| A | `OtlpConfig` | `kgateway-server/src/config.rs` |
| B | OTel deps (http-proto + reqwest client) | `Cargo.toml` (+ server) |
| C | `otel::init()` (providers) + `OtelObserver` (span + metrics) | new `kgateway-server/src/otel.rs` |
| D | Wire observer into engine build; hold providers in state; shutdown flush | `app.rs`, `main.rs` |

## Testing
- Unit: `OtelObserver::on_response` builds a span + records metrics without panicking when the
  SDK is a no-op provider (disabled path is a clean no-op).
- Live: a tiny mock OTLP/HTTP collector (accepts `POST /v1/traces`, `/v1/metrics`) confirms the
  gateway actually exports a span + metric after a chat.

## Done beyond the MVP
- **W3C `traceparent` context propagation.** Inbound trace context is parsed
  (`otel::parse_traceparent`) and stashed in `Ctx.ext` by the handlers; the observer builds
  the span with `build_with_context` so it nests under the caller's distributed trace. No
  config needed. Live-verified: the exported span protobuf carries the propagated trace-id.

## Out of scope (later)
Span events for plugin/tool steps, exemplars, gRPC transport, log-signal (OTLP logs) export.
