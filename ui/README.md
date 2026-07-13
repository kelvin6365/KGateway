# KGateway UI

Next.js (App Router) + Tailwind v4 + TanStack Query dashboard for KGateway.

## Dev

```bash
pnpm install
pnpm dev          # http://localhost:3000
```

The UI talks to the gateway's HTTP API. Set the base URL if it isn't the default:

```bash
export NEXT_PUBLIC_KGATEWAY_URL=http://localhost:8080
```

Start the gateway first (`cargo run -p kgateway-server -- --config ../config.json`), then
open the **Playground** to send a live chat completion (streaming or not) through it.

## Status

- **Dashboard** — live gateway health badge + placeholder stat tiles (live metrics: M5/M8).
- **Playground** — fully working chat completion (streaming via SSE + non-streaming).
- **Providers / Virtual Keys / Logs / Cache / MCP / Plugins / Settings** — placeholder
  pages; built out in M8 once the control-plane APIs land (see `../docs/02-roadmap.md`).

## Build

```bash
pnpm build        # production build (used in CI)
pnpm lint
```
