# 08 — Getting Started

A 5-minute guide to running KGateway locally, sending your first request, and opening the dashboard.

## Prerequisites

- **Rust** 1.85+ (`rustc --version`)
- **Node** 22+ and **pnpm** (for the dashboard)
- A provider API key (e.g. `OPENAI_API_KEY`)
- Optional: **Docker** / **Helm** for containerized deploys

## 1. Configure

Copy the example config and set your key(s) via environment variables (config references them as `${VAR}` — never hard-code secrets):

```bash
cp config.example.json config.json      # config.json is gitignored
export OPENAI_API_KEY=sk-...
# optional extras
export ANTHROPIC_API_KEY=...
export GROQ_API_KEY=...
```

`config.json` (trimmed):

```json
{
  "port": 8080,
  "database": "sqlite://./kgateway.db?mode=rwc",
  "providers": {
    "openai":    { "keys": [ { "id": "default", "value": "${OPENAI_API_KEY}", "weight": 1 } ] },
    "anthropic": { "keys": [ { "id": "default", "value": "${ANTHROPIC_API_KEY}", "weight": 1 } ] }
  }
}
```

This is the minimal shape. See [16-configuration.md](./16-configuration.md) for the **complete
config reference** — every field, type, and default (virtual keys, semantic cache, MCP, content
capture, redaction, RBAC tokens, retention, CORS, timeouts).

## 2. Run the gateway

```bash
cargo run -p kgateway-server -- --config config.json
# → kgateway listening on 0.0.0.0:8080
```

## 3. Send a request

Everything is OpenAI-compatible. Route with the `provider/model` convention:

```bash
# non-streaming
curl -s http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"openai/gpt-4o","messages":[{"role":"user","content":"Say hi in 3 words."}]}'

# streaming (SSE)
curl -N http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"openai/gpt-4o","messages":[{"role":"user","content":"Count to 5."}],"stream":true}'

# switch providers by changing the prefix — same request shape
#   anthropic/claude-3-5-sonnet,  groq/llama-3.1-70b,  ollama/llama3, ...
```

Other endpoints:

```bash
# embeddings
curl -s localhost:8080/v1/embeddings -H 'content-type: application/json' \
  -d '{"model":"openai/text-embedding-3-small","input":["hello"]}'

# rerank (Cohere)
curl -s localhost:8080/v1/rerank -H 'content-type: application/json' \
  -d '{"model":"cohere/rerank-v3.5","query":"cats","documents":["dogs","cats are great"]}'

# image generation
curl -s localhost:8080/v1/images/generations -H 'content-type: application/json' \
  -d '{"model":"openai/dall-e-3","prompt":"a red bicycle"}'
```

## 3b. Use with Claude Code (Anthropic ingress)

KGateway also exposes an **Anthropic Messages** endpoint (`POST /v1/messages`), so Anthropic
clients like **Claude Code** can route *through* the gateway to any provider — governance,
logging, failover, and caching all apply. Streaming and tool use are fully translated.

1. Configure the target provider in `config.json` (e.g. z.ai's GLM Coding Plan over the Anthropic
   protocol):

   ```json
   "zai": {
     "kind": "anthropic",
     "base_url": "https://api.z.ai/api/anthropic",
     "keys": [{ "id": "coding-plan", "value": "${ZAI_API_KEY}", "weight": 1 }]
   }
   ```

2. Run the gateway (`cargo run -p kgateway-server -- --config config.json`), then point Claude
   Code at it:

   ```bash
   export ANTHROPIC_BASE_URL=http://localhost:8080     # KGateway; it appends /v1/messages
   export ANTHROPIC_AUTH_TOKEN=local                   # any token; a virtual key if governance is on
   export ANTHROPIC_MODEL="zai/glm-4.6"                # routes to the zai provider
   export ANTHROPIC_SMALL_FAST_MODEL="zai/glm-4.6"     # background model
   claude
   ```

   …or, per-project, in `.claude/settings.json`:

   ```json
   {
     "model": "zai/glm-4.6",
     "env": {
       "ANTHROPIC_BASE_URL": "http://localhost:8080",
       "ANTHROPIC_AUTH_TOKEN": "local"
     }
   }
   ```

   The `provider/model` prefix picks the route (`zai/glm-4.6`, `openai/gpt-4o`, …). Your real
   upstream key lives in KGateway's config — Claude Code only sends the dummy/virtual token. Watch
   requests flow in the dashboard **Logs** page.

   **Two traps:**

   - **`ANTHROPIC_BASE_URL` is a base, not the endpoint.** Claude Code appends `/v1/messages`
     itself, so set `http://localhost:8080` — not `http://localhost:8080/v1/messages`, which
     resolves to `/v1/messages/v1/messages` and 404s.
   - **Set the model, not just the aliases.** `ANTHROPIC_DEFAULT_SONNET_MODEL` /
     `_OPUS_MODEL` / `_HAIKU_MODEL` only remap the *sonnet* / *opus* / *haiku* aliases. If Claude
     Code is on any other model, none of them apply and the literal Claude model name reaches the
     gateway; with no `provider/` prefix it routes to the default `openai` provider and fails with
     `400 invalid request`. Use `model` / `ANTHROPIC_MODEL` to force the route.

## 4. Beyond the basics (optional)

Everything below is opt-in config — see [16-configuration.md](./16-configuration.md) for the
full field-by-field reference. Highlights:

- **Failover** — per-request fallbacks retried on a retryable upstream error (send `"fallbacks": [{ "provider": "anthropic", "model": "claude-3-5-sonnet" }]` in the request body).
- **Governance** — `virtual_keys` give clients an `Authorization: Bearer <id>` with model allow/deny-lists, request rate limits, and token + USD cost budgets. The first key flips the gateway to strict mode; with a Postgres `database`, counters are shared across replicas.
- **Semantic cache** — `semantic_cache` short-circuits repeat prompts by embedding similarity.
- **MCP tool-calling** — `mcp.builtin_tools` (demo `echo` tool) and `mcp.servers[]` (external stdio servers) for agentic tool use.
- **Logs & content capture** — persist request audit rows to `database`; opt into raw request/response body capture with `content_logging`, bounded and admin-only.
- **Redaction** — `redaction` reversibly masks secrets/PII in captured bodies (AES-GCM), revealable by an admin.
- **RBAC** — `admin_token` (legacy admin) and/or `api_tokens[]` bind bearer tokens to `viewer` / `operator` / `admin` roles for the control plane.
- **Retention / CORS / timeout** — `log_retention_days`, `cors_allow_origins`, `request_timeout_secs`.

### Hot-reload

Edit `config.json` and send `SIGHUP` — the gateway rebuilds the engine (providers,
virtual keys, cache, MCP servers) **without a restart or dropped requests**:

```bash
kill -HUP $(pgrep -f kgateway-server)   # port + admin_token still require a restart
```

## 5. Dashboard

```bash
cd ui
pnpm install
NEXT_PUBLIC_KGATEWAY_URL=http://localhost:8080 pnpm dev
# → http://localhost:3000
```

Pages: **Dashboard** (live metrics), **Playground** (send chats), **Logs** (filterable request
audit with SSE live tail, detail drawer, and — for admins — a **Reveal** button that un-redacts
captured bodies), **Analytics** (requests-over-time, latency/cost/token histograms, top model/
provider rankings), **MCP** (tools), **Providers** and **Virtual Keys** (live add/edit/remove).

## 6. Observability & control plane

The `/api/*` control plane and `/metrics` power the dashboard and are scriptable directly. When
`admin_token` or `api_tokens` are set they require `Authorization: Bearer <token>` (see the
[RBAC roles](./16-configuration.md#apitokenconfig--role)).

```bash
curl localhost:8080/metrics                 # Prometheus text
curl localhost:8080/api/logs                 # filtered/paginated request audit log (JSON)
curl localhost:8080/api/logs/{id}            # single-request detail (incl. captured bodies)
curl localhost:8080/api/logs/stats           # aggregate stats
curl localhost:8080/api/logs/timeseries      # analytics: requests/errors over time
curl localhost:8080/api/logs/{id}/reveal     # un-redact bodies (admin / logs:reveal only)
curl localhost:8080/api/providers            # configured providers + capabilities
curl localhost:8080/api/mcp/tools            # discovered MCP tools
curl localhost:8080/health                   # liveness
```

## 7. Docker

```bash
docker compose up --build
```

## 8. Kubernetes (Helm)

```bash
# SQLite (single replica + PVC)
helm install kg charts/kgateway --set secretEnv.OPENAI_API_KEY=sk-...

# Postgres (multi-replica + HPA)
helm install kg charts/kgateway \
  --set database.mode=postgres \
  --set database.url='postgres://user:pass@pg:5432/kgateway' \
  --set replicaCount=3 --set autoscaling.enabled=true \
  --set secretEnv.OPENAI_API_KEY=sk-...
```

## Development

```bash
cargo test --workspace                                    # unit + integration tests
cargo clippy --workspace --all-targets -- -D warnings     # lint
cargo fmt --all --check                                   # format
cargo bench -p kgateway-core                              # hot-path benchmarks
```

## Troubleshooting

- **401 on every request** — `virtual_keys` are configured (strict mode); send `Authorization: Bearer <vk_id>`.
- **`unknown provider`** — the model prefix isn't a configured provider; check `config.json` `providers` and the `provider/model` string.
- **Claude Code: `400 invalid request`** — Claude Code is sending its own model name (e.g. a
  `claude-*` id) because only the `ANTHROPIC_DEFAULT_*_MODEL` aliases were set. Unprefixed models
  route to the default `openai` provider. Set `model` / `ANTHROPIC_MODEL` to `zai/glm-4.6` (see §3b).
- **Claude Code: 404 on every request** — `ANTHROPIC_BASE_URL` includes `/v1/messages`; drop it (see §3b).
- **`no eligible API keys`** — the key's `models` allow-list excludes the requested model, or `${ENV}` var is unset.
- **`operation not supported`** — that provider lacks the capability (e.g. rerank on OpenAI); use a provider that supports it (Cohere for rerank).
