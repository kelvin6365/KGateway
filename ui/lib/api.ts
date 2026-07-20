// Typed client for the KGateway data-plane + control-plane APIs.
// Base URL is configurable via NEXT_PUBLIC_KGATEWAY_URL (default http://localhost:8080).

export const BASE_URL =
  process.env.NEXT_PUBLIC_KGATEWAY_URL ?? "http://localhost:8080";

// Admin token for control-plane write calls. Persisted in localStorage so it survives
// reloads; only required when the gateway has `admin_token` configured.
const ADMIN_TOKEN_KEY = "kgateway_admin_token";

export function getAdminToken(): string {
  if (typeof window === "undefined") return "";
  return window.localStorage.getItem(ADMIN_TOKEN_KEY) ?? "";
}

export function setAdminToken(token: string): void {
  if (typeof window === "undefined") return;
  if (token) window.localStorage.setItem(ADMIN_TOKEN_KEY, token);
  else window.localStorage.removeItem(ADMIN_TOKEN_KEY);
}

function adminHeaders(): Record<string, string> {
  const token = getAdminToken();
  return token ? { authorization: `Bearer ${token}` } : {};
}

export interface ChatMessage {
  role: "system" | "user" | "assistant" | "tool";
  content: string;
}

export interface ChatRequest {
  model: string;
  messages: ChatMessage[];
  stream?: boolean;
  temperature?: number;
}

export interface ChatResponse {
  id: string;
  model: string;
  choices: {
    index: number;
    message: { role: string; content: string | null };
    finish_reason: string | null;
  }[];
  usage?: { prompt_tokens: number; completion_tokens: number; total_tokens: number };
}

export interface ApiError {
  error: { message: string; type?: string; provider?: string | null };
}

/** A single request log entry from GET /api/logs (newest first). */
export interface RequestLog {
  request_id: string;
  created_at: number; // unix ms
  virtual_key: string | null;
  provider: string;
  model: string;
  status: number;
  prompt_tokens: number;
  completion_tokens: number;
  latency_ms: number;
  cost: number | null;
  stream: boolean;
  cache_hit: boolean;
  stop_reason: string | null;
  error_message: string | null;
  /**
   * Serialized JSON of the captured request body (e.g. chat messages). Only populated by
   * GET /api/logs/{id} (see `getLog`) — the list endpoint always returns null here to stay
   * lean. Also null when content capture is disabled. May end with a `…[truncated]` suffix.
   */
  request_body?: string | null;
  /**
   * Serialized JSON of the captured response body, or (for streamed chat) the accumulated
   * assistant text as a plain string — so this is best-effort JSON, not guaranteed JSON.
   * Same population rules as `request_body`.
   */
  response_body?: string | null;
  /**
   * True when the captured `request_body`/`response_body` have had secrets/PII replaced by
   * `⟦REDACTED:n⟧` placeholders. Present on both list and detail responses. Admins with the
   * `logs:reveal` permission can fetch the originals via `revealLog`.
   */
  redacted: boolean;
}

/** Column the log list can be sorted by (GET /api/logs `sort_by`). */
export type LogSortBy = "created_at" | "latency" | "tokens" | "cost";
export type SortOrder = "asc" | "desc";

/** Query params accepted by GET /api/logs. */
export interface LogQueryParams {
  limit?: number;
  offset?: number;
  sort_by?: LogSortBy;
  order?: SortOrder;
  provider?: string;
  model?: string;
  status?: number;
  virtual_key?: string;
  cache_hit?: boolean;
  since_ms?: number;
  search?: string;
}

/** Filters shared by the list and stats endpoints (no paging/sort). */
export type LogStatsFilters = Omit<LogQueryParams, "limit" | "offset" | "sort_by" | "order">;

/** GET /api/logs response shape. */
export interface LogPage {
  logs: RequestLog[];
  total: number;
}

/** GET /api/logs/stats response shape. */
export interface LogStats {
  total: number;
  success: number;
  errors: number;
  avg_latency_ms: number;
  total_tokens: number;
  total_cost: number;
  cache_hits: number;
}

/** One bucket of GET /api/logs/timeseries `points` (unix ms bucket start). */
export interface TimePoint {
  ts: number;
  count: number;
  errors: number;
}

/** GET /api/logs/timeseries response shape. */
export interface TimeseriesResponse {
  points: TimePoint[];
}

/** Query params accepted by GET /api/logs/timeseries — the shared log filters plus bucketing. */
export interface TimeseriesParams extends LogStatsFilters {
  bucket_ms?: number;
}

/** Metric a distribution histogram can be built over (GET /api/logs/histogram `metric`). */
export type HistogramMetric = "latency" | "cost" | "tokens";

/** One bucket of GET /api/logs/histogram `buckets`. */
export interface HistogramBucket {
  lo: number;
  hi: number;
  count: number;
}

/** GET /api/logs/histogram response shape. */
export interface Histogram {
  metric: string;
  buckets: HistogramBucket[];
  total: number;
}

/** Query params accepted by GET /api/logs/histogram. */
export interface HistogramParams extends LogStatsFilters {
  metric?: HistogramMetric;
  buckets?: number;
}

/** Dimension GET /api/logs/rankings can rank by. */
export type RankBy = "model" | "provider" | "virtual_key";

/** Metric GET /api/logs/rankings can rank/sort by. */
export type RankMetric = "count" | "cost" | "tokens" | "errors";

/** One row of GET /api/logs/rankings `rankings`. */
export interface Rank {
  key: string;
  count: number;
  cost: number;
  tokens: number;
  errors: number;
}

/** GET /api/logs/rankings response shape. */
export interface RankingsResponse {
  rankings: Rank[];
}

/** Query params accepted by GET /api/logs/rankings. */
export interface RankingsParams extends LogStatsFilters {
  by?: RankBy;
  metric?: RankMetric;
  limit?: number;
}

/** GET /api/logs/filterdata response shape — known values for the filter dropdowns. */
export interface FilterData {
  providers: string[];
  models: string[];
  virtual_keys: string[];
}

/** Serializes a params object into a `?a=b&c=d` string, omitting undefined/null/empty values. */
function toQueryString(
  params: Record<string, string | number | boolean | undefined | null>,
): string {
  const sp = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === null || value === "") continue;
    sp.set(key, String(value));
  }
  const s = sp.toString();
  return s ? `?${s}` : "";
}

/** A tool exposed by the MCP gateway (GET /api/mcp/tools). */
export interface McpTool {
  type: "function";
  function: {
    name: string;
    description?: string;
    parameters?: Record<string, unknown>;
  };
}

/** A configured provider + its capabilities (GET /api/providers). */
export interface ProviderSummary {
  name: string;
  capabilities: string[];
  key_count: number;
}

/** Parsed view of the Prometheus text exposed at GET /metrics. */
export interface Metrics {
  requestsTotal: number;
  byStatus: Record<string, number>;
  latencyMsSum: number;
  latencyCount: number;
  avgLatencyMs: number;
}

/** GET /health — returns true if the gateway is reachable and healthy. */
export async function health(): Promise<boolean> {
  const res = await fetch(`${BASE_URL}/health`, { cache: "no-store" });
  if (!res.ok) return false;
  const body = await res.json().catch(() => ({}));
  return body?.status === "ok";
}

/** POST /v1/chat/completions (non-streaming). Throws the gateway error message. */
export async function chatCompletion(
  req: ChatRequest,
  signal?: AbortSignal,
): Promise<ChatResponse> {
  const res = await fetch(`${BASE_URL}/v1/chat/completions`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ ...req, stream: false }),
    signal,
  });
  if (!res.ok) {
    const err = (await res.json().catch(() => null)) as ApiError | null;
    throw new Error(err?.error?.message ?? `HTTP ${res.status}`);
  }
  return res.json();
}

/**
 * POST /v1/chat/completions with stream:true. Reads the SSE body, parses each
 * `data: {json}` line, and invokes `onChunk` with each content delta. Resolves when
 * the stream ends (`data: [DONE]`).
 */
export async function chatCompletionStream(
  req: ChatRequest,
  onChunk: (delta: string) => void,
  signal?: AbortSignal,
): Promise<void> {
  const res = await fetch(`${BASE_URL}/v1/chat/completions`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ ...req, stream: true }),
    signal,
  });
  if (!res.ok || !res.body) {
    const err = (await res.json().catch(() => null)) as ApiError | null;
    throw new Error(err?.error?.message ?? `HTTP ${res.status}`);
  }

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });

    // Split on SSE frame boundaries.
    let sep: number;
    while ((sep = buffer.indexOf("\n\n")) !== -1) {
      const frame = buffer.slice(0, sep);
      buffer = buffer.slice(sep + 2);
      for (const line of frame.split("\n")) {
        const trimmed = line.trimStart();
        if (!trimmed.startsWith("data:")) continue;
        const data = trimmed.slice(5).trim();
        if (data === "[DONE]") return;
        try {
          const parsed = JSON.parse(data) as {
            choices?: { delta?: { content?: string } }[];
          };
          const delta = parsed.choices?.[0]?.delta?.content;
          if (delta) onChunk(delta);
        } catch {
          // ignore malformed keep-alive / partial frames
        }
      }
    }
  }
}

/** GET /api/logs — paged, filterable, sortable request logs. */
export async function getLogs(params: LogQueryParams = {}): Promise<LogPage> {
  const qs = toQueryString({ ...params });
  const res = await fetch(`${BASE_URL}/api/logs${qs}`, {
    cache: "no-store",
    headers: adminHeaders(),
  });
  if (res.status === 401) throw new Error("admin token required");
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  const body = (await res.json().catch(() => ({}))) as Partial<LogPage>;
  return { logs: body.logs ?? [], total: body.total ?? 0 };
}

/** GET /api/logs/{id} — a single request log. Throws if not found (404). */
export async function getLog(id: string): Promise<RequestLog> {
  const res = await fetch(`${BASE_URL}/api/logs/${encodeURIComponent(id)}`, {
    cache: "no-store",
    headers: adminHeaders(),
  });
  if (res.status === 401) throw new Error("admin token required");
  if (res.status === 404) throw new Error("log not found");
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return res.json();
}

/** GET /api/whoami response shape — the current token's role + granted permissions. */
export interface Whoami {
  role: "viewer" | "operator" | "admin" | string;
  permissions: string[];
}

/** GET /api/whoami — role + permissions for the current admin token. */
export async function getWhoami(): Promise<Whoami> {
  const res = await fetch(`${BASE_URL}/api/whoami`, {
    cache: "no-store",
    headers: adminHeaders(),
  });
  if (res.status === 401) throw new Error("admin token required");
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return res.json();
}

/** GET /api/logs/{id}/reveal response shape — original (unredacted) bodies. */
export interface RevealedLog {
  request_id: string;
  request_body: string | null;
  response_body: string | null;
}

/**
 * GET /api/logs/{id}/reveal — the original, unredacted bodies for a log (admin only,
 * requires the `logs:reveal` permission). Throws a descriptive error on 403 (not
 * permitted), 400 (redaction not enabled on the gateway), and 404 (log not found).
 */
export async function revealLog(id: string): Promise<RevealedLog> {
  const res = await fetch(
    `${BASE_URL}/api/logs/${encodeURIComponent(id)}/reveal`,
    { cache: "no-store", headers: adminHeaders() },
  );
  if (res.status === 401) throw new Error("admin token required");
  if (res.status === 403) throw new Error("reveal requires admin");
  if (res.status === 400) throw new Error("redaction not enabled");
  if (res.status === 404) throw new Error("log not found");
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return res.json();
}

/** GET /api/logs/dropped — count of logs shed under writer backpressure (admin-only). */
export async function getDroppedCount(): Promise<number> {
  const res = await fetch(`${BASE_URL}/api/logs/dropped`, {
    cache: "no-store",
    headers: adminHeaders(),
  });
  if (res.status === 401) throw new Error("admin token required");
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  const body = (await res.json().catch(() => ({}))) as { dropped?: number };
  return body.dropped ?? 0;
}

/** GET /api/logs/stats — aggregate counters for the given filters. */
export async function getLogStats(filters: LogStatsFilters = {}): Promise<LogStats> {
  const qs = toQueryString({ ...filters });
  const res = await fetch(`${BASE_URL}/api/logs/stats${qs}`, {
    cache: "no-store",
    headers: adminHeaders(),
  });
  if (res.status === 401) throw new Error("admin token required");
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return res.json();
}

/** GET /api/logs/timeseries — bucketed request/error counts for the given filters. */
export async function getTimeseries(
  params: TimeseriesParams = {},
): Promise<TimeseriesResponse> {
  const qs = toQueryString({ ...params });
  const res = await fetch(`${BASE_URL}/api/logs/timeseries${qs}`, {
    cache: "no-store",
    headers: adminHeaders(),
  });
  if (res.status === 401) throw new Error("admin token required");
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  const body = (await res.json().catch(() => ({}))) as Partial<TimeseriesResponse>;
  return { points: body.points ?? [] };
}

/** GET /api/logs/histogram — distribution of latency/cost/tokens for the given filters. */
export async function getHistogram(params: HistogramParams = {}): Promise<Histogram> {
  const qs = toQueryString({ ...params });
  const res = await fetch(`${BASE_URL}/api/logs/histogram${qs}`, {
    cache: "no-store",
    headers: adminHeaders(),
  });
  if (res.status === 401) throw new Error("admin token required");
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  const body = (await res.json().catch(() => ({}))) as Partial<Histogram>;
  return {
    metric: body.metric ?? params.metric ?? "latency",
    buckets: body.buckets ?? [],
    total: body.total ?? 0,
  };
}

/** GET /api/logs/rankings — top models/providers/virtual-keys by a chosen metric. */
export async function getRankings(
  params: RankingsParams = {},
): Promise<RankingsResponse> {
  const qs = toQueryString({ ...params });
  const res = await fetch(`${BASE_URL}/api/logs/rankings${qs}`, {
    cache: "no-store",
    headers: adminHeaders(),
  });
  if (res.status === 401) throw new Error("admin token required");
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  const body = (await res.json().catch(() => ({}))) as Partial<RankingsResponse>;
  return { rankings: body.rankings ?? [] };
}

/** GET /api/logs/filterdata — known providers/models/virtual-keys, for filter dropdowns. */
export async function getFilterData(): Promise<FilterData> {
  const res = await fetch(`${BASE_URL}/api/logs/filterdata`, {
    cache: "no-store",
    headers: adminHeaders(),
  });
  if (res.status === 401) throw new Error("admin token required");
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  const body = (await res.json().catch(() => ({}))) as Partial<FilterData>;
  return {
    providers: body.providers ?? [],
    models: body.models ?? [],
    virtual_keys: body.virtual_keys ?? [],
  };
}

/**
 * Builds the URL for the live log tail SSE endpoint. Browser `EventSource` cannot set
 * request headers, so the admin token travels as a URL-encoded `?token=` query param.
 */
export function logStreamUrl(token: string): string {
  return `${BASE_URL}/api/logs/stream?token=${encodeURIComponent(token)}`;
}

/** GET /api/mcp/tools — MCP tools registered on the gateway. */
export async function getMcpTools(): Promise<McpTool[]> {
  const res = await fetch(`${BASE_URL}/api/mcp/tools`, { cache: "no-store" });
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  const body = (await res.json().catch(() => ({}))) as { tools?: McpTool[] };
  return body.tools ?? [];
}

/** GET /api/providers — configured providers + their capabilities. */
/** One entry from the aggregated `GET /v1/models` listing (OpenAI list format). */
export interface ListedModel {
  id: string; // routable "provider/model"
  owned_by: string; // provider name
  created: number;
}

/**
 * Aggregated model list — the gateway fans out to each configured provider's
 * official list-models API and returns routable `provider/model` ids. Data-plane:
 * no admin token needed (a virtual key is required only in strict mode).
 */
export async function getModels(): Promise<ListedModel[]> {
  const res = await fetch(`${BASE_URL}/v1/models`, { cache: "no-store" });
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  const body = (await res.json().catch(() => ({}))) as { data?: ListedModel[] };
  return body.data ?? [];
}

export async function getProviders(): Promise<ProviderSummary[]> {
  const res = await fetch(`${BASE_URL}/api/providers`, {
    cache: "no-store",
    headers: adminHeaders(),
  });
  if (res.status === 401) throw new Error("admin token required");
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  const body = (await res.json().catch(() => ({}))) as {
    providers?: ProviderSummary[];
  };
  return body.providers ?? [];
}

/** A provider config for the write API (PUT /api/config/providers/:name). */
export interface ProviderConfigInput {
  kind?: string; // "openai" | "anthropic" | undefined (infer from name)
  base_url?: string;
  keys: { id: string; value: string; weight: number; models?: string[] }[];
}

/** PUT /api/config/providers/{name} — create or update a provider (persists + reloads). */
export async function putProvider(
  name: string,
  config: ProviderConfigInput,
): Promise<void> {
  const res = await fetch(
    `${BASE_URL}/api/config/providers/${encodeURIComponent(name)}`,
    {
      method: "PUT",
      headers: { "content-type": "application/json", ...adminHeaders() },
      body: JSON.stringify(config),
    },
  );
  if (!res.ok) {
    const err = (await res.json().catch(() => null)) as {
      error?: { message?: string };
    } | null;
    throw new Error(err?.error?.message ?? `HTTP ${res.status}`);
  }
}

/** DELETE /api/config/providers/{name} — remove a provider (persists + reloads). */
export async function deleteProvider(name: string): Promise<void> {
  const res = await fetch(
    `${BASE_URL}/api/config/providers/${encodeURIComponent(name)}`,
    { method: "DELETE", headers: adminHeaders() },
  );
  if (!res.ok) {
    const err = (await res.json().catch(() => null)) as {
      error?: { message?: string };
    } | null;
    throw new Error(err?.error?.message ?? `HTTP ${res.status}`);
  }
}

/** A virtual key config (GET /api/config/virtual-keys). */
export interface VirtualKey {
  id: string;
  name: string;
  allowed_models: string[];
  /** Denied models — always rejected, wins over the allow-list. */
  denied_models?: string[];
  max_requests_per_min?: number | null;
  max_total_tokens?: number | null;
  /** Max estimated USD cost per rolling period. */
  max_cost_per_period?: number | null;
  /** Length of the cost-budget period, in seconds (defaults to 60 when a cost cap is set). */
  max_cost_period_secs?: number | null;
}

export type VirtualKeyInput = Omit<VirtualKey, "id">;

/** GET /api/config/virtual-keys — configured virtual keys (admin-only). */
export async function getVirtualKeys(): Promise<VirtualKey[]> {
  const res = await fetch(`${BASE_URL}/api/config/virtual-keys`, {
    cache: "no-store",
    headers: adminHeaders(),
  });
  if (res.status === 401) throw new Error("admin token required");
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  const body = (await res.json().catch(() => ({}))) as {
    virtual_keys?: VirtualKey[];
  };
  return body.virtual_keys ?? [];
}

/** PUT /api/config/virtual-keys/{id} — create/update a virtual key (persists + reloads). */
export async function putVirtualKey(
  id: string,
  input: VirtualKeyInput,
): Promise<void> {
  const res = await fetch(
    `${BASE_URL}/api/config/virtual-keys/${encodeURIComponent(id)}`,
    {
      method: "PUT",
      headers: { "content-type": "application/json", ...adminHeaders() },
      body: JSON.stringify(input),
    },
  );
  if (!res.ok) {
    const err = (await res.json().catch(() => null)) as {
      error?: { message?: string };
    } | null;
    throw new Error(err?.error?.message ?? `HTTP ${res.status}`);
  }
}

/** DELETE /api/config/virtual-keys/{id} — remove a virtual key (persists + reloads). */
export async function deleteVirtualKey(id: string): Promise<void> {
  const res = await fetch(
    `${BASE_URL}/api/config/virtual-keys/${encodeURIComponent(id)}`,
    { method: "DELETE", headers: adminHeaders() },
  );
  if (!res.ok) {
    const err = (await res.json().catch(() => null)) as {
      error?: { message?: string };
    } | null;
    throw new Error(err?.error?.message ?? `HTTP ${res.status}`);
  }
}

/**
 * Parse the Prometheus exposition text at /metrics into a typed Metrics object.
 *
 * Handles lines of the form `metric_name{label="value",...} 123`, ignoring
 * blank lines and `#` HELP/TYPE comment lines. Unknown metrics are skipped.
 */
export function parseMetrics(text: string): Metrics {
  const byStatus: Record<string, number> = {};
  let requestsTotal = 0;
  let latencyMsSum = 0;
  let latencyCount = 0;

  // name, optional {labels}, then a numeric value.
  const line = /^([a-zA-Z_:][a-zA-Z0-9_:]*)(\{[^}]*\})?\s+([0-9eE.+-]+)\s*$/;

  for (const raw of text.split("\n")) {
    const trimmed = raw.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const m = line.exec(trimmed);
    if (!m) continue;
    const [, name, labels, valueStr] = m;
    const value = Number(valueStr);
    if (!Number.isFinite(value)) continue;

    switch (name) {
      case "kgateway_requests_total":
        requestsTotal = value;
        break;
      case "kgateway_requests_by_status": {
        const status = /status="([^"]*)"/.exec(labels ?? "")?.[1];
        if (status) byStatus[status] = value;
        break;
      }
      case "kgateway_request_latency_ms_sum":
        latencyMsSum = value;
        break;
      case "kgateway_request_latency_ms_count":
        latencyCount = value;
        break;
    }
  }

  return {
    requestsTotal,
    byStatus,
    latencyMsSum,
    latencyCount,
    avgLatencyMs: latencyCount ? latencyMsSum / latencyCount : 0,
  };
}

/** GET /metrics — fetches Prometheus text and parses it into Metrics. */
export async function getMetrics(): Promise<Metrics> {
  const res = await fetch(`${BASE_URL}/metrics`, { cache: "no-store" });
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  const text = await res.text();
  return parseMetrics(text);
}

/** Semantic-cache config summary, present on `Status` only when the feature is configured. */
export interface SemanticCacheStatus {
  embedding_provider: string;
  embedding_model: string;
  threshold: number;
}

/** Feature toggles reported by GET /api/status. */
export interface StatusFeatures {
  content_logging: boolean;
  redaction: boolean;
  semantic_cache: boolean;
  governance: boolean;
  mcp: boolean;
  otlp: boolean;
}

/** A request-pipeline stage (observer/plugin) reported by GET /api/status. */
export interface PluginStatus {
  name: string;
  description: string;
  enabled: boolean;
}

/** GET /api/status response shape — non-secret runtime + config summary for the dashboard. */
export interface Status {
  version: string;
  port: number;
  database: "memory" | "sqlite" | "postgres";
  auth: "enabled" | "open";
  log_retention_days: number | null;
  request_timeout_secs: number;
  cors_allow_origins: string[] | null;
  providers: string[];
  virtual_keys_count: number;
  semantic_cache: SemanticCacheStatus | null;
  redaction_reveal: boolean;
  features: StatusFeatures;
  plugins: PluginStatus[];
}

/** GET /api/status — non-secret runtime + config summary (feature flags, active plugins,
 *  DB mode, semantic-cache settings). Feeds the Cache / Plugins / Settings pages. */
export async function getStatus(): Promise<Status> {
  const res = await fetch(`${BASE_URL}/api/status`, {
    cache: "no-store",
    headers: adminHeaders(),
  });
  if (res.status === 401) throw new Error("admin token required");
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return res.json();
}
