# 05 — Frontend (Next.js dashboard)

The dashboard is built on **Next.js App Router + Tailwind + shadcn/ui**. shadcn is built on Radix primitives.

## Stack

| Concern | Choice |
|---|---|
| Framework | Next.js (App Router, TypeScript) |
| Styling | Tailwind CSS |
| Components | shadcn/ui (Radix primitives) |
| Data fetching | TanStack Query (React Query) against the gateway REST API |
| Forms | `react-hook-form` + `zod` |
| State | URL params first (`nuqs`), local state next, global store only if needed |
| Charts | Recharts |
| Icons | lucide-react |

## Pages

| Route | Purpose | Backend APIs | Milestone |
|---|---|---|---|
| `/` dashboard | usage, cost, latency, token charts; provider breakdown | `/api/logs/stats`, `/metrics` | M8 |
| `/providers` | configure providers + API keys, weights, model lists | `/api/providers` CRUD | M8 |
| `/virtual-keys` | virtual key hierarchy, budgets, rate limits, model access | `/api/governance/vkeys` | M8 |
| `/logs` | live request log, filter by provider/model/vkey, drill into a request | `/api/logs` (+ SSE tail) | M8 |
| `/cache` | semantic cache stats, hit rate, entries | `/api/cache` | M8 |
| `/mcp` | MCP servers, discovered tools, allow-lists, auth | `/api/mcp` | M8 |
| `/plugins` | enable/order plugins, per-plugin config, sequence | `/api/plugins` | M8 |
| `/settings` | global config, env, export/import config.json | `/api/config` | M8 |

## Project structure

```
ui/
├── app/
│   ├── layout.tsx
│   ├── page.tsx                 # dashboard
│   ├── providers/page.tsx
│   ├── virtual-keys/page.tsx
│   ├── logs/page.tsx
│   ├── cache/page.tsx
│   ├── mcp/page.tsx
│   ├── plugins/page.tsx
│   └── settings/page.tsx
├── components/
│   ├── ui/                      # shadcn components
│   └── charts/                  # Recharts wrappers
├── lib/
│   ├── api.ts                   # typed fetch client (base URL configurable)
│   ├── types/                   # shared types + zod schemas (mirror backend schema)
│   └── query.ts                 # TanStack Query setup
└── ...
```

## API contract

The backend (`kgateway-server`) exposes:
- **Data-plane** (OpenAI-compatible): `/v1/chat/completions`, `/v1/embeddings`, `/v1/audio/*`, `/v1/images/*`, `/v1/rerank`, `/v1/batches`, `/v1/files`, `/v1/models`.
- **Control-plane** (dashboard): `/api/providers`, `/api/governance/*`, `/api/logs`, `/api/cache`, `/api/mcp`, `/api/plugins`, `/api/config`, `/metrics`, `/health`.

Types are the source of truth in Rust (`schemars`-generated JSON Schema); the UI mirrors them in `ui/lib/types` with matching `zod` schemas. A later task can auto-generate TS types from the OpenAPI/JSON Schema to prevent drift.

## Notes

- Config can be edited live in the UI — the control-plane APIs write to the store and hot-reload the engine.
- Keep server-only secrets out of the client; the dashboard talks to the gateway's control-plane, which holds credentials.
- Build starts at **M8** but can begin in parallel once M4 stabilizes the control-plane APIs.
