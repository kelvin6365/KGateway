# 06 — Deployment

**Docker-first** (from M0). **Helm** later (M9), per your roadmap.

## Docker (primary)

The [`Dockerfile`](../Dockerfile) at the repo root is a multi-stage build (`rust:1.85-slim`
builder → `debian:bookworm-slim` runtime) producing a stripped binary that runs as a non-root
user. No libpq/libsqlite in the runtime image — sqlx bundles SQLite + uses the pure-Rust
Postgres driver and reqwest uses rustls, so only CA certificates are needed. A
[`.dockerignore`](../.dockerignore) keeps `target/` and `ui/` out of the build context.

```bash
# Build
docker build -t kgateway:latest .

# Run: mount your config, pass provider keys + control-plane tokens as env, persist SQLite
docker run --rm -p 8080:8080 \
  -e OPENAI_API_KEY="sk-..." \
  -e KGATEWAY_ADMIN_TOKEN="..." \
  -v "$PWD/config.json:/etc/kgateway/config.json:ro" \
  -v kg-data:/data \
  kgateway:latest
```

The image's `ENTRYPOINT` is `kgateway-server` with a default `--config /etc/kgateway/config.json`.
`GET /health` is unauthenticated for liveness/readiness probes (no in-image `HEALTHCHECK` — the
slim runtime ships no HTTP client; orchestrators probe `/health` directly, and the Helm chart
does).

### docker-compose (dev) — [`docker-compose.yml`](../docker-compose.yml)

```bash
OPENAI_API_KEY=sk-... docker compose up --build
```
Default deploy is **single container + SQLite volume** — zero external dependencies, so it
deploys in seconds. The compose file includes a commented-out Postgres service for scale-out
(stateless pods + shared DB).

Optimizations to add later: `cargo-chef` for dependency-layer caching, distroless runtime,
multi-arch (arm64/amd64) via buildx, and a separate UI image.

## Configuration model

- **`config.json`** — providers, keys, routing, plugin config. Full field reference in
  [`16-configuration.md`](16-configuration.md). Parsed via `serde` derives on the Rust
  `Config` types (`crates/kgateway-server/src/config.rs`).
- **`${ENV_VAR}` interpolation** — placeholders *inside* config string values (keys, tokens,
  database URL) are resolved from the environment at load time, so secrets stay out of the
  file. This fills placeholders; it is not a field-level override of `config.json`.
- **Control-plane API / UI** — live edits (providers, virtual keys) are persisted to
  `config.json` and hot-reloaded without a restart.

## Helm ✅ (M9)

The chart lives in [`charts/kgateway/`](../charts/kgateway) — validated with `helm lint` + `helm template` (renders in both SQLite and Postgres modes):

```
charts/kgateway/
├── Chart.yaml
├── values.yaml           # image, replicas, DB mode, config, resources, HPA, ingress, secrets
└── templates/            # _helpers, deployment, service, configmap, secret, pvc, hpa, ingress, serviceaccount
```

```bash
# SQLite (single replica + PVC — default)
helm install kg charts/kgateway --set secretEnv.OPENAI_API_KEY=sk-...

# Postgres (multi-replica, stateless pods, HPA)
helm install kg charts/kgateway \
  --set database.mode=postgres \
  --set database.url='postgres://user:pass@pg:5432/kgateway' \
  --set replicaCount=3 --set autoscaling.enabled=true
```

- `database.mode` toggles the SQLite PVC path vs a Postgres URL (the server picks `PostgresLogStore` for `postgres://` URLs, else `SqliteLogStore`).
- The `config` value is rendered into a ConfigMap as `config.json`; the DB URL is injected automatically per mode. A `checksum/config` annotation rolls pods on config change.
- Secrets: API keys via `secretEnv` (dev) or `existingSecret` (prod), injected as env and referenced from config via `${VAR}`.
- HPA on CPU. Readiness + liveness = `/health`. Prometheus scrape annotations point at `/metrics`.

## Scaling notes

- **Single node:** SQLite + in-memory rate limiter — fine for most. One replica (SQLite is a local file on the PVC).
- **Multi node:** Postgres store + stateless gateway pods behind a Service + HPA. Follow-on for full multi-node: Redis for shared rate-limit/budget counters + `pgvector` for a shared semantic cache (both in-memory today).
- **Graceful shutdown:** SIGINT/SIGTERM stop accepting new connections and drain in-flight requests (`axum::serve(...).with_graceful_shutdown`) — clean k8s rollouts.

## Performance

`cargo bench -p kgateway-core` (criterion, `benches/hotpath.rs`):

| Path | Overhead |
|---|---|
| Gateway per-request (full pipeline vs instant mock provider) | **~2.8 µs** |
| Weighted key selection (8 keys, model-filtered) | ~97 ns |
| Response serialize / request deserialize | ~323 ns / ~408 ns |
