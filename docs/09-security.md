# 09 — Security Review

An independent security review was run over `crates/`, `ui/`, and `charts/`. This records the
findings and their resolution.

## Findings & status

| # | Severity | Finding | Status |
|---|---|---|---|
| 1 | **Critical** | Governance (auth / rate limits / budgets / model allow-lists) and audit logging ran only on chat — bypassed on `/v1/embeddings`, `/v1/images`, `/v1/audio/*`, `/v1/rerank` | ✅ **Fixed** |
| 2 | **High** | Control-plane (`/api/*`, `/metrics`) was unauthenticated | ✅ **Fixed** (opt-in) |
| 3 | **High** | Unbounded-depth JSON could stack-overflow (`abort()`) the whole process | ✅ **Fixed** |
| 4 | **Medium** | Unbounded client `fallbacks[]` → outbound-call fan-out; no global timeout | ✅ **Fixed** (cap) / ⏳ timeout |
| 5 | **Medium** | `ApiKey` derived `Debug` with no redaction (latent leak) | ✅ **Fixed** |
| 6 | **Low** | Upstream provider error bodies forwarded to clients verbatim | ⏳ Tracked |
| 7 | **Low** | CORS hardcoded `permissive()`, not configurable | ⏳ Tracked |

### 1 — Governance/audit bypass (Critical) → fixed
Governance and logging were `LlmPlugin`s, which are `ChatRequest`-typed and so only ran on
chat. Introduced a capability-generic **`RequestObserver`** (`on_request` check +
`on_response` record) that the engine runs on **every** method. `GovernancePlugin` and
`LoggingPlugin` are now observers; all handlers set `ctx.virtual_key` from the
`Authorization` header. Verified: images/rerank return 401 (no key) / 200 (valid) / 400
(model not allowed), and non-chat calls are audited.

### 2 — Control-plane auth (High) → fixed (opt-in)
`config.admin_token` (env-interpolated). When set, `/api/*` and `/metrics` require
`Authorization: Bearer <token>` (`auth::require_admin` middleware on a split
control-plane router). When unset, they're open and a startup **warning** is logged.
Verified: 401 without/with wrong token, 200 with correct; data-plane unaffected.

### 3 — JSON depth DoS (High) → fixed
`auth::json_depth_guard` middleware buffers JSON bodies and rejects nesting deeper than
`MAX_JSON_DEPTH = 64` with a 400 **before** `serde_json` recurses. Verified: a 200-deep
body returns 400 and the process survives.

### 4 — Fallback fan-out / timeout (Medium) → capped
Client `fallbacks[]` is capped at `MAX_FALLBACKS = 5`. Per-request provider timeouts
already exist (`REQUEST_TIMEOUT`); a **global** `tower_http::timeout::TimeoutLayer` is a
tracked follow-on.

### 5 — Key redaction (Medium) → fixed
`ApiKey` now hand-implements `Debug` to print `value: "<redacted>"` (the `derive` would
leak the plaintext into any `{:?}`/`tracing::debug!(?key)`). No active leak existed, but
the type-level guard prevents a future one.

### 6, 7 — tracked follow-ons (Low)
- Map upstream provider error bodies to a generic client message (retain detail only in
  server-side tracing) — some providers echo a masked key fragment in error text.
- Make CORS an explicit, configurable origin allow-list (defaults to `permissive()`,
  acceptable now that the control-plane is auth-gated).

## Cleared (investigated, no issue)
Secret handling (keys never logged / never in `KgError` / never in the audit store or
`/api/providers` DTO); **SQL injection** (sqlx bound parameters only); **SSRF** (`base_url`
is admin-config, client `model` never reaches a URL); **path traversal** (multipart
`filename` is only a form-field name); **panics on client input** (no attacker-reachable
`unwrap`); budget arithmetic (`saturating_add`, `>=` checks); UI (no `dangerouslySetInnerHTML`,
`eval`, or client-side secret exposure); Helm (`existingSecret` escape hatch, no hardcoded
secrets).

## Access model — tokens vs virtual keys

Two credential classes exist, and they answer different questions:

| Credential | Configured in | Data plane (`/v1/*`) | Read APIs (logs / sessions / analytics) | Config writes / reveal |
|---|---|---|---|---|
| **Access token** (`admin_token`, `api_tokens` — viewer / operator / admin) | config, `${ENV}`-interpolated | no | **all data, every key** | per role (`config:write` operator+, `logs:reveal` admin) |
| **Virtual key** | config plaintext (`virtual_keys[].id` *is* the bearer secret) | yes (strict mode) | **only its own rows** — the `virtual_key` filter is forced server-side | never — 401 |

Rules of the model:

- **Open mode** — no tokens declared *and* no virtual keys: every API is open (dev). The
  first virtual key ends open mode for reads the same way it flips the data plane to strict.
- **Scoped reads** — a virtual key presented to `/api/logs*`, `/api/sessions*` or
  `/api/whoami` authenticates, but the response is force-restricted to that key's own
  traffic: list/aggregate endpoints get a server-side `virtual_key` override (the caller's
  own query param is ignored), `/api/logs/{id}` and `/api/sessions/{id}` return the same
  404 for another key's row as for a missing one (no existence oracle), `filterdata` can't
  enumerate the key roster, and the SSE tail only carries the key's own events.
- **Token-only surfaces** — `/metrics`, `/api/status`, `/api/providers`,
  `/api/config/*`, `/api/mcp/tools`, `/api/logs/dropped` reject virtual keys outright.
- **Key-id masking** — `GET /api/config/virtual-keys` returns real ids only to callers
  holding `config:write` (they could rewrite the keys anyway); viewers get masked ids with
  `id_masked: true`.
- **Fail-closed unchanged** — a declared-but-unresolved token table still locks token
  auth; virtual keys (config plaintext, never `${ENV}`) keep authenticating, scoped.
- **Caveat: virtual keys without tokens.** Reads then require a key (scoped) and reveal
  locks outright (no admin credential can exist, and reveal is the most sensitive read),
  but config *writes* remain unauthenticated — an anonymous caller could edit providers
  or delete keys. Left open deliberately so the dashboard that created the first key
  isn't locked out; the gateway logs a startup warning for this shape. Set an
  `admin_token` in any real deployment.

## Posture

Ready for untrusted clients with `admin_token` set and governance configured. Secret-handling
discipline is solid throughout. Remaining items (#4 timeout, #6, #7) are hardening, not
blockers. Reminder for operators: **set `admin_token` in production** and put the gateway
behind TLS / an ingress you control.
