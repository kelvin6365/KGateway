"use client";

import { Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { usePathname, useRouter, useSearchParams } from "next/navigation";
import { useQuery } from "@tanstack/react-query";
import { ArrowDown, ArrowUp, Check, Copy, Radio } from "lucide-react";
import {
  getLogs,
  getLog,
  getLogStats,
  getDroppedCount,
  getWhoami,
  revealLog,
  logStreamUrl,
  getAdminToken,
  setAdminToken,
  getFilterData,
  type RequestLog,
  type LogQueryParams,
  type LogStatsFilters,
  type LogSortBy,
  type SortOrder,
} from "@/lib/api";
import { cn } from "@/lib/utils";
import { statusColor } from "@/lib/status";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { Switch } from "@/components/ui/switch";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { Skeleton } from "@/components/ui/skeleton";
import { EmptyState } from "@/components/baroque/empty-state";
import { TraceWaterfall } from "@/components/trace-waterfall";
import { AnalyticsPanel, type TimeRange } from "./analytics";

const LIVE_BUFFER_CAP = 200;

type CacheFilter = "all" | "hit" | "miss";
type View = "logs" | "analytics";

const VIEWS: { value: View; label: string }[] = [
  { value: "logs", label: "Logs" },
  { value: "analytics", label: "Analytics" },
];

const TIME_RANGES: { value: TimeRange; label: string }[] = [
  { value: "15m", label: "Last 15m" },
  { value: "1h", label: "Last 1h" },
  { value: "24h", label: "Last 24h" },
  { value: "all", label: "All" },
];

function timeRangeToSinceMs(range: TimeRange): number | undefined {
  const now = Date.now();
  switch (range) {
    case "15m":
      return now - 15 * 60 * 1000;
    case "1h":
      return now - 60 * 60 * 1000;
    case "24h":
      return now - 24 * 60 * 60 * 1000;
    case "all":
    default:
      return undefined;
  }
}

function useDebouncedValue<T>(value: T, delayMs: number): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const t = setTimeout(() => setDebounced(value), delayMs);
    return () => clearTimeout(t);
  }, [value, delayMs]);
  return debounced;
}

function formatCost(cost: number | null | undefined): string {
  if (cost === null || cost === undefined) return "—";
  return `$${cost.toFixed(4)}`;
}

function formatTime(ms: number): string {
  return new Date(ms).toLocaleString();
}

function StatTile({ label, value, note }: { label: string; value: string; note?: string }) {
  return (
    <Card>
      <CardContent>
        <div className="text-xs uppercase tracking-wide text-muted-foreground">{label}</div>
        <div className="mt-2 text-2xl font-semibold">{value}</div>
        {note && <div className="mt-1 text-xs text-muted-foreground">{note}</div>}
      </CardContent>
    </Card>
  );
}

function MiniBadge({ children, tone = "muted" }: { children: React.ReactNode; tone?: "muted" | "accent" }) {
  return (
    <span
      className={cn(
        "inline-flex items-center rounded-full border border-border px-2 py-0.5 text-[10px] font-medium uppercase tracking-wide",
        tone === "accent" ? "text-primary" : "text-muted-foreground"
      )}
    >
      {children}
    </span>
  );
}

const TRUNCATED_SUFFIX = "…[truncated]";

/** True if a captured body was cut short by the backend's size cap. */
function isTruncated(body: string): boolean {
  return body.endsWith(TRUNCATED_SUFFIX);
}

/** Best-effort pretty-print: valid JSON is re-indented, anything else is returned as-is
 *  (e.g. accumulated streamed assistant text, which is plain text rather than JSON). */
function prettyPrintBody(body: string): string {
  try {
    return JSON.stringify(JSON.parse(body), null, 2);
  } catch {
    return body;
  }
}

/**
 * A "Request" or "Response" captured-content section in the log detail drawer. Renders a
 * Pretty/Raw toggle, a copy-to-clipboard button, a truncated badge, and a calm empty state
 * when no content was captured (capture disabled, or this capability doesn't capture it).
 */
function ContentSection({
  title,
  body,
  loading,
  error,
  streamHint,
}: {
  title: string;
  body: string | null | undefined;
  loading: boolean;
  error?: string;
  /** When true, the empty state hints that streamed responses need `capture_streaming`. */
  streamHint?: boolean;
}) {
  const [mode, setMode] = useState<"pretty" | "raw">("pretty");
  const [copied, setCopied] = useState(false);

  async function handleCopy() {
    if (!body) return;
    try {
      await navigator.clipboard.writeText(body);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard API unavailable (e.g. insecure context) — nothing sensible to do.
    }
  }

  const truncated = !!body && isTruncated(body);
  // Memoized: unbounded capture can yield multi-hundred-KB bodies, and re-parsing
  // them on every render would jank the drawer.
  const displayText = useMemo(
    () => (body ? (mode === "raw" ? body : prettyPrintBody(body)) : ""),
    [body, mode]
  );

  return (
    <div className="flex flex-col gap-2">
      <div className="flex items-center justify-between gap-2">
        <div className="flex items-center gap-2">
          <span className="text-sm font-semibold">{title}</span>
          {truncated && <MiniBadge>truncated</MiniBadge>}
        </div>
        {body && (
          <div className="flex items-center gap-2">
            <ToggleGroup
              type="single"
              value={mode}
              onValueChange={(v) => {
                if (v) setMode(v as "pretty" | "raw");
              }}
              variant="outline"
              size="sm"
              spacing={0}
              className="overflow-hidden rounded-md"
            >
              <ToggleGroupItem value="pretty" className="px-2 py-1 text-xs capitalize">
                Pretty
              </ToggleGroupItem>
              <ToggleGroupItem value="raw" className="px-2 py-1 text-xs capitalize">
                Raw
              </ToggleGroupItem>
            </ToggleGroup>
            <button
              onClick={handleCopy}
              className="inline-flex items-center gap-1 text-xs text-muted-foreground"
              title="Copy to clipboard"
            >
              {copied ? <Check size={12} /> : <Copy size={12} />}
              {copied ? "Copied" : "Copy"}
            </button>
          </div>
        )}
      </div>

      {loading ? (
        <div className="rounded-md border px-3 py-4 text-xs text-muted-foreground">
          Loading…
        </div>
      ) : error ? (
        <div className="rounded-md border px-3 py-3 text-xs text-muted-foreground">
          Could not load captured content ({error}).
        </div>
      ) : body ? (
        <pre className="max-h-[60vh] overflow-auto whitespace-pre-wrap break-words rounded-md border bg-background/60 px-3 py-2 font-mono text-xs">
          {displayText}
        </pre>
      ) : (
        <div className="rounded-md border px-3 py-3 text-xs text-muted-foreground">
          {streamHint ? (
            <>
              No {title.toLowerCase()} content captured. This was a streamed response — set{" "}
              <code>content_logging.capture_streaming: true</code> (in addition to{" "}
              <code>enabled</code>) to capture streamed payloads.
            </>
          ) : (
            <>
              No {title.toLowerCase()} content captured — enable{" "}
              <code>content_logging</code> to capture payloads (admin only).
            </>
          )}
        </div>
      )}
    </div>
  );
}

interface SortableHeaderProps {
  label: string;
  column: LogSortBy;
  sortBy: LogSortBy;
  order: SortOrder;
  disabled: boolean;
  onSort: (column: LogSortBy) => void;
}

function SortableHeader({ label, column, sortBy, order, disabled, onSort }: SortableHeaderProps) {
  const active = sortBy === column;
  return (
    <TableHead className="px-4 py-3 font-medium">
      <button
        onClick={() => !disabled && onSort(column)}
        disabled={disabled}
        className={cn(
          "inline-flex items-center gap-1 disabled:cursor-not-allowed disabled:opacity-60",
          active ? "text-foreground" : "text-muted-foreground"
        )}
      >
        {label}
        {active && (order === "asc" ? <ArrowUp size={12} /> : <ArrowDown size={12} />)}
      </button>
    </TableHead>
  );
}

function LogsPageContent() {
  // --- admin token ---
  const [adminTok, setAdminTok] = useState("");
  const [showAdmin, setShowAdmin] = useState(false);
  const [tokenVersion, setTokenVersion] = useState(0); // bump to re-read localStorage
  const [hasToken, setHasToken] = useState(false);
  useEffect(() => {
    setHasToken(!!getAdminToken());
  }, [tokenVersion]);

  function saveAdmin() {
    setAdminToken(adminTok);
    setShowAdmin(false);
    setTokenVersion((v) => v + 1);
  }

  // --- view ---
  const [view, setView] = useState<View>("logs");

  // --- filters ---
  const [provider, setProvider] = useState("");
  const [model, setModel] = useState("");
  const [virtualKey, setVirtualKey] = useState("");
  const [status, setStatus] = useState("");
  const [search, setSearch] = useState("");
  const [cacheHit, setCacheHit] = useState<CacheFilter>("all");
  const [timeRange, setTimeRange] = useState<TimeRange>("all");

  const debouncedProvider = useDebouncedValue(provider, 300);
  const debouncedModel = useDebouncedValue(model, 300);
  const debouncedVirtualKey = useDebouncedValue(virtualKey, 300);
  const debouncedStatus = useDebouncedValue(status, 300);
  const debouncedSearch = useDebouncedValue(search, 300);

  const sinceMs = useMemo(() => timeRangeToSinceMs(timeRange), [timeRange]);

  // --- sort + paging ---
  const [sortBy, setSortBy] = useState<LogSortBy>("created_at");
  const [order, setOrder] = useState<SortOrder>("desc");
  const [limit, setLimit] = useState(25);
  const [offset, setOffset] = useState(0);

  function handleSort(column: LogSortBy) {
    if (sortBy === column) {
      setOrder((o) => (o === "asc" ? "desc" : "asc"));
    } else {
      setSortBy(column);
      setOrder("desc");
    }
    setOffset(0);
  }

  // --- live tail ---
  const [live, setLive] = useState(false);
  const [liveLogs, setLiveLogs] = useState<RequestLog[]>([]);

  useEffect(() => {
    if (!live) return;
    const token = getAdminToken();
    if (!token) {
      setLive(false);
      return;
    }
    setLiveLogs([]);
    const es = new EventSource(logStreamUrl(token));
    es.onmessage = (ev) => {
      try {
        const log = JSON.parse(ev.data) as RequestLog;
        setLiveLogs((prev) => [log, ...prev].slice(0, LIVE_BUFFER_CAP));
      } catch {
        // ignore malformed frames
      }
    };
    es.onerror = () => {
      // EventSource retries automatically; nothing to do here.
    };
    return () => es.close();
  }, [live]);

  const statusNum = debouncedStatus.trim() ? Number(debouncedStatus.trim()) : undefined;
  const statusValid = statusNum === undefined || Number.isFinite(statusNum);

  const baseFilters: LogStatsFilters = {
    provider: debouncedProvider.trim() || undefined,
    model: debouncedModel.trim() || undefined,
    virtual_key: debouncedVirtualKey.trim() || undefined,
    status: statusValid ? statusNum : undefined,
    cache_hit: cacheHit === "all" ? undefined : cacheHit === "hit",
    since_ms: sinceMs,
    search: debouncedSearch.trim() || undefined,
  };

  const listParams: LogQueryParams = {
    ...baseFilters,
    limit,
    offset,
    sort_by: sortBy,
    order,
  };

  const {
    data: logPage,
    isLoading: logsLoading,
    isError: logsError,
    error: logsErrorObj,
  } = useQuery({
    queryKey: ["logs", listParams],
    queryFn: () => getLogs(listParams),
    enabled: !live,
    retry: false,
    placeholderData: (prev) => prev,
  });

  const { data: stats } = useQuery({
    queryKey: ["logs-stats", baseFilters],
    queryFn: () => getLogStats(baseFilters),
    retry: false,
    refetchInterval: 5000,
  });

  // Known providers/models/virtual-keys, to populate the filter dropdowns.
  const { data: filterData } = useQuery({
    queryKey: ["logs-filterdata"],
    queryFn: () => getFilterData(),
    retry: false,
    staleTime: 60000,
  });

  const rows: RequestLog[] = live ? liveLogs : logPage?.logs ?? [];
  const total = live ? liveLogs.length : logPage?.total ?? 0;
  const rangeStart = total === 0 ? 0 : offset + 1;
  const rangeEnd = live ? liveLogs.length : Math.min(offset + limit, total);

  const [selectedLog, setSelectedLog] = useState<RequestLog | null>(null);

  // The open request lives in the URL (?request=<id>), so a trace can be linked to a
  // teammate and the browser back button closes the dialog.
  const router = useRouter();
  const pathname = usePathname();
  const searchParams = useSearchParams();
  const requestParam = searchParams.get("request");

  const openDetail = useCallback(
    (log: RequestLog) => {
      setSelectedLog(log);
      const next = new URLSearchParams(searchParams.toString());
      next.set("request", log.request_id);
      pushedRef.current = true;
      router.push(`${pathname}?${next}`, { scroll: false });
    },
    [pathname, router, searchParams],
  );

  // Set when *we* pushed the history entry, so closing can pop it instead of stacking a
  // second /logs entry that makes the first Back press look like a no-op.
  const pushedRef = useRef(false);

  const closeDetail = useCallback(() => {
    setSelectedLog(null);
    if (pushedRef.current) {
      pushedRef.current = false;
      router.back();
      return;
    }
    // Arrived by deep link: there is no entry of ours to pop.
    const next = new URLSearchParams(searchParams.toString());
    next.delete("request");
    const qs = next.toString();
    router.replace(qs ? `${pathname}?${qs}` : pathname, { scroll: false });
  }, [pathname, router, searchParams]);

  // Full record for the open dialog row — fetched via getLog(id) because the list endpoint
  // omits request_body/response_body to stay lean. Scalars render immediately from the row
  // (selectedLog); this only supplies the captured content once it resolves.
  const {
    data: logDetail,
    isFetching: logDetailLoading,
    isError: logDetailError,
    error: logDetailErrorObj,
  } = useQuery({
    queryKey: ["log-detail", requestParam],
    queryFn: () => getLog(requestParam!),
    enabled: !!requestParam,
    retry: false,
  });

  // Deep link / reload: adopt the detail record as the selection when the URL names a
  // request the list hasn't supplied (opened directly, or it has scrolled out of the page).
  useEffect(() => {
    if (!requestParam) {
      setSelectedLog(null);
    } else if (logDetail && logDetail.request_id === requestParam) {
      setSelectedLog((cur) => (cur?.request_id === requestParam ? cur : logDetail));
    }
  }, [requestParam, logDetail]);

  // How many upstream tries this request took — the tell that something failed over.
  // Streamed successes record `stream.ttft` instead of a plain `attempt` span.
  const attemptCount = useMemo(() => {
    if (!logDetail?.spans) return null;
    const n = logDetail.spans.filter(
      (sp) => sp.name.startsWith("attempt ·") || sp.name.startsWith("stream.ttft"),
    ).length;
    return n > 0 ? n : null;
  }, [logDetail]);

  // Small "N dropped" indicator — admin-gated, so only polled once a token is set.
  const { data: droppedCount } = useQuery({
    queryKey: ["logs-dropped"],
    queryFn: () => getDroppedCount(),
    enabled: hasToken,
    retry: false,
    refetchInterval: 10000,
  });

  // Current caller's role/permissions — determines whether the reveal action is offered.
  const { data: whoami } = useQuery({
    queryKey: ["whoami"],
    queryFn: () => getWhoami(),
    enabled: hasToken,
    retry: false,
  });
  const canReveal = !!whoami?.permissions.includes("logs:reveal");

  // Reveal flow for redacted content — kept only in component state, never persisted or
  // logged. Reset whenever the drawer closes or a different row is opened so secrets never
  // leak across rows.
  const [revealed, setRevealed] = useState<{
    request: string | null;
    response: string | null;
  } | null>(null);
  const [revealLoading, setRevealLoading] = useState(false);
  const [revealError, setRevealError] = useState<string | null>(null);

  useEffect(() => {
    setRevealed(null);
    setRevealError(null);
    setRevealLoading(false);
  }, [selectedLog?.request_id]);

  async function handleReveal() {
    if (!selectedLog) return;
    setRevealLoading(true);
    setRevealError(null);
    try {
      const res = await revealLog(selectedLog.request_id);
      setRevealed({ request: res.request_body, response: res.response_body });
    } catch (e) {
      setRevealError((e as Error).message);
    } finally {
      setRevealLoading(false);
    }
  }

  function handleHideRevealed() {
    setRevealed(null);
    setRevealError(null);
  }

  const authError =
    (logsErrorObj as Error | undefined)?.message === "admin token required";

  return (
    <div className="flex flex-col gap-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="font-display text-3xl font-semibold tracking-wide">Logs</h1>
          <p className="text-sm text-muted-foreground">
            Requests through the gateway — filter, sort, and tail live traffic.
          </p>
        </div>
        <div className="flex items-center gap-3">
          {hasToken && !!droppedCount && droppedCount > 0 && (
            <Tooltip>
              <TooltipTrigger asChild>
                <span className="cursor-default text-xs text-muted-foreground">
                  {droppedCount.toLocaleString()} dropped
                </span>
              </TooltipTrigger>
              <TooltipContent>
                Requests the log writer shed under backpressure — it could not keep up and
                dropped entries rather than block the gateway.
              </TooltipContent>
            </Tooltip>
          )}
          <button
            onClick={() => {
              setAdminTok(getAdminToken());
              setShowAdmin((s) => !s);
            }}
            className="text-xs text-muted-foreground underline"
          >
            {hasToken ? "admin token set" : "set admin token"}
          </button>
        </div>
      </div>

      <Tabs value={view} onValueChange={(v) => setView(v as View)}>
        <TabsList>
          {VIEWS.map((v) => (
            <TabsTrigger key={v.value} value={v.value}>
              {v.label}
            </TabsTrigger>
          ))}
        </TabsList>
      </Tabs>

      {showAdmin && (
        <Card>
          <CardContent className="flex flex-col gap-2">
            <Label className="text-xs font-medium">
              Admin token (required for GET /api/logs*, only needed if the gateway has{" "}
              <code>admin_token</code> set)
            </Label>
            <div className="flex gap-2">
              <Input
                type="password"
                value={adminTok}
                onChange={(e) => setAdminTok(e.target.value)}
                placeholder="Bearer token for /api/*"
              />
              <Button onClick={saveAdmin}>Save</Button>
            </div>
          </CardContent>
        </Card>
      )}

      {/* Stats bar */}
      <div className="grid grid-cols-2 gap-4 sm:grid-cols-3 lg:grid-cols-7">
        <StatTile label="Total" value={(stats?.total ?? 0).toLocaleString()} />
        <StatTile
          label="Success"
          value={(stats?.success ?? 0).toLocaleString()}
          note="2xx"
        />
        <StatTile
          label="Errors"
          value={(stats?.errors ?? 0).toLocaleString()}
          note="4xx/5xx"
        />
        <StatTile
          label="Avg latency"
          value={`${Math.round(stats?.avg_latency_ms ?? 0).toLocaleString()} ms`}
        />
        <StatTile
          label="Total tokens"
          value={(stats?.total_tokens ?? 0).toLocaleString()}
        />
        <StatTile label="Total cost" value={formatCost(stats?.total_cost ?? 0)} />
        <StatTile
          label="Cache hits"
          value={(stats?.cache_hits ?? 0).toLocaleString()}
        />
      </div>

      {/* Filters */}
      <Card>
        <CardContent className="flex flex-col gap-3">
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-4">
            <div className="flex flex-col gap-1">
              <Label className="text-xs font-medium">Provider</Label>
              <Input
                value={provider}
                onChange={(e) => {
                  setProvider(e.target.value);
                  setOffset(0);
                }}
                placeholder="openai"
                list="provider-options"
              />
              <datalist id="provider-options">
                {(filterData?.providers ?? []).map((p) => (
                  <option key={p} value={p} />
                ))}
              </datalist>
            </div>
            <div className="flex flex-col gap-1">
              <Label className="text-xs font-medium">Model</Label>
              <Input
                value={model}
                onChange={(e) => {
                  setModel(e.target.value);
                  setOffset(0);
                }}
                placeholder="gpt-4o"
                list="model-options"
              />
              <datalist id="model-options">
                {(filterData?.models ?? []).map((m) => (
                  <option key={m} value={m} />
                ))}
              </datalist>
            </div>
            <div className="flex flex-col gap-1">
              <Label className="text-xs font-medium">Virtual key</Label>
              <Input
                value={virtualKey}
                onChange={(e) => {
                  setVirtualKey(e.target.value);
                  setOffset(0);
                }}
                placeholder="vk_team_alpha"
                list="vkey-options"
              />
              <datalist id="vkey-options">
                {(filterData?.virtual_keys ?? []).map((k) => (
                  <option key={k} value={k} />
                ))}
              </datalist>
            </div>
            <div className="flex flex-col gap-1">
              <Label className="text-xs font-medium">Status</Label>
              <Input
                value={status}
                onChange={(e) => {
                  setStatus(e.target.value);
                  setOffset(0);
                }}
                placeholder="200"
                inputMode="numeric"
              />
            </div>
          </div>

          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-4">
            <div className="flex flex-col gap-1 lg:col-span-2">
              <Label className="text-xs font-medium">Search</Label>
              <Input
                value={search}
                onChange={(e) => {
                  setSearch(e.target.value);
                  setOffset(0);
                }}
                placeholder="Search request id, model, error…"
              />
            </div>
            <div className="flex flex-col gap-1">
              <Label className="text-xs font-medium">Cache</Label>
              <Select
                value={cacheHit}
                onValueChange={(v) => {
                  setCacheHit(v as CacheFilter);
                  setOffset(0);
                }}
              >
                <SelectTrigger className="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="all">All</SelectItem>
                  <SelectItem value="hit">Hits only</SelectItem>
                  <SelectItem value="miss">Misses only</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div className="flex flex-col gap-1">
              <Label className="text-xs font-medium">Time range</Label>
              <ToggleGroup
                type="single"
                value={timeRange}
                onValueChange={(v) => {
                  if (v) {
                    setTimeRange(v as TimeRange);
                    setOffset(0);
                  }
                }}
                variant="outline"
                spacing={0}
                className="w-full overflow-hidden rounded-md"
              >
                {TIME_RANGES.map((r) => (
                  <ToggleGroupItem
                    key={r.value}
                    value={r.value}
                    className="flex-1 text-xs font-medium"
                  >
                    {r.label}
                  </ToggleGroupItem>
                ))}
              </ToggleGroup>
            </div>
          </div>

          <div className="flex items-center justify-between border-t pt-3">
            <div className="flex items-center gap-2 text-xs text-muted-foreground">
              {!statusValid && <span className="text-error">Status must be a number.</span>}
            </div>
            {view === "logs" && (
              <label className="flex items-center gap-2 text-sm">
                <span title={hasToken ? "" : "Set an admin token to enable live tail"}>Live</span>
                <Switch
                  checked={live}
                  onCheckedChange={() => setLive((l) => !l)}
                  disabled={!hasToken}
                />
                {live && (
                  <span className="inline-flex items-center gap-1 text-xs font-medium text-primary">
                    <Radio size={12} className="animate-pulse" />
                    tailing
                  </span>
                )}
                {!hasToken && (
                  <span className="text-xs text-muted-foreground">
                    (set an admin token above)
                  </span>
                )}
              </label>
            )}
          </div>
        </CardContent>
      </Card>

      {/* Analytics view */}
      {view === "analytics" && (
        <AnalyticsPanel filters={baseFilters} timeRange={timeRange} active={view === "analytics"} />
      )}

      {/* Table (Logs view only) */}
      {view === "logs" && (authError ? (
        <EmptyState
          title="Could not load logs"
          hint="The gateway requires an admin token — click ‘set admin token’ above."
        />
      ) : logsError && !live ? (
        <EmptyState
          title="Could not load logs"
          hint="The gateway did not respond to GET /api/logs. Confirm it is running and reachable."
        />
      ) : rows.length === 0 && !logsLoading ? (
        <EmptyState
          title={live ? "Waiting for live traffic…" : "No requests match your filters"}
          hint={
            live
              ? "New requests will appear here as they happen."
              : "Try widening the time range or clearing filters."
          }
        />
      ) : (
        <div className="overflow-hidden rounded-xl border bg-card">
          <Table className="min-w-[900px] border-collapse text-sm">
            <TableHeader>
              <TableRow className="text-left text-xs uppercase tracking-wide text-muted-foreground hover:bg-transparent">
                <SortableHeader
                  label="Time"
                  column="created_at"
                  sortBy={sortBy}
                  order={order}
                  disabled={live}
                  onSort={handleSort}
                />
                <TableHead className="px-4 py-3 font-medium">Provider</TableHead>
                <TableHead className="px-4 py-3 font-medium">Model</TableHead>
                <TableHead className="px-4 py-3 font-medium">Virtual key</TableHead>
                <TableHead className="px-4 py-3 font-medium">Status</TableHead>
                <SortableHeader
                  label="Latency"
                  column="latency"
                  sortBy={sortBy}
                  order={order}
                  disabled={live}
                  onSort={handleSort}
                />
                <SortableHeader
                  label="Tokens"
                  column="tokens"
                  sortBy={sortBy}
                  order={order}
                  disabled={live}
                  onSort={handleSort}
                />
                <SortableHeader
                  label="Cost"
                  column="cost"
                  sortBy={sortBy}
                  order={order}
                  disabled={live}
                  onSort={handleSort}
                />
                <TableHead className="px-4 py-3 font-medium">Flags</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {logsLoading && rows.length === 0
                ? Array.from({ length: 5 }).map((_, i) => (
                    <TableRow key={`skeleton-${i}`} className="hover:bg-transparent">
                      <TableCell colSpan={9} className="px-4 py-3">
                        <Skeleton className="h-4 w-full" />
                      </TableCell>
                    </TableRow>
                  ))
                : rows.map((l) => (
                    <TableRow
                      key={l.request_id}
                      className="cursor-pointer"
                      onClick={() => openDetail(l)}
                    >
                      <TableCell className="whitespace-nowrap px-4 py-3 text-muted-foreground">
                        {formatTime(l.created_at)}
                      </TableCell>
                      <TableCell className="px-4 py-3">{l.provider}</TableCell>
                      <TableCell className="px-4 py-3 font-medium">{l.model}</TableCell>
                      <TableCell className="px-4 py-3">
                        {l.virtual_key ? (
                          <code className="text-xs">{l.virtual_key}</code>
                        ) : (
                          <span className="text-muted-foreground">—</span>
                        )}
                      </TableCell>
                      <TableCell className="px-4 py-3">
                        <span
                          className="inline-flex items-center gap-2 text-sm font-medium"
                          style={{ color: statusColor(l.status) }}
                        >
                          <span
                            className="h-2 w-2 shrink-0 rounded-full"
                            style={{ background: statusColor(l.status) }}
                            aria-hidden
                          />
                          {l.status}
                        </span>
                      </TableCell>
                      <TableCell className="px-4 py-3">
                        {l.latency_ms.toLocaleString()} ms
                      </TableCell>
                      <TableCell className="px-4 py-3 text-muted-foreground">
                        {l.prompt_tokens.toLocaleString()} / {l.completion_tokens.toLocaleString()}
                      </TableCell>
                      <TableCell className="px-4 py-3">{formatCost(l.cost)}</TableCell>
                      <TableCell className="px-4 py-3">
                        <div className="flex gap-1">
                          {l.stream && <MiniBadge>stream</MiniBadge>}
                          {l.cache_hit && <MiniBadge tone="accent">cache</MiniBadge>}
                          {l.redacted && <MiniBadge>redacted</MiniBadge>}
                        </div>
                      </TableCell>
                    </TableRow>
                  ))}
            </TableBody>
          </Table>

          {/* Pagination */}
          <div className="flex flex-wrap items-center justify-between gap-3 border-t px-4 py-3 text-xs text-muted-foreground">
            <div className="flex items-center gap-2">
              <span>Rows per page</span>
              <Select
                value={String(limit)}
                disabled={live}
                onValueChange={(v) => {
                  setLimit(Number(v));
                  setOffset(0);
                }}
              >
                <SelectTrigger size="sm" className="text-xs">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="25">25</SelectItem>
                  <SelectItem value="50">50</SelectItem>
                  <SelectItem value="100">100</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div className="flex items-center gap-3">
              <span>
                {live
                  ? `${liveLogs.length} live row${liveLogs.length === 1 ? "" : "s"}`
                  : `${rangeStart}–${rangeEnd} of ${total.toLocaleString()}`}
              </span>
              <div className="flex gap-1">
                <button
                  onClick={() => setOffset((o) => Math.max(0, o - limit))}
                  disabled={live || offset === 0}
                  className="rounded-md border px-2 py-1 disabled:cursor-not-allowed disabled:opacity-40"
                >
                  Prev
                </button>
                <button
                  onClick={() => setOffset((o) => o + limit)}
                  disabled={live || offset + limit >= total}
                  className="rounded-md border px-2 py-1 disabled:cursor-not-allowed disabled:opacity-40"
                >
                  Next
                </button>
              </div>
            </div>
          </div>
        </div>
      ))}

      {/* Detail dialog. Open state lives in the URL (?request=<id>) so a trace is
          shareable and the browser back button closes it. */}
      <Dialog
        open={!!requestParam}
        onOpenChange={(o) => {
          if (!o) closeDetail();
        }}
      >
        {/* Radix focuses the first focusable child on open, which lands on a trace
            row and makes it look pre-selected. Nothing here needs focus on arrival. */}
        <DialogContent onOpenAutoFocus={(e) => e.preventDefault()}>
          <DialogHeader>
            <DialogTitle>Request trace</DialogTitle>
            <span className="font-mono text-[11px] break-all text-muted-foreground">
              {requestParam}
              {selectedLog && ` · ${formatTime(selectedLog.created_at)}`}
            </span>
          </DialogHeader>
          {!selectedLog && (
            <DialogBody>
              <div className="mx-auto w-full max-w-[1400px] px-5 py-5">
                {logDetailError ? (
                  <div className="rounded-md border px-4 py-3 text-sm text-muted-foreground">
                    This request could not be loaded
                    {(logDetailErrorObj as Error | undefined)?.message
                      ? ` (${(logDetailErrorObj as Error).message})`
                      : ""}
                    . It may have aged out of the retention window, or the control plane may
                    need an admin token.
                  </div>
                ) : (
                  <Skeleton className="h-40 w-full rounded-md" />
                )}
              </div>
            </DialogBody>
          )}
          {selectedLog && (
            <DialogBody>
              <div className="mx-auto w-full max-w-[1400px]">
              {/* Summary strip: the facts you check before reading a single bar. */}
              <dl className="grid grid-cols-2 border-b bg-card sm:grid-cols-4">
                {[
                  ["Route", `${selectedLog.provider}/${selectedLog.model}`],
                  ["Status", String(selectedLog.status), statusColor(selectedLog.status)],
                  ["Total latency", `${selectedLog.latency_ms.toLocaleString()} ms`],
                  ["Attempts", attemptCount === null ? "—" : String(attemptCount),
                    attemptCount !== null && attemptCount > 1 ? "var(--warning)" : undefined],
                  ["Tokens", `${selectedLog.prompt_tokens.toLocaleString()} → ${selectedLog.completion_tokens.toLocaleString()}`],
                  ["Cost", formatCost(selectedLog.cost)],
                  ["Streamed", selectedLog.stream ? "yes" : "no"],
                  ["Served from cache", selectedLog.cache_hit ? "yes" : "no",
                    selectedLog.cache_hit ? "var(--success)" : undefined],
                  ["Virtual key", selectedLog.virtual_key ?? "—"],
                  ["Stop reason", selectedLog.stop_reason ?? "—"],
                ].map(([label, value, color]) => (
                  <div
                    key={label}
                    className="flex flex-col gap-0.5 border-t border-r px-4 py-2.5"
                  >
                    <dt className="text-[10px] uppercase tracking-wide text-muted-foreground">
                      {label}
                    </dt>
                    <dd
                      className="truncate font-mono text-xs tabular-nums"
                      title={value as string}
                      style={color ? { color: color as string } : undefined}
                    >
                      {value}
                    </dd>
                  </div>
                ))}
              </dl>

              <div className="flex flex-col gap-4 px-5 py-4">
                {selectedLog.error_message && (
                  <div className="rounded-md border border-error px-3 py-2 text-sm text-error">
                    {selectedLog.error_message}
                  </div>
                )}

                {logDetailLoading && !logDetail && (
                  <Skeleton className="h-28 w-full rounded-md" />
                )}

                {logDetail?.spans && logDetail.spans.length > 0 && (
                  <TraceWaterfall spans={logDetail.spans} />
                )}

                <div className="flex flex-col gap-4 border-t pt-4">
                {(logDetail?.redacted ?? selectedLog.redacted) && (
                  <div
                    className={cn(
                      "flex flex-wrap items-center justify-between gap-2 rounded-md border px-3 py-2 text-xs",
                      revealed && "border-error"
                    )}
                  >
                    <div className="flex flex-wrap items-center gap-2">
                      {revealed ? (
                        <span className="inline-flex items-center rounded-full border border-error px-2 py-0.5 text-[10px] font-medium uppercase tracking-wide text-error">
                          Revealed
                        </span>
                      ) : canReveal ? (
                        <span className="text-muted-foreground">Content is redacted.</span>
                      ) : (
                        <span className="text-muted-foreground">
                          Content is redacted — reveal requires admin.
                        </span>
                      )}
                      {revealError && <span className="text-error">{revealError}</span>}
                    </div>
                    {canReveal &&
                      (revealed ? (
                        <Button size="sm" onClick={handleHideRevealed} className="px-3 py-1 text-xs">
                          Hide
                        </Button>
                      ) : (
                        <Button
                          size="sm"
                          onClick={handleReveal}
                          disabled={revealLoading}
                          className="px-3 py-1 text-xs"
                        >
                          {revealLoading ? "Revealing…" : "🔓 Reveal"}
                        </Button>
                      ))}
                  </div>
                )}
                <ContentSection
                  title="Request"
                  body={revealed ? revealed.request : logDetail?.request_body}
                  loading={logDetailLoading && !logDetail}
                  error={
                    logDetailError
                      ? (logDetailErrorObj as Error | undefined)?.message
                      : undefined
                  }
                />
                <ContentSection
                  title="Response"
                  body={revealed ? revealed.response : logDetail?.response_body}
                  loading={logDetailLoading && !logDetail}
                  error={
                    logDetailError
                      ? (logDetailErrorObj as Error | undefined)?.message
                      : undefined
                  }
                  streamHint={selectedLog.stream}
                />
                </div>
              </div>
              </div>
            </DialogBody>
          )}
        </DialogContent>
      </Dialog>
    </div>
  );
}

// `useSearchParams` (which drives the detail dialog's ?request=<id>) opts the page into
// client-side rendering, so Next requires a Suspense boundary around it.
export default function LogsPage() {
  return (
    <Suspense fallback={<Skeleton className="h-96 w-full rounded-xl" />}>
      <LogsPageContent />
    </Suspense>
  );
}
