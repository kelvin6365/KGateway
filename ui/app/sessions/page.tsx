"use client";

// Sessions — an operational view of AI-usage journeys. Instead of a flat list of raw
// numbers, it leads with health + spend, surfaces the sessions that need attention
// (errors, live activity, runaway/looping agents, cost whales), and encodes each row so
// an operator can scan for problems. Everything is derived from the /api/sessions summary
// (see lib/session-insights) — no extra calls.

import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import Link from "next/link";
import {
  Activity,
  AlertTriangle,
  Check,
  Copy,
  Flame,
  Repeat,
  Search,
} from "lucide-react";
import {
  getSessions,
  getAdminToken,
  setAdminToken,
  type SessionSummary,
} from "@/lib/api";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Skeleton } from "@/components/ui/skeleton";
import { SegmentedControl } from "@/components/charts";
import { OrnateCard } from "@/components/baroque/filigree";
import { EmptyState } from "@/components/baroque/empty-state";
import { useCountUp } from "@/components/baroque/use-count-up";
import { useStaggerReveal } from "@/components/baroque/use-reveal";
import { formatCost, formatCompact, formatDuration, formatRelative } from "@/lib/format";
import {
  insight,
  overview,
  sortSessions,
  LIVE_WINDOW_MS,
  type ClientSort,
  type SessionHealth,
} from "@/lib/session-insights";

const SORTS: { value: ClientSort; label: string }[] = [
  { value: "attention", label: "Attention" },
  { value: "recent", label: "Recent" },
  { value: "cost", label: "Cost" },
  { value: "tokens", label: "Tokens" },
  { value: "calls", label: "Calls" },
];

const HEALTH_COLOR: Record<SessionHealth, string> = {
  error: "var(--error)",
  live: "var(--success)",
  busy: "var(--warning)",
  idle: "var(--muted-foreground)",
};

/** Now, sampled once per render tick from the polling query so relative times stay fresh. */
function useNow(intervalMs = 30000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), intervalMs);
    return () => clearInterval(t);
  }, [intervalMs]);
  return now;
}

// ---- KPI band ----------------------------------------------------------------

function Kpi({
  label,
  value,
  format,
  note,
  tone,
}: {
  label: string;
  value: number;
  format: (v: number) => string;
  note?: string;
  tone?: "error" | "success";
}) {
  const animated = useCountUp(value);
  const color =
    tone === "error" && value > 0
      ? "var(--error)"
      : tone === "success" && value > 0
        ? "var(--success)"
        : undefined;
  return (
    <OrnateCard className="p-4">
      <div className="text-[11px] uppercase tracking-wide text-muted-foreground">{label}</div>
      <div className="mt-1.5 text-2xl font-semibold tabular-nums" style={color ? { color } : undefined}>
        {format(animated)}
      </div>
      {note && <div className="mt-0.5 text-[11px] text-muted-foreground">{note}</div>}
    </OrnateCard>
  );
}

// ---- Attention spotlight -----------------------------------------------------

function Spotlight({
  icon: Icon,
  tone,
  label,
  session,
  detail,
}: {
  icon: React.ComponentType<{ size?: number; className?: string }>;
  tone: string;
  label: string;
  session: SessionSummary;
  detail: string;
}) {
  return (
    <Link
      href={`/sessions/${encodeURIComponent(session.session_id)}`}
      className="group flex items-center gap-3 rounded-lg border border-border p-3 transition-colors hover:border-[var(--primary)]/50 hover:bg-accent/40"
    >
      <span
        className="flex h-8 w-8 shrink-0 items-center justify-center rounded-md"
        style={{ background: `color-mix(in oklab, ${tone} 15%, transparent)`, color: tone }}
      >
        <Icon size={16} />
      </span>
      <div className="min-w-0">
        <div className="text-[11px] uppercase tracking-wide text-muted-foreground">{label}</div>
        <div className="truncate text-sm font-medium">{detail}</div>
      </div>
    </Link>
  );
}

// ---- Session row -------------------------------------------------------------

function CopyId({ id }: { id: string }) {
  const [copied, setCopied] = useState(false);
  const short = id.length > 16 ? `${id.slice(0, 8)}…${id.slice(-4)}` : id;
  return (
    <button
      type="button"
      onClick={(e) => {
        e.preventDefault();
        e.stopPropagation();
        navigator.clipboard?.writeText(id).then(
          () => {
            setCopied(true);
            setTimeout(() => setCopied(false), 1200);
          },
          () => {},
        );
      }}
      title={`Copy ${id}`}
      className="inline-flex items-center gap-1 rounded font-mono text-[11px] text-muted-foreground hover:text-foreground"
    >
      {copied ? <Check size={11} /> : <Copy size={11} />}
      {short}
    </button>
  );
}

function Chip({ children, tone }: { children: React.ReactNode; tone?: string }) {
  return (
    <span
      className="inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-[10px] font-medium"
      style={
        tone
          ? { borderColor: `color-mix(in oklab, ${tone} 40%, transparent)`, color: tone }
          : { borderColor: "var(--border)", color: "var(--muted-foreground)" }
      }
    >
      {children}
    </span>
  );
}

function SessionCard({
  s,
  now,
  maxCost,
}: {
  s: SessionSummary;
  now: number;
  maxCost: number;
}) {
  const ins = insight(s, now);
  const color = HEALTH_COLOR[ins.health];
  const primaryModel = s.models[0] ?? s.providers[0] ?? "—";
  const costPct = maxCost > 0 ? Math.max(2, (s.total_cost / maxCost) * 100) : 0;

  return (
    <Link
      href={`/sessions/${encodeURIComponent(s.session_id)}`}
      data-reveal
      className="group relative block overflow-hidden rounded-xl border border-border bg-card/40 transition-all hover:border-[var(--primary)]/40 hover:bg-accent/30 focus-visible:outline-none focus-visible:ring-3 focus-visible:ring-ring/50"
    >
      {/* status rail */}
      <span aria-hidden className="absolute inset-y-0 left-0 w-1" style={{ background: color }} />

      <div className="flex flex-col gap-2 p-3.5 pl-5">
        {/* line 1: identity + headline cost/time */}
        <div className="flex flex-wrap items-center gap-2">
          <span className="relative flex h-2.5 w-2.5 shrink-0">
            {ins.live && (
              <span
                className="absolute inline-flex h-full w-full animate-ping rounded-full opacity-60"
                style={{ background: color }}
              />
            )}
            <span className="relative inline-flex h-2.5 w-2.5 rounded-full" style={{ background: color }} />
          </span>

          <span className="truncate font-mono text-sm font-semibold" title={primaryModel}>
            {primaryModel}
          </span>
          {s.models.length > 1 && (
            <span className="text-[11px] text-muted-foreground">+{s.models.length - 1}</span>
          )}
          <Chip>{s.virtual_key ?? "anonymous"}</Chip>
          {ins.live && <Chip tone="var(--success)">live</Chip>}
          {ins.busy && (
            <Chip tone="var(--warning)">
              <Repeat size={9} /> high activity
            </Chip>
          )}

          <div className="ml-auto flex items-center gap-3">
            <span className="text-sm font-semibold tabular-nums">{formatCost(s.total_cost)}</span>
            <span className="text-[11px] text-muted-foreground">{formatRelative(s.last_ts, now)}</span>
          </div>
        </div>

        {/* line 2: metrics */}
        <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-muted-foreground">
          <span className="tabular-nums">
            <span className="font-medium text-foreground">{s.call_count.toLocaleString()}</span> calls
          </span>
          <span className="tabular-nums">
            <span className="font-medium text-foreground">{formatCompact(s.total_tokens)}</span> tokens
          </span>
          {s.error_count > 0 && (
            <span className="inline-flex items-center gap-1 tabular-nums" style={{ color: "var(--error)" }}>
              <AlertTriangle size={11} />
              {s.error_count.toLocaleString()} error{s.error_count > 1 ? "s" : ""}
            </span>
          )}
          {s.cache_hits > 0 && (
            <span className="tabular-nums">{Math.round(ins.cacheRate * 100)}% cached</span>
          )}
          <span className="tabular-nums">{formatDuration(ins.spanMs)} span</span>
          <CopyId id={s.session_id} />
        </div>

        {/* cost bar — spot the whales at a glance */}
        <div className="h-1 overflow-hidden rounded-full" style={{ background: "var(--border)" }}>
          <div className="h-1 rounded-full transition-all" style={{ width: `${costPct}%`, background: color }} />
        </div>
      </div>
    </Link>
  );
}

// ---- Page --------------------------------------------------------------------

export default function SessionsPage() {
  const now = useNow();
  const [sort, setSort] = useState<ClientSort>("attention");
  const [search, setSearch] = useState("");
  const [erroredOnly, setErroredOnly] = useState(false);
  const [adminTok, setAdminTokState] = useState("");

  useEffect(() => setAdminTokState(getAdminToken()), []);

  const { data, isLoading, isError, error } = useQuery({
    queryKey: ["sessions", "recent"],
    // Fetch a generous recent window; all sorting/filtering happens client-side so the
    // derived "Attention" order and instant filtering feel snappy.
    queryFn: () => getSessions({ sort: "recent", limit: 200 }),
    retry: false,
    refetchInterval: 10000,
  });

  const all = useMemo(() => data?.sessions ?? [], [data]);
  const ov = useMemo(() => overview(all, data?.total ?? all.length, now), [all, data, now]);

  const spotlights = useMemo(() => {
    if (all.length === 0) return [];
    const out: {
      key: string;
      icon: typeof Flame;
      tone: string;
      label: string;
      session: SessionSummary;
      detail: string;
    }[] = [];
    const mostErrors = all.filter((s) => s.error_count > 0).sort((a, b) => b.error_count - a.error_count)[0];
    if (mostErrors) {
      out.push({
        key: "errors",
        icon: AlertTriangle,
        tone: "var(--error)",
        label: "Most errors",
        session: mostErrors,
        detail: `${mostErrors.error_count} errors · ${mostErrors.models[0] ?? ""}`,
      });
    }
    const busiest = [...all]
      .map((s) => ({ s, i: insight(s, now) }))
      .filter((x) => x.i.busy)
      .sort((a, b) => b.i.callsPerMin - a.i.callsPerMin)[0];
    if (busiest) {
      out.push({
        key: "busy",
        icon: Repeat,
        tone: "var(--warning)",
        label: "Possible loop",
        session: busiest.s,
        detail: `${busiest.s.call_count} calls in ${formatDuration(busiest.i.spanMs)}`,
      });
    }
    const topSpend = [...all].sort((a, b) => b.total_cost - a.total_cost)[0];
    if (topSpend && topSpend.total_cost > 0) {
      out.push({
        key: "spend",
        icon: Flame,
        tone: "var(--chart-1)",
        label: "Top spender",
        session: topSpend,
        detail: `${formatCost(topSpend.total_cost)} · ${topSpend.total_tokens.toLocaleString()} tokens`,
      });
    }
    return out.slice(0, 3);
  }, [all, now]);

  const shown = useMemo(() => {
    const q = search.trim().toLowerCase();
    let list = all;
    if (erroredOnly) list = list.filter((s) => s.error_count > 0);
    if (q) {
      list = list.filter(
        (s) =>
          s.session_id.toLowerCase().includes(q) ||
          (s.virtual_key ?? "").toLowerCase().includes(q) ||
          s.models.some((m) => m.toLowerCase().includes(q)),
      );
    }
    return sortSessions(list, sort, now);
  }, [all, search, erroredOnly, sort, now]);

  const listScope = useStaggerReveal<HTMLDivElement>();
  const needsToken = (error as Error | undefined)?.message === "admin token required";

  return (
    <div className="flex flex-col gap-5">
      {/* header */}
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="font-display text-3xl font-semibold tracking-wide">Sessions</h1>
          <p className="text-sm text-muted-foreground">
            Each agent run and app conversation, grouped from its calls — spot the costly, the
            failing, and the runaways at a glance.
          </p>
        </div>
        <Input
          type="password"
          value={adminTok}
          onChange={(e) => setAdminTokState(e.target.value)}
          onBlur={() => setAdminToken(adminTok)}
          onKeyDown={(e) => e.key === "Enter" && setAdminToken(adminTok)}
          placeholder="admin token"
          className="h-9 w-40 text-xs"
        />
      </div>

      {isLoading && <Skeleton className="h-96 w-full rounded-xl" />}

      {isError && (
        <Card className="py-4">
          <CardContent className="text-sm text-muted-foreground">
            {needsToken
              ? "This gateway requires an admin token. Enter it above to view sessions."
              : `Could not load sessions: ${(error as Error).message}`}
          </CardContent>
        </Card>
      )}

      {!isLoading && !isError && all.length === 0 && (
        <EmptyState
          title="No sessions yet"
          hint="Send requests with an x-session-id header — or an OpenAI user field / Anthropic metadata.user_id — and each working session will group here with its cost, tokens, and journey."
        />
      )}

      {all.length > 0 && (
        <>
          {/* KPI band */}
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-6">
            <Kpi
              label="Active now"
              value={ov.active}
              format={(v) => Math.round(v).toString()}
              note={`last ${Math.round(LIVE_WINDOW_MS / 60000)}m`}
              tone="success"
            />
            <Kpi label="Sessions" value={ov.total} format={(v) => Math.round(v).toLocaleString()} />
            <Kpi label="Spend" value={ov.spend} format={(v) => formatCost(v)} />
            <Kpi label="Tokens" value={ov.tokens} format={(v) => formatCompact(v)} />
            <Kpi
              label="Errored"
              value={ov.errored}
              format={(v) => Math.round(v).toString()}
              note="sessions"
              tone="error"
            />
            <Kpi label="Avg / session" value={ov.avgCost} format={(v) => formatCost(v)} />
          </div>

          {/* attention spotlight */}
          {spotlights.length > 0 && (
            <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
              {spotlights.map((sp) => (
                <Spotlight
                  key={sp.key}
                  icon={sp.icon}
                  tone={sp.tone}
                  label={sp.label}
                  session={sp.session}
                  detail={sp.detail}
                />
              ))}
            </div>
          )}

          {/* controls */}
          <div className="flex flex-wrap items-center gap-3">
            <div className="relative flex-1 sm:min-w-[220px] sm:flex-none">
              <Search
                size={14}
                className="pointer-events-none absolute left-2.5 top-1/2 -translate-y-1/2 text-muted-foreground"
              />
              <Input
                value={search}
                onChange={(e) => setSearch(e.target.value)}
                placeholder="Search id, model, key…"
                className="h-9 w-full pl-8 text-sm sm:w-64"
              />
            </div>
            <label className="flex items-center gap-2 text-xs text-muted-foreground">
              <Switch checked={erroredOnly} onCheckedChange={setErroredOnly} />
              Errored only
            </label>
            <div className="ml-auto flex items-center gap-2">
              <Activity size={14} className="text-muted-foreground" />
              <SegmentedControl value={sort} options={SORTS} onChange={setSort} />
            </div>
          </div>

          {/* list */}
          {shown.length === 0 ? (
            <Card className="py-8">
              <CardContent className="text-center text-sm text-muted-foreground">
                No sessions match the current filters.
              </CardContent>
            </Card>
          ) : (
            <div ref={listScope} className="flex flex-col gap-2.5">
              {shown.map((s) => (
                <SessionCard key={s.session_id} s={s} now={now} maxCost={ov.maxCost} />
              ))}
            </div>
          )}

          <p className="text-xs text-muted-foreground">
            {shown.length === all.length
              ? `${shown.length} sessions`
              : `${shown.length} of ${all.length} shown`}
            {ov.total > all.length && ` · newest ${all.length} of ${ov.total.toLocaleString()}`}
          </p>
        </>
      )}
    </div>
  );
}
