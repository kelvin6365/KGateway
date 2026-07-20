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
| `/playground` | multi-turn chat through the gateway: streaming, per-response latency + tokens, model picker fed by the aggregated listing | `/v1/chat/completions`, `/v1/models` | M8 |
| `/providers` | configure providers + API keys, weights, model lists | `/api/providers` CRUD | M8 |
| `/virtual-keys` | virtual keys: budgets, rate limits, model allow/deny-lists | `/api/config/virtual-keys` (+ `PUT`/`DELETE` by id) | M8 |
| `/logs` | live request log, filter by provider/model/vkey; a full-screen request dialog (URL-addressable via `?request=<id>`) with the **trace waterfall**, captured bodies, and admin reveal | `/api/logs` (+ SSE tail), `/api/logs/{id}` | M8, M25 |
| `/cache` | semantic cache settings + hit rate, derived from the log | `/api/status`, `/api/logs/stats` | M8 |
| `/mcp` | MCP servers and the tools they expose | `/api/mcp/tools` | M8 |
| `/plugins` | which plugins are active in the pipeline, and why | `/api/status` | M8 |
| `/docs` | generated API reference: every endpoint grouped by auth tier, cURL/Python/JS samples, links to the Markdown and llms.txt artifacts | `/openapi.json` | M26 |
| `/settings` | runtime + feature summary, admin token entry | `/api/status`, `/api/whoami` | M8 |

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
- **Control-plane** (dashboard): `/api/logs` (+ `/{id}`, `/stream`, `/stats`, `/histogram`, `/timeseries`, `/rankings`, `/filterdata`, `/dropped`), `/api/providers`, `/api/config/providers`, `/api/config/virtual-keys` (+ `PUT`/`DELETE` by name/id), `/api/mcp/tools`, `/api/status`, `/api/whoami`, `/api/logs/{id}/reveal`, `/metrics`.
- **Docs** (public): `/openapi.json`, `/llms.txt`, `/llms-full.txt`, `/docs/{slug}.md` — the authoritative list, generated from the router; see [16-configuration.md](./16-configuration.md) for `public_url`.

Types are the source of truth in Rust (`schemars`-generated JSON Schema); the UI mirrors them in `ui/lib/types` with matching `zod` schemas. A later task can auto-generate TS types from the OpenAPI/JSON Schema to prevent drift.

## Notes

- Config can be edited live in the UI — the control-plane APIs write to the store and hot-reload the engine.
- Keep server-only secrets out of the client; the dashboard talks to the gateway's control-plane, which holds credentials.
- Build starts at **M8** but can begin in parallel once M4 stabilizes the control-plane APIs.
