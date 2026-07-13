# 12 — M10 Phase 2: request/response content capture

Scope for capturing the **full request/response payloads** (chat messages, tool calls,
prompts, rerank docs, …) into the logs — the one hard architectural change deferred from
the M10 MVP (`docs/11-logs-plan.md`, `docs/02-roadmap.md` → M10). The MVP captured scalar
metadata only; this phase adds the actual content behind an opt-in, admin-only gate.

## Why this is "the hard one"

Three coupled problems, none of which the MVP had to solve:

1. **The observer only sees a summary.** `RequestObserver::on_response(ctx, &CallRecord)`
   (`crates/kgateway-core/src/observer.rs`) receives a `CallRecord` — provider/model/status/
   tokens — **not** the request or response payloads. Content has to be threaded from the
   engine call sites into the record. Because observers are capability-generic (they run on
   chat/embeddings/images/audio/rerank) and each capability's content has a different shape,
   the payload is carried as **pre-serialized JSON strings**, produced at the call site.

2. **Hot-path cost.** The MVP's `LoggingPlugin::on_response` does an inline
   `store.append(log).await` on the request path. That's fine for a ~200-byte scalar row; it
   is **not** fine for multi-KB message blobs written synchronously per request. This phase
   moves durable writes off the hot path onto a **bounded channel + background batch-writer**.

3. **Safety.** Raw messages can carry secrets/PII. KGateway has no reversible-redaction or
   `Logs:Reveal` RBAC (those are full-parity, later). So content capture must be **off by
   default, opt-in, size-capped, and only ever returned from the admin-guarded detail
   endpoint** — never in list responses.

## Design decisions

### D1 — Threading content: serialize at the engine call site
`CallRecord` gains two fields:
```rust
pub request_body: Option<String>,   // serialized JSON, truncated to max_body_bytes
pub response_body: Option<String>,  // serialized JSON, truncated to max_body_bytes
```
The engine holds a `ContentCapture` policy (built from config). At each record site — chat
(`engine.rs:152`), the cache-hit short-circuit (`:134`), the reject path (`:140`), and the
`cap_record` capability sites — when capture is enabled the engine serializes the request
and response (`serde_json::to_string`, truncated) and sets the fields, exactly mirroring how
`rec.cache_hit = true` is already set at the call site. When disabled, the fields stay `None`
at **zero serialization cost** (the `serde_json` call is skipped entirely, not computed-then-
dropped).

Per-capability content matrix (pragmatic — skip useless/huge blobs):
| Capability   | request_body                | response_body                          |
|--------------|-----------------------------|----------------------------------------|
| chat         | messages + params + tools   | message content + tool_calls + finish  |
| embeddings   | input text                  | *omitted* (vectors are huge & opaque)  |
| images       | prompt + params             | URLs / metadata only (never b64 blobs) |
| audio        | text side only              | text side only (never binary)          |
| rerank       | query + documents           | ranked indices + scores                |

### D2 — Off the hot path: bounded channel + background batch-writer
Introduce a `LogWriter` background task that owns the `Arc<dyn LogStore>` and drains a
bounded `tokio::mpsc::Receiver<RequestLog>`, batching (flush on N=128 buffered **or** every
~200 ms) via a new `LogStore::append_batch(Vec<RequestLog>)` (default = loop `append`;
SQLite/Postgres override with a multi-row `INSERT`). `LoggingPlugin` becomes a producer:
`tx.try_send(log)` — non-blocking. On `Full` it drops the log and bumps a `dropped: AtomicU64`
counter (surfaced via metrics + `GET /api/logs/dropped`). SSE broadcast stays
synchronous (it's cheap, in-memory) so the live tail is unaffected.

- **Applies to all logs, not just content** — it's the correct hot-path architecture and
  content rides on top. Trade-off: a crash loses the bounded in-flight buffer (it's logs, not
  billing; the `dropped` counter makes loss visible).
- **Graceful shutdown**: on SIGTERM/SIGINT, close the sender and `await` the writer's final
  flush (wire into the existing shutdown path in `main.rs`).

### D3 — Storage: same table, body columns excluded from list queries
Add `request_body`/`response_body` `TEXT` (nullable) columns to `request_logs` (+ idempotent
ALTER migrations, same pattern as the MVP columns). Crucially, **`recent`/`query` do NOT
select the body columns** (they return `None`) so list/live-tail queries stay lean; only
`get(id)` selects them. Postgres TOASTs large text out-of-line automatically, so the main heap
doesn't bloat; SQLite is acceptable at the target scale. *(Alternative: a separate
`log_content` table joined only on detail — cleaner for large-payload offload later, but adds
a join and a second write. Deferred unless scale demands it.)*

### D4 — Safety envelope for Phase 2
- New config block `content_logging { enabled: bool = false, max_body_bytes: usize = 16384,
  capture_streaming: bool }`.
- Bodies are returned **only** by `GET /api/logs/{id}` (already admin-guarded) — never by
  `/api/logs` or `/api/logs/stream`.
- Truncation marker when a body exceeds `max_body_bytes` (store a `"...[truncated]"` suffix
  or a `truncated: bool` flag).
- **No field-level redaction in Phase 2** — the gate is opt-in + size-cap + admin-only. A
  startup warning is logged when `content_logging.enabled` is true. Reversible redaction +
  `Logs:Reveal` RBAC remain full-parity (later).

### D5 — Streaming capture
Streamed chat responses are assembled from SSE deltas, so there's no single `ChatResponse` to
serialize. When `capture_streaming` is on, tee the stream: accumulate the assembled text (cap
at `max_body_bytes`) and write the record at end-of-stream (extending the existing best-effort
stream record in `chat_stream`, `engine.rs:~586`). If off, streamed requests capture the
**request** body only. *(This is the fiddliest sub-item; it can ship in the same phase or be
split out — see open decisions.)*

## Work breakdown

| # | Area | Files | Notes |
|---|------|-------|-------|
| A | Config + policy | `kgateway-server/src/config.rs`, `kgateway-core` (a `ContentCapture` struct) | `content_logging` block; plumb into `build_configured_engine` |
| B | Engine capture | `kgateway-core/src/observer.rs` (fields), `engine.rs` (call sites, serialize+truncate) | zero-cost when disabled |
| C | Async writer | `kgateway-store` (`append_batch`, multi-row INSERTs), `kgateway-plugins/src/logging.rs` (producer + `dropped`), a `LogWriter` task | bounded mpsc, batch flush, shutdown flush |
| D | Store columns | `kgateway-store` sqlite.rs/postgres.rs/lib.rs | new columns + migrations; `get` selects bodies, `query`/`recent` don't |
| E | Server | `handlers.rs` (`/api/logs/dropped`), shutdown flush wiring | detail already returns the widened `RequestLog` |
| F | UI | `ui/app/logs/page.tsx` | detail drawer: collapsible pretty-printed request/response sections; "capture disabled" empty state; raw/pretty toggle |

**Fan-out plan (after the shared core lands):** A+B+C+D are coupled (data model + engine +
writer) and I own them as one green base, then fan out **E (server)** and **F (UI)** as
parallel agents against the frozen `RequestLog`/endpoint contract — the same two-wave shape
that worked for the MVP.

## Testing
- Unit: truncation at `max_body_bytes`; capture-off ⇒ bodies `None` (and serializer not
  called); `append_batch` round-trip; writer batching + `dropped` increment on a full channel;
  per-capability content matrix (embeddings omit vectors, etc.).
- Integration (live smoke): capture on ⇒ `/api/logs/{id}` returns bodies while `/api/logs`
  omits them; capture off ⇒ bodies null; streamed capture assembles the response; shutdown
  flush drains the buffer.
- `code-reviewer` agent pass, then `cargo test`/`clippy -D warnings` + `ui` build green.

## Behavior notes / known limitations
- **Streamed records finalize at end-of-stream when `capture_streaming` is on**, emitted by a
  `Drop` guard (`StreamCaptureGuard`) living inside the response-body stream. Because it fires
  on Drop, the record is emitted on normal completion **and** on early client disconnect — an
  aborted stream is still logged, carrying the request body and the *partial* response text
  accumulated before the cut (verified live: a stream cancelled mid-flight logged
  `response_body: "PART1_"`). Exactly one record per stream (no double-count). The emit runs
  on a detached task (Drop can't await); a record landing exactly during process shutdown may
  be dropped and counted.
- **The SSE live tail never carries captured bodies.** The broadcast path strips
  `request_body`/`response_body` before sending, mirroring the lean LIST contract — captured
  content is reachable only via the admin-guarded `GET /api/logs/{id}`.
- **Streaming cache hits / rejects are audited + governed.** A `pre_llm` short-circuit
  (semantic-cache hit) or reject on a streaming request records an observer event just like
  the non-streaming path, so `stream: true` can't bypass governance token accounting.
- **The Drop-guard emit is runtime-guarded.** The deferred record is emitted via
  `Handle::try_current()` → `handle.spawn`; if the guard is dropped with no Tokio runtime in
  context it logs and drops the record rather than panicking in `drop` (which would abort).
- **Errored/aborted streams record their real status.** The guard tracks the last upstream
  error seen while consuming the stream, so a mid-flight failure is logged with that status
  (and error detail), not a blanket 200.
- **Deferred stream accounting has no ordering guarantee.** The end-of-stream record runs on
  a detached task, so governance `on_response` for a streamed request may land after the
  client's next request begins. Harmless today (streamed token totals are 0, so no budget is
  deducted); revisit when full stream token accounting lands, as it would open a small
  budget-race window.
- **Streamed token totals remain 0** in the record (unchanged from the MVP) — full stream
  token accounting is separate future work.
- **No field-level redaction** — capture is gated by opt-in + size cap + admin-only read.

## Explicitly out of scope (→ full parity, later)
Reversible redaction + `Logs:Reveal` RBAC; team/customer attribution; cost/latency/token
histograms + rankings; `/api/logs/filterdata`; sessions (parent-request grouping);
object-storage offload for large payloads; ClickHouse backend.

## Resolved decisions (locked 2026-07-13)
1. **Streaming capture — INCLUDE in this phase.** Tee the stream, accumulate the assembled
   response text (capped at `max_body_bytes`), write the record at end-of-stream (D5).
2. **Storage model — SAME TABLE, columns excluded from list queries** (D3). `request_body`/
   `response_body` on `request_logs`; `recent`/`query` never select them, only `get(id)` does.
   No separate content table for now.
3. **Async writer — ALWAYS-ON** (D2). All logs flow through the bounded channel + batch-writer;
   the MVP's inline append is retired. A crash loses the small in-flight buffer (acceptable for
   logs; the `dropped` counter surfaces it).
