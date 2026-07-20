# 07 — Quality: testing & code review

Every milestone (and every sizeable part within one) must pass this gate before the next starts.

## Per-part gate

1. **Compiles clean:** `cargo build --workspace` — zero warnings ideally.
2. **Lints:** `cargo clippy --workspace --all-targets -- -D warnings`.
3. **Formatted:** `cargo fmt --all --check`.
4. **Unit tests pass:** `cargo test --workspace`.
5. **Code review:** a `code-reviewer` agent reviews the diff (correctness, error handling, concurrency, faithfulness to the engine's plugin/hook semantics). Findings triaged; blocking ones fixed.
6. **Behavior verified:** for anything with a runtime surface, exercise it (curl the endpoint / run the mock) — not just tests.

## Testing strategy by layer

| Layer | What to test | How |
|---|---|---|
| **Schema** | serde round-trip, OpenAI wire compatibility, streaming delta accumulation | table-driven unit tests; golden JSON fixtures |
| **Provider** | request encode, response decode, one stream case, one error→`KgError` mapping (retryable flag) | `wiremock` / `mockito` HTTP mock; no live API keys in CI |
| **Router / engine** | fallback on retryable error, key selection distribution, per-provider isolation, no-cascade | deterministic fakes implementing `Provider` |
| **Plugins** | pre/post ordering (LIFO), short-circuit path, error-is-non-blocking | fake plugins recording call order |
| **Store** | CRUD, migrations, SQLite↔Postgres parity | `sqlx::test` with temp DB |
| **Governance** | budget exhaustion, rate-limit trip, vkey scoping | time-controlled token bucket |
| **HTTP transport** | endpoint contract, SSE framing, error status mapping | `axum` test client (`tower::ServiceExt::oneshot`) |
| **Integration adapters** | OpenAI/Anthropic/... inbound → internal → outbound | fixture requests |

## Test infra choices

- Provider HTTP mocking: **`wiremock`** (async) or **`mockito`**.
- HTTP-layer: axum's `oneshot` via `tower`.
- Property tests for schema round-trips: **`proptest`** (optional).
- Benchmarks (M9): **`criterion`** for per-request overhead; a `k6`/`oha` load test for RPS.
- **No live-API tests in CI.** Keep an opt-in `--ignored` suite that hits real APIs locally when keys are present.

## CI (`.github/workflows/ci.yml`)

```
jobs:
  rust:   fmt-check → clippy(-D warnings) → test --workspace
  ui:     pnpm install → lint → typecheck → build
  docker: build image (no push) on main
```

## Code-review checklist (KGateway-specific)

- [ ] Hook error paths are **non-blocking** (logged, not fatal) — matches the engine's hook semantics.
- [ ] Rejection uses **`ShortCircuit`**, not `Err`.
- [ ] `pre_request` mutations are committed before fallback loop; `pre_llm` runs per-attempt.
- [ ] `KgError::is_retryable` set correctly for each provider error mapping.
- [ ] Provider isolation preserved (no shared unbounded state that lets one provider stall others).
- [ ] Streaming: no full-stream buffering in `Ctx`; only IDs/handles held.
- [ ] Secrets never logged; `${ENV}` interpolation not leaked in error messages.
- [ ] Capability registry updated when a provider gains/loses a trait.

## Definition of Done (milestone)

All gate steps green + review findings resolved + docs/roadmap checkbox ticked + (if API surface changed) `05-frontend.md` API contract updated.

## Docs that can't go stale

Two guards keep the generated documentation honest, and both run in `cargo test`:

- **Route/catalog drift** (`api_catalog::drift_tests`) — parses the `.route(...)` table out of
  `app.rs` and asserts it matches `ENDPOINTS` in both directions. Adding an endpoint without
  documenting it, or documenting one that no longer exists, fails the gate.
- **Renderer consistency** (`api_docs::tests`) — every documented parameter must reach
  `/openapi.json`; multipart endpoints must not be described as JSON; code samples must follow
  the configured base URL; Markdown table cells must escape pipes; no example may single-quote a
  shell variable it expects to expand.

**What they do not check:** whether a *described* default, cap, or required-ness matches the
handler. `"limit is capped at 200"` is prose, and prose can be wrong — verify it against the
constant when you touch it.
