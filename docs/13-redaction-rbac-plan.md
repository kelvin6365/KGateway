# 13 — Redaction + RBAC (M11)

The security layer that M10 Phase 2 was explicitly deferred against. Content capture now
stores **raw request/response bodies** (`docs/12-content-capture-plan.md`) — secrets and PII
— and the only protection today is a single all-or-nothing `admin_token`: any holder reads
everything via `GET /api/logs/{id}`. This track adds (a) **redaction** so captured bodies
don't sit in the store in the clear, and (b) **RBAC** so "read a log" and "reveal its raw
secrets" are distinct, separately-granted permissions.

## Where we are today
- **Auth**: one `admin_token` (`crates/kgateway-server/src/auth.rs` `require_admin`) gating
  the whole control plane. Binary: you're admin or you're out. Data-plane uses per-request
  virtual keys (governance), unrelated to this.
- **Captured content**: `request_body`/`response_body` on `RequestLog`, written by
  `LoggingPlugin`, stored verbatim, returned by the admin-only detail endpoint.

## The two pieces

### A. Redaction — don't store raw secrets
At capture time, scan the serialized body for sensitive spans and replace them with stable
tokens, keeping a **reversible mapping** so an authorized operator can later un-redact.
- **Detection**: a built-in regex set (emails, JWTs, `sk-`/API-key shapes, AWS access keys,
  bearer tokens, credit-card / phone / IP patterns) + operator-supplied custom patterns from
  config. Each match → a placeholder `⟦REDACTED:<n>⟧`; the (placeholder → original) pairs are
  the mapping for that log.
- **Storage**: the redacted body goes in `request_body`/`response_body` (so a plain read is
  already safe); the mapping is stored **encrypted** (AES-256-GCM with a key from
  `redaction.key` / `${ENV}`) in a new `redaction_mapping` column, decryptable only to
  reveal. No key configured ⇒ redaction still masks, but reveal is unavailable (mapping is
  dropped, not stored in the clear).
- **Applied** in the write path (`LoggingPlugin` / a `Redactor`), off the request hot path
  like the rest of logging. Off unless `redaction.enabled` (but strongly recommended whenever
  `content_logging.enabled`).

### B. RBAC — separate "view" from "reveal"
Replace the single token with a small **token → role → permissions** model, config-driven.
- **Config**: `api_tokens: [{ token, role, name }]`. Backward-compat: an existing top-level
  `admin_token` is treated as one `admin`-role token, so current deployments keep working.
- **Roles → permissions** (fixed set for the MVP):
  | role | permissions |
  |------|-------------|
  | `viewer` | `logs:view`, `metrics:view` |
  | `operator` | viewer + `config:write` |
  | `admin` | operator + `logs:reveal` |
- **Enforcement**: `require_admin` becomes `require_permission(perm)` — the middleware
  resolves the bearer token → role → permission set and checks the route's required
  permission (401 unknown token / 403 insufficient). Routes annotate their permission
  (`GET /api/logs` → `logs:view`, `PUT /api/config/*` → `config:write`, reveal → `logs:reveal`).
- **Reveal**: `GET /api/logs/{id}?reveal=true` (or `/api/logs/{id}/reveal`) requires
  `logs:reveal`; decrypts the mapping and returns the un-redacted body. Every reveal is
  itself audit-logged (who/when/which log id) — revealing secrets is a privileged action.

## Data model
- `RequestLog.redaction_mapping: Option<String>` (encrypted blob; never serialized to any
  client — like the bodies, detail-only and further gated) + a `redacted: bool` flag.
- New column + idempotent migrations (same pattern as M10).
- Reveal is a distinct store call (`get_with_mapping`) so the normal detail path never even
  loads the ciphertext.

## Work breakdown
| # | Area | Files |
|---|------|-------|
| A | Redactor (patterns, tokenize, AES-GCM encrypt/decrypt) | new `kgateway-plugins/src/redaction.rs` (+ `aes-gcm` dep) |
| B | Store: `redaction_mapping`/`redacted` columns + migrations + `get_with_mapping` | `kgateway-store` |
| C | Capture path applies redaction before write | `kgateway-plugins/src/logging.rs` |
| D | RBAC model: `api_tokens` config, token→role→perm resolver, `require_permission` middleware | `kgateway-server/src/{config,auth,app}.rs` |
| E | Reveal endpoint (perm-gated) + reveal audit log | `kgateway-server/src/handlers.rs` |
| F | UI: role-aware controls, "Reveal" action on redacted bodies, reveal audit surfaced | `ui/` |

Shared core (A–E) I own to green, then fan out F (UI) against the frozen contract.

## Testing
Pattern detection (each built-in pattern + custom), reversible round-trip (redact → encrypt →
decrypt → equals original), no-key ⇒ mask-without-mapping, permission matrix (viewer can't
reveal/write, operator can't reveal, admin can), reveal audit emitted, backward-compat
(`admin_token` alone still authorizes admin), and that redacted bodies never leak the raw
value on the non-reveal path.

## Security review resolutions
Two adversarial review passes; the core posture (no un-redacted leak to store/SSE/logs, no
RBAC bypass, correct nonce/AEAD, mapping never serialized to clients) was confirmed. Fixed:
- **Placeholder injection (HIGH).** Reveal used a global `String::replace`, so a body that
  pre-planted a literal `⟦REDACTED:0⟧` could get a real secret substituted into it. Fixed by
  embedding an unguessable per-record random marker in every placeholder
  (`⟦REDACTED:<marker>:i⟧`) — generated after the (attacker-influenceable) input is known and
  stored inside the encrypted mapping — so planted literals can't match.
- **Fail-closed auth (MEDIUM).** If tokens were declared but all `${ENV}` refs resolved empty
  (broken secret injection), the control plane no longer silently opens — it *locks* (enforced,
  every request 401) with a loud startup `error!`. Only a config with no tokens at all runs open.
- **Reveal audit "who" (MEDIUM).** The reveal audit line now records the caller's token
  name/role (`revealed_by`), not just the log id.
- **Defense-in-depth mapping load (LOW).** `get` (detail) no longer loads the ciphertext at
  all; a dedicated `get_with_mapping` (reveal-only) does. So even if the `skip_serializing`
  guard were ever removed, ordinary detail reads still wouldn't carry the mapping.
- **Reveal signal (LOW).** The reveal response now returns per-body `request_revealed` /
  `response_revealed` flags so the UI can distinguish "restored" from "nothing to reveal".
- **Timing comment (LOW).** Corrected to not overclaim constant-time on the token compare.

**Accepted risk / follow-up:** the redaction key is derived with a single unsalted SHA-256
(`redaction.key` is treated as high-entropy key material). If the DB is exfiltrated AND a
low-entropy key was chosen, mappings are brute-forceable cheaply. Hardening to a slow, salted
KDF (Argon2id/PBKDF2 + persisted salt) and key rotation is deferred — **operators must use a
high-entropy `redaction.key`.**

## Out of scope (later)
Per-user accounts / SSO, per-team or per-vkey log scoping, key rotation for the redaction key,
format-preserving redaction, ML/entropy-based detection, redacting *live* proxied traffic (this
track redacts only what's captured into logs).

## Resolved decisions (locked)
1. **Reversible redaction + reveal.** Tokenize, store an AES-256-GCM-encrypted mapping,
   `logs:reveal`-gated un-redaction.
2. **Fixed 3-role model** — `viewer` / `operator` / `admin` with predefined permission sets.
3. **Degrade to masking + warn** when reversible redaction is on but no key is configured —
   still mask, drop the mapping (reveal unavailable), loud startup warning; never blocks boot.
