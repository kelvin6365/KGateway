# 15 — Performance (M13)

Measured validation of the founding premise — *Rust instead of Go for better performance*.
Two layers of measurement: **Criterion micro-benchmarks** (in-process, isolate the gateway's
own overhead) and an **HTTP load test** (end-to-end through axum against an instant mock
upstream). Reproduce with `cargo bench` and the `ab` command below.

> Numbers below are from one dev machine (darwin, release build). Treat them as
> **relative** figures (layer-to-layer deltas), not absolute hardware specs.

## Micro-benchmarks — gateway overhead (`cargo bench`)

### Hot-path primitives (`crates/kgateway-core/benches/hotpath.rs`)
| Bench | Time | Note |
|---|---|---|
| `keyselect` (eligible + weighted pick, 8 keys) | **~99 ns** | comparable gateways advertise ~10 ns for the equivalent |
| schema serialize response | **~327 ns** | serde_json round of the internal schema |
| schema deserialize request | **~395 ns** | |
| `engine_chat` end-to-end (bare) | **~2.9 µs** | full `chat` pipeline vs an instant provider |

### Pipeline overhead by layer (`crates/kgateway-plugins/benches/pipeline.rs`)
Same request through an engine with progressively more layers, against an instant provider —
so each number is KGateway's own per-request cost (no network):

| Layers | Time | Δ vs bare |
|---|---|---|
| `bare` (engine only) | **3.13 µs** | — |
| `+ governance` (virtual-key observer) | **3.22 µs** | +0.09 µs |
| `+ logging` (audit observer, in-mem store) | **3.45 µs** | +0.32 µs |
| `full` (logging + governance) | **3.56 µs** | +0.43 µs |
| `+ content capture` (JSON-serialize req+resp) | **4.29 µs** | +1.16 µs |
| `+ redaction` (prefilter + scan + AES-GCM) | **~15 µs** | (both bench bodies contain secrets) |

**Reading it:** the full production observability path (logging + governance) costs **~3.5 µs**
per request — comfortably inside a typical ~11–59 µs mean-overhead range for comparable gateways. Content
capture adds ~0.7 µs (two JSON serializations). Redaction is the heaviest layer, but see the
decomposition + optimization below — for realistic traffic (most bodies contain no secrets) it
is far cheaper than the all-bodies-match bench implies.

### Redaction cost, decomposed (`redaction/*` benches)
Per-body redact of a ~180-byte JSON body:

| Case | Time | What it measures |
|---|---|---|
| no secrets (prefilter miss) | **~0.30 µs** | one `RegexSet` prefilter pass, early return |
| secrets present, mask-only | **~4.2 µs** | prefilter + per-pattern span scan + string build |
| secrets present + AES-GCM mapping | **~6.3 µs** | + encrypt/serialize/base64 the reversible mapping |

**Optimization applied (RegexSet prefilter).** Most request/response bodies contain no
secrets, so redaction now runs a single `RegexSet::is_match` prefilter first and returns in
**~0.3 µs** when nothing matches — skipping all per-pattern scanning, allocation, and crypto.
Only bodies that actually contain a secret pay the full ~6 µs. I also tried collapsing the
patterns into one **alternation regex**: it was *slower* on matching bodies (6.4 µs vs 4.2 µs
for span extraction) because the big combined automaton (bounded-repetition card numbers, `\b`
boundaries) hits a slow match path — so the shipped design is `RegexSet` prefilter + N
fast-failing per-pattern scans, which won on both the no-match and match cases.

At ~3.5 µs of overhead, the gateway's own single-core ceiling is ≈ **285k req/s** — real
throughput is bounded by upstream/network, not KGateway.

## HTTP load test (end-to-end)
Release server against an instant **threaded** mock upstream (the mock, not the gateway, is
the bottleneck — see caveat), 30k requests at concurrency 50:

```
ab -k -c 50 -n 30000 -p body.json -T application/json \
   http://127.0.0.1:8090/v1/chat/completions
```
| Metric | Value |
|---|---|
| Requests/sec | ~3,850 (mock-bound) |
| Latency p50 / p95 | **1 ms / 1 ms** |
| Latency p99 | ~32 ms (mock GC / connection churn) |
| Failed | ~0.08% (mock under load, not the gateway) |

This path includes the always-on logging observer + async batch writer. p50/p95 of **1 ms**
confirms KGateway adds ≈1 ms end-to-end; the req/s figure is limited by the Python mock (even
threaded, GIL-bound), **not** by KGateway — the clean gateway figure is the ~3.5 µs micro-bench
above. A production-grade upstream + a native load tool (`wrk`/`bombardier`) would push req/s
far higher.

## Caveats (honest accounting)
- **The mock is the bottleneck** in the HTTP test — Python `http.server` (even threaded) can't
  saturate the gateway. The micro-benchmarks are the trustworthy gateway-overhead figures.
- Single machine, single run, no pinning/isolation — deltas between layers are meaningful;
  absolute µs will vary by hardware.
- Streaming, failover, and cache-hit paths aren't separately benchmarked yet.

## Optimization follow-ups (not blocking)
- **Redaction — DONE:** added a `RegexSet` prefilter so secret-free bodies (the common case)
  cost ~0.3 µs instead of a full per-pattern scan. Alternation-regex approach was measured and
  rejected (slower on matching bodies). Remaining lever for secret-bearing bodies is the
  AES-GCM/serialize step (~2 µs), not the regex.
- Push analytics/query aggregations down to SQL (currently scan-and-fold) for large stores.
- A native load harness (`wrk`/`bombardier`) + a concurrent Rust mock for a true throughput
  ceiling.
