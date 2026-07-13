"use client";

// Operator dashboard: everything an admin wants at a glance — health, traffic
// over a chosen window, cost/tokens, latency, recent errors for triage, and
// the configured system surface (providers / virtual keys / cache / MCP).
// All aggregate data comes from the logs control-plane APIs (time-range aware
// via since_ms); the Prometheus /metrics tiles it replaced were process-
// lifetime counters that reset on every gateway restart.

import { useMemo, useState } from "react";
import Link from "next/link";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, ArrowRight, Boxes, Database, KeyRound, Wrench } from "lucide-react";
import {
  BASE_URL,
  getAdminToken,
  getDroppedCount,
  getLogs,
  getLogStats,
  getMcpTools,
  getProviders,
  getRankings,
  getTimeseries,
  getVirtualKeys,
  health,
  setAdminToken,
  type LogStatsFilters,
  type RankBy,
  type RankMetric,
  type RequestLog,
} from "@/lib/api";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import {
  ChartCard,
  RANK_METRICS,
  RankingsTable,
  SegmentedControl,
  TimeseriesChart,
} from "@/components/charts";
import { OrnateCard } from "@/components/baroque/filigree";
import { useCountUp } from "@/components/baroque/use-count-up";
import { useStaggerReveal } from "@/components/baroque/use-reveal";
import { bucketMsForRange, sinceMsForRange, type TimeRange } from "@/lib/time";
import { statusColor } from "@/lib/status";

const STATS_POLL_MS = 15000;
const ERRORS_POLL_MS = 10000;

const TIME_RANGES: { value: TimeRange; label: string }[] = [
  { value: "1h", label: "Last 1h" },
  { value: "24h", label: "Last 24h" },
  { value: "7d", label: "Last 7d" },
  { value: "all", label: "All" },
];

function StatTile({
  label,
  value,
  format,
  note,
  tone,
}: {
  label: string;
  value: number;
  format?: (v: number) => string;
  note: string;
  tone?: "error";
}) {
  const animated = useCountUp(value);
  const display = format ? format(animated) : Math.round(animated).toLocaleString();

  return (
    <OrnateCard className="p-5">
      <div className="text-xs uppercase tracking-wide text-muted-foreground">{label}</div>
      <div
        className="mt-2 text-3xl font-semibold"
        style={tone === "error" && value > 0 ? { color: "var(--error)" } : undefined}
      >
        {display}
      </div>
      <div className="mt-1 text-xs text-muted-foreground">{note}</div>
    </OrnateCard>
  );
}

/** Small link card for the system row (providers / keys / cache / MCP). */
function SystemTile({
  href,
  icon: Icon,
  label,
  value,
  note,
}: {
  href: string;
  icon: React.ComponentType<{ size?: number; className?: string }>;
  label: string;
  value: string;
  note: string;
}) {
  return (
    <Link href={href} className="group">
      <Card className="h-full gap-2 py-4 transition-colors group-hover:border-[var(--primary)]/40">
        <CardContent className="flex flex-col gap-1">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2 text-xs uppercase tracking-wide text-muted-foreground">
              <Icon size={14} className="text-primary" />
              {label}
            </div>
            <ArrowRight
              size={14}
              className="text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100"
            />
          </div>
          <div className="text-xl font-semibold">{value}</div>
          <div className="truncate text-xs text-muted-foreground" title={note}>
            {note}
          </div>
        </CardContent>
      </Card>
    </Link>
  );
}

/** Latest 4xx/5xx requests for one-click triage — each row links to /logs. */
function RecentErrors({ errors, loading }: { errors: RequestLog[]; loading: boolean }) {
  return (
    <Card className="gap-3 py-4">
      <CardContent className="flex flex-col gap-3">
        <div className="flex items-center justify-between">
          <div className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
            Recent errors
          </div>
          <Link href="/logs" className="text-xs text-primary hover:underline">
            View all logs
          </Link>
        </div>
        {loading && errors.length === 0 ? (
          <div className="py-4 text-center text-xs text-muted-foreground">Loading…</div>
        ) : errors.length === 0 ? (
          <div className="flex items-center gap-2 py-4 text-sm text-muted-foreground">
            <span className="h-2 w-2 rounded-full" style={{ background: "var(--success)" }} aria-hidden />
            No recent errors — the gateway is running clean.
          </div>
        ) : (
          <div className="flex flex-col">
            {errors.map((l) => (
              <Link
                key={l.request_id}
                href="/logs"
                className="flex items-center gap-3 border-b border-border py-2 text-sm transition-opacity last:border-b-0 hover:opacity-80"
              >
                <span
                  className="inline-flex shrink-0 items-center gap-1.5 font-medium"
                  style={{ color: statusColor(l.status) }}
                >
                  <span
                    className="h-2 w-2 rounded-full"
                    style={{ background: statusColor(l.status) }}
                    aria-hidden
                  />
                  {l.status}
                </span>
                <span className="shrink-0 whitespace-nowrap text-xs text-muted-foreground">
                  {new Date(l.created_at).toLocaleString()}
                </span>
                <span className="shrink-0 font-medium">
                  {l.provider}/{l.model}
                </span>
                <span className="truncate text-xs text-muted-foreground">
                  {l.error_message ?? "—"}
                </span>
              </Link>
            ))}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

export default function DashboardPage() {
  const qc = useQueryClient();
  const scope = useStaggerReveal<HTMLDivElement>();

  // --- admin token (same affordance as the Logs page) ---
  const [adminTok, setAdminTok] = useState("");
  const [showAdmin, setShowAdmin] = useState(false);
  const hasToken = typeof window !== "undefined" && !!getAdminToken();

  function saveAdmin() {
    setAdminToken(adminTok);
    setShowAdmin(false);
    qc.invalidateQueries();
  }

  // --- time range drives every aggregate below ---
  const [timeRange, setTimeRange] = useState<TimeRange>("24h");
  const sinceMs = useMemo(() => sinceMsForRange(timeRange), [timeRange]);
  const bucketMs = bucketMsForRange(timeRange);
  // Same key shape as the Logs page's baseFilters so TanStack Query caches align.
  const filters: LogStatsFilters = useMemo(
    () => ({
      provider: undefined,
      model: undefined,
      virtual_key: undefined,
      status: undefined,
      cache_hit: undefined,
      since_ms: sinceMs,
      search: undefined,
    }),
    [sinceMs],
  );

  // --- health (public) ---
  const { data: healthy, isLoading: healthLoading } = useQuery({
    queryKey: ["health"],
    queryFn: health,
    refetchInterval: 5000,
    retry: false,
  });

  // --- aggregates (admin-gated when the gateway has admin_token set) ---
  const {
    data: stats,
    isError: statsError,
    error: statsErrorObj,
  } = useQuery({
    queryKey: ["logs-stats", filters],
    queryFn: () => getLogStats(filters),
    retry: false,
    refetchInterval: STATS_POLL_MS,
    placeholderData: (prev) => prev,
  });

  const { data: timeseries, isLoading: tsLoading } = useQuery({
    queryKey: ["logs-timeseries", filters, bucketMs],
    queryFn: () => getTimeseries({ ...filters, bucket_ms: bucketMs }),
    retry: false,
    refetchInterval: STATS_POLL_MS,
    placeholderData: (prev) => prev,
  });

  const [rankMetric, setRankMetric] = useState<RankMetric>("cost");
  const { data: modelRankings, isLoading: modelLoading } = useQuery({
    queryKey: ["logs-rankings", "model" satisfies RankBy, filters, rankMetric],
    queryFn: () => getRankings({ ...filters, by: "model", metric: rankMetric, limit: 10 }),
    retry: false,
    refetchInterval: STATS_POLL_MS,
    placeholderData: (prev) => prev,
  });
  const { data: providerRankings, isLoading: providerLoading } = useQuery({
    queryKey: ["logs-rankings", "provider" satisfies RankBy, filters, rankMetric],
    queryFn: () => getRankings({ ...filters, by: "provider", metric: rankMetric, limit: 10 }),
    retry: false,
    refetchInterval: STATS_POLL_MS,
    placeholderData: (prev) => prev,
  });

  // Recent errors: the API has no status>=400 family filter, so fetch the
  // latest page and filter client-side. Deliberately NOT time-range scoped.
  const { data: recentPage, isLoading: recentLoading } = useQuery({
    queryKey: ["logs", { limit: 100, sort_by: "created_at", order: "desc" }],
    queryFn: () => getLogs({ limit: 100, sort_by: "created_at", order: "desc" }),
    retry: false,
    refetchInterval: ERRORS_POLL_MS,
    placeholderData: (prev) => prev,
  });
  const recentErrors = useMemo(
    () => (recentPage?.logs ?? []).filter((l) => l.status >= 400).slice(0, 5),
    [recentPage],
  );

  // --- system surface (providers/vkeys admin-gated; MCP public) ---
  const { data: providers, isError: providersError } = useQuery({
    queryKey: ["providers"],
    queryFn: getProviders,
    retry: false,
    staleTime: 60000,
  });
  const { data: virtualKeys, isError: vkeysError } = useQuery({
    queryKey: ["virtual-keys"],
    queryFn: getVirtualKeys,
    retry: false,
    staleTime: 60000,
  });
  const { data: mcpTools } = useQuery({
    queryKey: ["mcp-tools"],
    queryFn: getMcpTools,
    retry: false,
    staleTime: 60000,
  });
  const { data: droppedCount } = useQuery({
    queryKey: ["logs-dropped"],
    queryFn: getDroppedCount,
    enabled: hasToken,
    retry: false,
    refetchInterval: ERRORS_POLL_MS,
  });

  const authError =
    statsError && (statsErrorObj as Error | undefined)?.message === "admin token required";

  const status = healthLoading
    ? { label: "Checking…", color: "var(--muted-foreground)" }
    : healthy
      ? { label: "Connected", color: "var(--success)" }
      : { label: "Unreachable", color: "var(--error)" };

  const total = stats?.total ?? 0;
  const successRate = total > 0 ? ((stats?.success ?? 0) / total) * 100 : null;
  const errorRate = total > 0 ? ((stats?.errors ?? 0) / total) * 100 : null;
  const cacheRate = total > 0 ? ((stats?.cache_hits ?? 0) / total) * 100 : null;

  const capabilities = Array.from(new Set((providers ?? []).flatMap((p) => p.capabilities)));

  return (
    <div ref={scope} className="flex flex-col gap-6">
      {/* Header: identity + health + window */}
      <div data-reveal className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="font-display text-3xl font-semibold tracking-wide">Dashboard</h1>
          <p className="text-sm text-muted-foreground">
            Gateway at <code className="font-mono">{BASE_URL}</code>
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-3">
          {hasToken && !!droppedCount && droppedCount > 0 && (
            <span
              className="inline-flex items-center gap-1 text-xs"
              style={{ color: "var(--warning)" }}
              title="Requests the log writer shed under backpressure."
            >
              <AlertTriangle size={12} />
              {droppedCount.toLocaleString()} logs dropped
            </span>
          )}
          <Badge variant="outline">
            <span className="h-2 w-2 rounded-full" style={{ background: status.color }} aria-hidden />
            {status.label}
          </Badge>
          <SegmentedControl
            value={timeRange}
            options={TIME_RANGES}
            onChange={(v) => setTimeRange(v)}
          />
        </div>
      </div>

      {/* Admin token affordance (gateway in strict mode) */}
      {authError && (
        <Card data-reveal className="py-4">
          <CardContent className="flex flex-col gap-2">
            <div className="text-sm font-semibold">Admin token required</div>
            <p className="text-xs text-muted-foreground">
              This gateway has <code>admin_token</code> configured — dashboard metrics need it.
            </p>
            {showAdmin ? (
              <div className="flex gap-2">
                <Input
                  type="password"
                  value={adminTok}
                  onChange={(e) => setAdminTok(e.target.value)}
                  placeholder="Bearer token for /api/*"
                />
                <Button onClick={saveAdmin}>Save</Button>
              </div>
            ) : (
              <div>
                <Button
                  variant="outline"
                  onClick={() => {
                    setAdminTok(getAdminToken());
                    setShowAdmin(true);
                  }}
                >
                  Set admin token
                </Button>
              </div>
            )}
          </CardContent>
        </Card>
      )}

      {/* Hero stats */}
      <div data-reveal className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <StatTile
          label="Requests"
          value={total}
          note={successRate !== null ? `${successRate.toFixed(1)}% success` : "No traffic in window"}
        />
        <StatTile
          label="Errors"
          value={stats?.errors ?? 0}
          tone="error"
          note={errorRate !== null ? `${errorRate.toFixed(1)}% error rate` : "4xx / 5xx"}
        />
        <StatTile
          label="Avg latency"
          value={stats?.avg_latency_ms ?? 0}
          format={(v) => `${Math.round(v).toLocaleString()} ms`}
          note={`Across ${total.toLocaleString()} requests`}
        />
        <StatTile
          label="Total cost"
          value={stats?.total_cost ?? 0}
          format={(v) => `$${v.toFixed(4)}`}
          note={`${(stats?.total_tokens ?? 0).toLocaleString()} tokens`}
        />
      </div>

      {/* Getting started — onboarding only, disappears once traffic exists */}
      {stats && total === 0 && (
        <Card data-reveal className="py-5">
          <CardContent>
            <div className="text-sm font-semibold">Getting started</div>
            <p className="mt-2 text-sm text-muted-foreground">
              Head to the <strong>Playground</strong> to send a chat completion through the
              gateway. Point requests at any configured provider using the{" "}
              <code>provider/model</code> convention (e.g. <code>openai/gpt-4o</code>,{" "}
              <code>anthropic/claude-3-5-sonnet</code>).
            </p>
          </CardContent>
        </Card>
      )}

      {/* Traffic */}
      <div data-reveal>
        <ChartCard title="Requests over time">
          <TimeseriesChart points={timeseries?.points ?? []} loading={tsLoading} />
        </ChartCard>
      </div>

      {/* Rankings */}
      <div data-reveal>
        <div className="mb-2 flex items-center justify-between">
          <div className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
            Top spenders
          </div>
          <SegmentedControl value={rankMetric} options={RANK_METRICS} onChange={setRankMetric} />
        </div>
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
          <RankingsTable
            title="Top models"
            rankings={modelRankings?.rankings ?? []}
            metric={rankMetric}
            loading={modelLoading}
          />
          <RankingsTable
            title="Top providers"
            rankings={providerRankings?.rankings ?? []}
            metric={rankMetric}
            loading={providerLoading}
          />
        </div>
      </div>

      {/* Triage */}
      <div data-reveal>
        <RecentErrors errors={recentErrors} loading={recentLoading} />
      </div>

      {/* System surface */}
      <div data-reveal className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <SystemTile
          href="/providers"
          icon={Boxes}
          label="Providers"
          value={providersError ? "—" : String(providers?.length ?? 0)}
          note={
            providersError
              ? "requires admin token"
              : capabilities.length
                ? capabilities.join(" · ")
                : "none configured"
          }
        />
        <SystemTile
          href="/virtual-keys"
          icon={KeyRound}
          label="Virtual keys"
          value={vkeysError ? "—" : String(virtualKeys?.length ?? 0)}
          note={
            vkeysError
              ? "requires admin token"
              : (virtualKeys?.length ?? 0) > 0
                ? "strict mode — bearer required"
                : "open mode"
          }
        />
        <SystemTile
          href="/cache"
          icon={Database}
          label="Cache"
          value={cacheRate !== null ? `${cacheRate.toFixed(1)}%` : "—"}
          note={`${(stats?.cache_hits ?? 0).toLocaleString()} hits in window`}
        />
        <SystemTile
          href="/mcp"
          icon={Wrench}
          label="MCP tools"
          value={String(mcpTools?.length ?? 0)}
          note={(mcpTools?.length ?? 0) > 0 ? "exposed to models" : "none registered"}
        />
      </div>
    </div>
  );
}
