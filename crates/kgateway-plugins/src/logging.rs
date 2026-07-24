//! Built-in audit logger. A [`RequestObserver`] that appends a `RequestLog` for EVERY
//! request (chat, embeddings, images, audio, rerank — not just chat) to a pluggable
//! `LogStore`. It never affects the request outcome; log-store failures are swallowed
//! with a warning so a broken log backend can't turn a good response into an error.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use kgateway_core::context::Ctx;
use kgateway_core::observer::{CallRecord, RequestObserver};
use kgateway_store::{LogStore, RequestLog};
use tokio::sync::{broadcast, mpsc, Notify};

use crate::pricing;
use crate::redaction::Redactor;

/// Max logs the batch writer drains and inserts per wakeup.
const WRITER_BATCH: usize = 128;

/// Records a `RequestLog` for every request, fans it out on a broadcast channel (SSE live
/// tail), and persists it. Persistence goes through an async batch writer when one is
/// attached (`with_writer`) — keeping durable writes off the request hot path — and falls
/// back to an inline `store.append` otherwise (used in tests). It never affects the
/// request outcome.
pub struct LoggingPlugin {
    store: Arc<dyn LogStore>,
    /// When set, each appended log is also published here (best-effort; a send error
    /// just means no live-tail subscribers, which is fine).
    tx: Option<broadcast::Sender<RequestLog>>,
    /// When set, durable writes are handed to the async batch writer via this bounded
    /// channel instead of an inline `store.append`.
    writer_tx: Option<mpsc::Sender<RequestLog>>,
    /// Count of logs dropped because the writer channel was full (backpressure) — surfaced
    /// at `GET /api/logs/dropped`.
    dropped: Arc<AtomicU64>,
    /// Redacts captured bodies before they're persisted/broadcast (M11). `None` = no
    /// redaction.
    redactor: Option<Arc<Redactor>>,
}

impl LoggingPlugin {
    pub fn new(store: Arc<dyn LogStore>) -> Self {
        Self {
            store,
            tx: None,
            writer_tx: None,
            dropped: Arc::new(AtomicU64::new(0)),
            redactor: None,
        }
    }

    /// Attach a redactor so captured bodies are redacted before persistence/broadcast.
    pub fn with_redactor(mut self, redactor: Arc<Redactor>) -> Self {
        self.redactor = Some(redactor);
        self
    }

    /// Attach a broadcast sender so appended logs are streamed to SSE subscribers.
    pub fn with_broadcast(mut self, tx: broadcast::Sender<RequestLog>) -> Self {
        self.tx = Some(tx);
        self
    }

    /// Route durable writes through the async batch writer (bounded channel). `dropped`
    /// is the shared counter incremented when the channel is full.
    pub fn with_writer(mut self, tx: mpsc::Sender<RequestLog>, dropped: Arc<AtomicU64>) -> Self {
        self.writer_tx = Some(tx);
        self.dropped = dropped;
        self
    }

    /// Build the persisted record from the request context and call outcome.
    fn build_log(ctx: &Ctx, record: &CallRecord) -> RequestLog {
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        RequestLog {
            request_id: ctx.request_id.to_string(),
            created_at,
            virtual_key: ctx.virtual_key.clone(),
            session_id: ctx.session_id.clone(),
            provider: record.provider.clone(),
            model: record.model.clone(),
            status: record.status,
            prompt_tokens: record.prompt_tokens,
            completion_tokens: record.completion_tokens,
            latency_ms: ctx.started_at.elapsed().as_millis() as u64,
            cost: pricing::estimate_cost(
                &record.model,
                record.prompt_tokens,
                record.completion_tokens,
            ),
            stream: record.stream,
            cache_hit: record.cache_hit,
            stop_reason: record.stop_reason.clone(),
            error_message: record.error_message.clone(),
            request_body: record.request_body.clone(),
            response_body: record.response_body.clone(),
            // Trace spans for the call waterfall. Serialization failure is not worth
            // failing an audit row over — the request still gets logged, just without
            // its trace.
            spans: if ctx.spans.is_empty() {
                None
            } else {
                serde_json::to_string(&ctx.spans.snapshot()).ok()
            },
            // Redaction is applied post-build (needs the redactor); defaults here.
            redacted: false,
            redaction_mapping: None,
        }
    }

    /// Redact captured bodies in place (M11). Replaces secrets/PII with placeholders and,
    /// when a key is configured, stores the encrypted reversible mapping as a small JSON
    /// object `{ "request": <blob>, "response": <blob> }` (only keys with redactions). No
    /// redactor configured ⇒ no-op.
    fn apply_redaction(&self, log: &mut RequestLog) {
        let Some(redactor) = &self.redactor else {
            return;
        };
        let mut mappings = serde_json::Map::new();
        let mut any = false;
        if let Some(body) = log.request_body.take() {
            let res = redactor.redact(&body);
            any |= res.any_redacted;
            if let Some(m) = res.mapping {
                mappings.insert("request".into(), serde_json::Value::String(m));
            }
            log.request_body = Some(res.redacted);
        }
        if let Some(body) = log.response_body.take() {
            let res = redactor.redact(&body);
            any |= res.any_redacted;
            if let Some(m) = res.mapping {
                mappings.insert("response".into(), serde_json::Value::String(m));
            }
            log.response_body = Some(res.redacted);
        }
        log.redacted = any;
        if !mappings.is_empty() {
            log.redaction_mapping =
                serde_json::to_string(&serde_json::Value::Object(mappings)).ok();
        }
    }
}

#[async_trait]
impl RequestObserver for LoggingPlugin {
    fn name(&self) -> &str {
        "logging"
    }

    async fn on_response(&self, ctx: &Ctx, record: &CallRecord) {
        let mut log = Self::build_log(ctx, record);
        // Redact BEFORE the log goes anywhere (broadcast or durable write): the persisted
        // bodies are the redacted ones, and the raw values survive only inside the
        // encrypted mapping.
        self.apply_redaction(&mut log);
        // Publish to the live tail first (cheap, in-memory) so subscribers see it even
        // if the durable write is slow; ignore the error (no subscribers). The SSE tail
        // mirrors the lean LIST contract — captured request/response bodies and trace
        // spans are NEVER broadcast (bodies would bypass the admin-only detail gate;
        // spans would bloat every live-tail frame); they go only to the durable store,
        // readable via GET /api/logs/{id}.
        if let Some(tx) = &self.tx {
            let lean = RequestLog {
                request_body: None,
                response_body: None,
                spans: None,
                ..log.clone()
            };
            let _ = tx.send(lean);
        }
        match &self.writer_tx {
            // Async path: hand off to the batch writer without blocking the request. If
            // the channel is full we drop the log (bounded memory) and count it.
            Some(tx) => {
                if tx.try_send(log).is_err() {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            // Inline fallback (no writer attached, e.g. tests).
            None => {
                if let Err(err) = self.store.append(log).await {
                    tracing::warn!(error = %err, "logging observer failed to append request log");
                }
            }
        }
    }
}

/// Background batch writer: drains the bounded channel and persists logs off the request
/// hot path. Batches up to [`WRITER_BATCH`] per wakeup. Exits when all senders are dropped
/// or when `shutdown` is signalled — draining whatever remains queued first, so a graceful
/// shutdown doesn't lose buffered logs.
pub async fn run_log_writer(
    store: Arc<dyn LogStore>,
    mut rx: mpsc::Receiver<RequestLog>,
    shutdown: Arc<Notify>,
) {
    let mut buf: Vec<RequestLog> = Vec::with_capacity(WRITER_BATCH);
    loop {
        tokio::select! {
            n = rx.recv_many(&mut buf, WRITER_BATCH) => {
                if n == 0 {
                    break; // all senders dropped
                }
                flush(&store, std::mem::take(&mut buf)).await;
            }
            _ = shutdown.notified() => {
                // Drain everything still queued, then exit.
                while let Ok(log) = rx.try_recv() {
                    buf.push(log);
                }
                flush(&store, std::mem::take(&mut buf)).await;
                break;
            }
        }
    }
}

/// Persist a batch, swallowing (but logging) errors — the writer must never panic the task.
async fn flush(store: &Arc<dyn LogStore>, batch: Vec<RequestLog>) {
    if batch.is_empty() {
        return;
    }
    let n = batch.len();
    if let Err(err) = store.append_batch(batch).await {
        tracing::warn!(error = %err, count = n, "log writer failed to persist batch");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgateway_store::MemoryLogStore;

    fn record(status: u16) -> CallRecord {
        CallRecord {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            status,
            prompt_tokens: 11,
            completion_tokens: 7,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn appends_a_log_for_a_successful_call() {
        let store = Arc::new(MemoryLogStore::default());
        let logger = LoggingPlugin::new(store.clone());
        let ctx = Ctx::new();

        logger.on_response(&ctx, &record(200)).await;

        let logs = store.recent(10).await.unwrap();
        assert_eq!(logs.len(), 1);
        let log = &logs[0];
        assert_eq!(log.status, 200);
        assert_eq!(log.provider, "openai");
        assert_eq!(log.model, "gpt-4o");
        assert_eq!(log.prompt_tokens, 11);
        assert_eq!(log.completion_tokens, 7);
        assert_eq!(log.request_id, ctx.request_id.to_string());
        // M10: cost is estimated from the static table and created_at is populated.
        assert!(log.cost.is_some());
        assert!(log.created_at > 0);
    }

    #[tokio::test]
    async fn broadcasts_appended_logs_to_subscribers() {
        let store = Arc::new(MemoryLogStore::default());
        let (tx, mut rx) = broadcast::channel(8);
        let logger = LoggingPlugin::new(store).with_broadcast(tx);
        let ctx = Ctx::new();

        logger.on_response(&ctx, &record(200)).await;

        let streamed = rx.try_recv().expect("a log should have been broadcast");
        assert_eq!(streamed.request_id, ctx.request_id.to_string());
        assert_eq!(streamed.status, 200);
    }

    #[tokio::test]
    async fn broadcast_strips_bodies_but_writer_keeps_them() {
        // Regression: captured bodies must never reach the SSE live tail (privacy), but
        // must reach the durable store (readable via the admin-only detail endpoint).
        let store = Arc::new(MemoryLogStore::default());
        let (btx, mut brx) = broadcast::channel(8);
        let (wtx, mut wrx) = mpsc::channel(8);
        let dropped = Arc::new(AtomicU64::new(0));
        let logger = LoggingPlugin::new(store)
            .with_broadcast(btx)
            .with_writer(wtx, dropped);
        let ctx = Ctx::new();
        let mut rec = record(200);
        rec.request_body = Some("REQ_SECRET".into());
        rec.response_body = Some("RESP_SECRET".into());

        logger.on_response(&ctx, &rec).await;

        let streamed = brx.try_recv().expect("broadcast");
        assert!(streamed.request_body.is_none(), "body leaked to SSE");
        assert!(streamed.response_body.is_none(), "body leaked to SSE");

        let written = wrx.try_recv().expect("writer");
        assert_eq!(written.request_body.as_deref(), Some("REQ_SECRET"));
        assert_eq!(written.response_body.as_deref(), Some("RESP_SECRET"));
    }

    #[tokio::test]
    async fn writer_persists_and_shutdown_flushes() {
        let store = Arc::new(MemoryLogStore::default());
        let (tx, rx) = mpsc::channel(16);
        let dropped = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(Notify::new());
        let writer = tokio::spawn(run_log_writer(store.clone(), rx, shutdown.clone()));

        let logger = LoggingPlugin::new(store.clone()).with_writer(tx, dropped.clone());
        let ctx = Ctx::new();
        logger.on_response(&ctx, &record(200)).await;
        logger.on_response(&ctx, &record(200)).await;

        // Signal shutdown; the writer drains what's queued, then exits.
        shutdown.notify_one();
        writer.await.expect("writer task");

        let logs = store.recent(10).await.unwrap();
        assert_eq!(logs.len(), 2);
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn producer_drops_when_writer_channel_full() {
        let store = Arc::new(MemoryLogStore::default());
        // Capacity 1 and no writer draining it: after the first send the channel is full.
        let (tx, _rx) = mpsc::channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let logger = LoggingPlugin::new(store).with_writer(tx, dropped.clone());
        let ctx = Ctx::new();
        for _ in 0..5 {
            logger.on_response(&ctx, &record(200)).await;
        }
        assert!(dropped.load(Ordering::Relaxed) >= 1);
    }

    #[tokio::test]
    async fn appends_a_log_for_a_failed_call() {
        let store = Arc::new(MemoryLogStore::default());
        let logger = LoggingPlugin::new(store.clone());
        let ctx = Ctx::new();

        logger.on_response(&ctx, &record(503)).await;

        let logs = store.recent(10).await.unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].status, 503);
    }
}
