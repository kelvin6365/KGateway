# 18 — Reference parity audit (M18)

Internal parity / gap audit of KGateway (Rust) against a mature reference implementation, across
four areas, done after the semantic-cache cross-check found a real correctness bug (cache ignored
request params). Each finding is `the reference does X` vs `KGateway does Y` with a
recommendation. Line refs verified in-repo by the audit agents.

## Findings, ranked (deduped across the four audits)

| # | Area | Sev | Gap | Effort |
|---|------|-----|-----|--------|
| A | providers | **HIGH** | `ChatRequest` silently drops sampling/control params (`stop`, `seed`, `frequency/presence_penalty`, `response_format`/JSON-mode, `tool_choice`, `n`, `logprobs`, `reasoning`, `stream_options`, …) and has no passthrough — behavior drift with zero error (the cache-bug class, at scale) | med |
| B | routing/stream | **HIGH** | Streaming has **no failover / no key-retry** on stream-open error (incl. error-as-first-chunk); non-stream path has both | large |
| C | routing | **HIGH** | **No backoff/jitter** between key/provider retries — a 429 immediately hammers the next key | small |
| D | streaming | **HIGH** | **Stream token usage never captured** (records 0) — gateway doesn't even request `stream_options.include_usage`; under-counts cost/quota/rankings | med |
| E | governance | **HIGH** | Budget is lifetime **token-count, never resets, not cost** — can't express "$50/month"; `pricing::estimate_cost` already exists but governance ignores it | med |
| F | governance | HIGH | Governance counters are **in-memory per-process** — bypassable by restart, N×limit across replicas | large |
| G | routing/stream | **HIGH→MED** | **401/402/403 don't rotate keys** — one revoked key aborts the whole provider instead of trying the next valid key | small |
| H | governance | MED (sec) | **Open-mode unknown-key bypass** — a made-up/deleted `vk_…` escapes all limits; the reference 401s it | small |
| I | governance | MED | Model **allow-list only**, no deny-list (reference: deny wins) | small |
| J | streaming | MED | Timeout / client-cancel / network all collapse to **502** — no 504 / 499 distinction | small |
| K | providers/stream | MED | Structured provider error fields (`type`/`code`/`param`) scrubbed away — SDK clients lose machine-readable errors (message scrub is a correct leak-defense; keep it, pass the safe fields) | med |
| L | providers | MED | Streamed `Delta` has **no `tool_calls`** — streamed tool/function calls can't be reconstructed | med |
| M | routing/stream | MED | **No stream idle timeout** — a silent upstream hangs the client AND permanently holds a per-provider concurrency permit (availability/DoS) | med |
| N | governance | MED | Rate limit is request-count only, hardcoded 60s — no token-throughput, no configurable period | med |
| O | providers | LOW | `fireworks`/`parasail` missing from the OpenAI-compat list (trivial) | trivial |
| P | streaming | LOW | Anthropic streams drop `finish_reason`/`stop_reason` (`message_delta` ignored) | small |
| — | providers | HIGH* | Text-only content (no multimodal); missing capability families (Responses/Batch/Files). High value but large wire-format changes — own track | large |

\* Deferred to their own milestones (wire-format / new capability surface).

## Fixed in this pass (M18 batch — high value, low risk) ✅ DONE
**A, C, G, E, H, I, O** — implemented + unit-tested, workspace green (test + clippy `-D warnings` +
fmt). See the M18 checklist in `02-roadmap.md`. Chosen for value×safety and independence; each is
self-contained and testable.

## Follow-on backlog (larger / own sessions)
- ✅ **B / M / D** streaming resilience — DONE (M19): stream-open failover + first-chunk peek,
  per-stream idle timeout + permit release, and stream usage capture. See `02-roadmap.md`.
- ✅ **F** shared/persistent governance counters — DONE (M20): `GovernanceStore` with an
  in-process default + a Postgres-backed shared store (atomic upserts), so limits hold under
  horizontal scaling. Reuses the existing DB connection. See `02-roadmap.md`.
- **K / J / P** error fidelity: pass safe structured error fields, distinguish 504/499/502,
  Anthropic stop_reason in stream.
- ✅ **L** streamed tool-call deltas — DONE (M21): `Delta.tool_calls` + `ToolCallAccumulator`
  (OpenAI-compatible providers); native adapters (Anthropic/Gemini/…) still to map. **N**
  token-throughput + configurable-period rate limits.
- Multimodal message content; Responses/Batch/Files capabilities; native **Vertex** provider.

## KGateway is fine / better than the reference (don't chase)
- **Rate-limit precision**: a true sliding window (prunes per-request timestamps) vs the
  reference's fixed-window ~2× burst — KGateway is *ahead*.
- **SSE UTF-8 reassembly**: byte-buffered frame decoding across TCP chunks — correct, tested.
- **Capability dispatch**: opt-in `as_embeddings()/…` returns a clean pre-dispatch `Unsupported`
  — cleaner than the reference's wide interface.
- **Streaming governance/audit**: enforcement + the `Drop`-guard record on early disconnect —
  closes the `stream:true` bypass; arguably ahead.
- **Fallback DoS cap** (client fallbacks capped at 5) — safer than the reference (uncapped).
- **Non-stream failover + weighted key selection**: faithful port, well-tested.
- **Circuit breaker**: neither has a standing cross-request one (the reference's is within a
  single request) — a *new feature for both*, not a KGateway regression.
