# 14 — Analytics: histograms, rankings, timeseries, filter-data (M12)

The remaining M10 full-parity observability work: turn the captured request logs into
distributions, rankings, and time series that a dashboard can chart — plus the
distinct-value lists that populate the filter sidebar's dropdowns. Builds directly on the
M10 logs platform (`RequestLog`, `LogFilter`, `LogStats`).

## Approach
All aggregations are computed **in Rust over the filtered scan window** (`recent(N)` +
`LogFilter::matches`), exactly like the existing `LogStore::stats`/`query` default impls —
backend-agnostic, works on Memory/SQLite/Postgres out of the box. Pushing these down to SQL
(`GROUP BY`, `width_bucket`, `date_trunc`) is a noted optimization, not a blocker. Every
endpoint accepts the same `LogFilter` query params as `/api/logs`, so charts respect the
active filters.

## Store additions (`kgateway-store`)
New types + `LogStore` trait methods (default impls scan + fold in Rust):
- `histogram(filter, metric, buckets) -> Histogram` — distribution of `latency_ms` | `cost` |
  `total_tokens` into N linear buckets (`{ lo, hi, count }[]`) between the observed min/max.
- `timeseries(filter, bucket_ms) -> Vec<TimePoint>` — `{ ts, count, errors }` bucketed by
  `created_at`, for a requests/errors-over-time chart.
- `rankings(filter, dimension, metric, limit) -> Vec<Rank>` — top-N by `model` | `provider` |
  `virtual_key`, scored by `count` | `cost` | `tokens` | `errors` (`{ key, count, cost,
  tokens, errors }`).
- `filter_values() -> FilterData` — distinct `providers` / `models` / `virtual_keys` present,
  for the UI filter dropdowns.

## Server API (`kgateway-server`, all `logs:view`)
- `GET /api/logs/histogram?metric=&buckets=&<filters>` → `Histogram`
- `GET /api/logs/timeseries?bucket_ms=&<filters>` → `{ points: TimePoint[] }`
- `GET /api/logs/rankings?by=&metric=&limit=&<filters>` → `{ rankings: Rank[] }`
- `GET /api/logs/filterdata` → `FilterData`

All in the existing `view_group` (RBAC `logs:view`), matchit-safe alongside `/api/logs/{id}`
(distinct static segments).

## UI (`ui/app/logs`)
An **Analytics** panel/tab on the logs page: a requests-over-time area chart (timeseries), a
latency/cost histogram (selectable metric), and top-models / top-providers ranking tables —
all driven by the current filters. Filter dropdowns populated from `filterdata`. Charts drawn
with lightweight inline SVG (no external chart dep — keeps the bundle self-contained), matching
the existing dashboard style.

## Testing
Bucketing edge cases (empty set, single value, all-equal → one bucket), ranking order + tie
handling, timeseries bucket alignment, filter-data distinctness, and that every endpoint honors
`LogFilter`. Plus a live smoke over seeded logs.

## Out of scope (later)
Percentile (p50/p95/p99) summaries, by-dimension cross-tabs (`/histogram/by-provider`),
cost-recalculation jobs, saved/scheduled reports, CSV export.
