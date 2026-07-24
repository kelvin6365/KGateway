"use client";

// Session journey — one session's full arc, told as a story: a chronological strip of
// every call, where the cost/tokens/errors went (by model and by outcome), a compact
// colored flow diagram, and an expandable call list that loads each call's trace. The
// question this page answers is "what did this session actually do, and where did the
// money/time/errors go?" — so the sequence leads, not abstract diagrams.

import { use, useCallback, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import Link from "next/link";
import {
  AlertTriangle,
  ArrowLeft,
  Check,
  ChevronDown,
  ChevronRight,
  Copy,
} from "lucide-react";
import { getSession, getLog, type RequestLog, type SessionSummary } from "@/lib/api";
import { Card, CardContent } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { Sankey, type SankeyNode, type SankeyLink } from "@/components/sankey";
import { TraceWaterfall } from "@/components/trace-waterfall";
import { ChartCard, SegmentedControl } from "@/components/charts";
import { statusColor } from "@/lib/status";
import { formatCost, formatCompact, formatDuration, formatClock, formatRelative } from "@/lib/format";
import { insight } from "@/lib/session-insights";

const isError = (status: number) => status < 200 || status >= 300;
const outcomeOf = (c: RequestLog) =>
  c.cache_hit ? "cache hit" : isError(c.status) ? "error" : "success";
const tokensOf = (c: RequestLog) => c.prompt_tokens + c.completion_tokens;

const OUTCOME_COLOR: Record<string, string> = {
  success: "var(--success)",
  "cache hit": "var(--warning)",
  error: "var(--error)",
};

// ---- Flow (compact, colored) -------------------------------------------------

type Weight = "count" | "tokens" | "cost";
const WEIGHTS: { value: Weight; label: string }[] = [
  { value: "count", label: "Calls" },
  { value: "tokens", label: "Tokens" },
  { value: "cost", label: "Cost" },
];
const weightOf = (c: RequestLog, w: Weight) =>
  w === "count" ? 1 : w === "tokens" ? tokensOf(c) : c.cost ?? 0;

function buildFlow(calls: RequestLog[], w: Weight): {
  nodes: SankeyNode[];
  links: SankeyLink[];
  colors: Record<string, string>;
} {
  const nodeSet = new Map<string, string>();
  const linkMap = new Map<string, number>();
  const colors: Record<string, string> = {};
  const add = (a: string, b: string, v: number) => {
    if (v <= 0) return;
    const k = JSON.stringify([a, b]);
    linkMap.set(k, (linkMap.get(k) ?? 0) + v);
  };
  for (const c of calls) {
    const v = weightOf(c, w);
    const prov = `p:${c.provider}`;
    const model = `m:${c.model}`;
    const out = outcomeOf(c);
    const outId = `o:${out}`;
    nodeSet.set(prov, c.provider);
    nodeSet.set(model, c.model);
    nodeSet.set(outId, out);
    colors[outId] = OUTCOME_COLOR[out];
    add(prov, model, v);
    add(model, outId, v);
  }
  const nodes = Array.from(nodeSet, ([id, label]) => ({ id, label }));
  const links = Array.from(linkMap, ([k, value]) => {
    const [source, target] = JSON.parse(k) as [string, string];
    return { source, target, value };
  });
  return { nodes, links, colors };
}

// ---- Header ------------------------------------------------------------------

function CopyChip({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      onClick={() =>
        navigator.clipboard?.writeText(text).then(
          () => {
            setCopied(true);
            setTimeout(() => setCopied(false), 1200);
          },
          () => {},
        )
      }
      className="inline-flex items-center gap-1 rounded border border-border px-2 py-1 font-mono text-xs text-muted-foreground hover:text-foreground"
      title="Copy session id"
    >
      {copied ? <Check size={12} /> : <Copy size={12} />}
      <span className="max-w-[52ch] truncate">{text}</span>
    </button>
  );
}

function Header({ summary, calls, now }: { summary: SessionSummary; calls: RequestLog[]; now: number }) {
  const ins = insight(summary, now);
  return (
    <div className="flex flex-col gap-2.5">
      <Link
        href="/sessions"
        className="inline-flex w-fit items-center gap-1 text-xs text-muted-foreground hover:text-foreground"
      >
        <ArrowLeft size={12} /> All sessions
      </Link>
      <div className="flex flex-wrap items-center gap-2">
        <h1 className="font-display text-2xl font-semibold tracking-wide">Session journey</h1>
        {ins.live && (
          <span
            className="inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-[10px] font-medium"
            style={{ borderColor: "color-mix(in oklab, var(--success) 40%, transparent)", color: "var(--success)" }}
          >
            <span className="relative flex h-1.5 w-1.5">
              <span className="absolute inline-flex h-full w-full animate-ping rounded-full opacity-70" style={{ background: "var(--success)" }} />
              <span className="relative inline-flex h-1.5 w-1.5 rounded-full" style={{ background: "var(--success)" }} />
            </span>
            active
          </span>
        )}
        {summary.error_count > 0 && (
          <span
            className="inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-[10px] font-medium"
            style={{ borderColor: "color-mix(in oklab, var(--error) 40%, transparent)", color: "var(--error)" }}
          >
            <AlertTriangle size={10} /> {summary.error_count} error{summary.error_count > 1 ? "s" : ""}
          </span>
        )}
      </div>
      <div className="flex flex-wrap items-center gap-2 text-sm text-muted-foreground">
        <CopyChip text={summary.session_id} />
        <span>
          {formatRelative(summary.first_ts, now)} · {formatDuration(ins.spanMs)} span
          {summary.virtual_key && (
            <>
              {" · "}key <code className="font-mono">{summary.virtual_key}</code>
            </>
          )}
        </span>
      </div>
      {calls.length < summary.call_count && (
        <p className="text-xs text-muted-foreground">
          Showing the first {calls.length.toLocaleString()} of {summary.call_count.toLocaleString()} calls.
        </p>
      )}
    </div>
  );
}

// ---- Stat tiles --------------------------------------------------------------

function Stat({ label, value, tone }: { label: string; value: string; tone?: "error" }) {
  return (
    <Card className="py-3.5">
      <CardContent className="px-4">
        <div className="text-[11px] uppercase tracking-wide text-muted-foreground">{label}</div>
        <div
          className="mt-1 text-2xl font-semibold tabular-nums"
          style={tone === "error" && value !== "0" ? { color: "var(--error)" } : undefined}
        >
          {value}
        </div>
      </CardContent>
    </Card>
  );
}

// ---- Journey strip (hero) ----------------------------------------------------

function JourneyStrip({ calls }: { calls: RequestLog[] }) {
  const t0 = calls[0].created_at;
  const last = calls[calls.length - 1];
  const t1 = Math.max(last.created_at + last.latency_ms, t0 + 1);
  const span = t1 - t0;
  return (
    <div className="flex flex-col gap-2">
      <div className="relative h-11 w-full overflow-hidden rounded-lg border border-border" style={{ background: "var(--muted)" }}>
        {calls.map((c, i) => {
          const left = Math.min(((c.created_at - t0) / span) * 100, 99.2);
          const width = Math.min(Math.max((c.latency_ms / span) * 100, 0.8), 100 - left);
          return (
            <div
              key={c.request_id}
              className="absolute top-0 h-full transition-opacity hover:opacity-100"
              style={{ left: `${left}%`, width: `${width}%`, minWidth: 2, background: statusColor(c.status), opacity: 0.88 }}
              title={`#${i + 1} · ${c.model} · ${c.status} · ${formatDuration(c.latency_ms)} · ${formatCost(c.cost)} · ${formatClock(c.created_at)}`}
            />
          );
        })}
      </div>
      <div className="flex justify-between text-[10px] tabular-nums text-muted-foreground">
        <span>{formatClock(t0)}</span>
        <span>{formatDuration(span)} elapsed</span>
        <span>{formatClock(t1)}</span>
      </div>
    </div>
  );
}

// ---- By-model breakdown ------------------------------------------------------

function ByModel({ calls }: { calls: RequestLog[] }) {
  const rows = useMemo(() => {
    const per = new Map<string, { calls: number; tokens: number; cost: number; errors: number }>();
    for (const c of calls) {
      const m = per.get(c.model) ?? { calls: 0, tokens: 0, cost: 0, errors: 0 };
      m.calls += 1;
      m.tokens += tokensOf(c);
      m.cost += c.cost ?? 0;
      if (isError(c.status)) m.errors += 1;
      per.set(c.model, m);
    }
    return Array.from(per, ([model, v]) => ({ model, ...v })).sort(
      (a, b) => b.cost - a.cost || b.calls - a.calls,
    );
  }, [calls]);
  const maxCost = Math.max(...rows.map((r) => r.cost), 1e-9);

  return (
    <div className="flex flex-col gap-3">
      {rows.map((r) => (
        <div key={r.model} className="flex flex-col gap-1">
          <div className="flex items-center justify-between gap-2 text-xs">
            <span className="truncate font-mono font-medium" title={r.model}>
              {r.model}
            </span>
            <span className="shrink-0 tabular-nums text-muted-foreground">
              {r.calls} calls · {formatCompact(r.tokens)} tok
              {r.errors > 0 && (
                <span style={{ color: "var(--error)" }}> · {r.errors} err</span>
              )}
              <span className="ml-2 font-medium text-foreground">{formatCost(r.cost)}</span>
            </span>
          </div>
          <div className="h-1.5 rounded-full" style={{ background: "var(--border)" }}>
            <div
              className="h-1.5 rounded-full"
              style={{
                width: `${Math.max(r.cost > 0 ? 3 : 0, (r.cost / maxCost) * 100)}%`,
                background: r.errors > 0 ? "var(--error)" : "var(--chart-1)",
              }}
            />
          </div>
        </div>
      ))}
    </div>
  );
}

// ---- Outcomes ----------------------------------------------------------------

function Outcomes({ calls }: { calls: RequestLog[] }) {
  const { ok, cache, err } = useMemo(() => {
    let ok = 0,
      cache = 0,
      err = 0;
    for (const c of calls) {
      if (c.cache_hit) cache += 1;
      else if (isError(c.status)) err += 1;
      else ok += 1;
    }
    return { ok, cache, err };
  }, [calls]);
  const total = calls.length || 1;
  const segs = [
    { label: "Success", n: ok, color: "var(--success)" },
    { label: "Cache", n: cache, color: "var(--warning)" },
    { label: "Error", n: err, color: "var(--error)" },
  ].filter((s) => s.n > 0);

  return (
    <div className="flex flex-col gap-3">
      <div className="flex h-3 overflow-hidden rounded-full" style={{ background: "var(--border)" }}>
        {segs.map((s) => (
          <div key={s.label} style={{ width: `${(s.n / total) * 100}%`, background: s.color }} title={`${s.label}: ${s.n}`} />
        ))}
      </div>
      <div className="flex flex-wrap gap-4 text-xs">
        {[
          { label: "Success", n: ok, color: "var(--success)" },
          { label: "Cache", n: cache, color: "var(--warning)" },
          { label: "Error", n: err, color: "var(--error)" },
        ].map((s) => (
          <span key={s.label} className="inline-flex items-center gap-1.5">
            <span className="h-2 w-2 rounded-full" style={{ background: s.color }} />
            <span className="text-muted-foreground">{s.label}</span>
            <span className="font-medium tabular-nums">{s.n}</span>
            <span className="text-muted-foreground">({Math.round((s.n / total) * 100)}%)</span>
          </span>
        ))}
      </div>
    </div>
  );
}

// ---- Call list ---------------------------------------------------------------

function CallRow({ call, index }: { call: RequestLog; index: number }) {
  const [open, setOpen] = useState(false);
  const { data: detail, isLoading } = useQuery({
    queryKey: ["log", call.request_id],
    queryFn: () => getLog(call.request_id),
    enabled: open,
    retry: false,
    staleTime: 300000,
  });

  return (
    <div className="rounded-md border border-border">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-3 px-3 py-2 text-left text-xs hover:bg-accent/50 focus-visible:outline-none focus-visible:ring-3 focus-visible:ring-ring/50"
      >
        {open ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
        <span className="w-6 tabular-nums text-muted-foreground">{index + 1}</span>
        <span className="inline-flex h-2 w-2 shrink-0 rounded-full" style={{ background: statusColor(call.status) }} aria-hidden />
        <span className="font-mono">{call.model}</span>
        <span style={isError(call.status) ? { color: "var(--error)" } : undefined}>{call.status}</span>
        <span className="ml-auto flex items-center gap-3 text-muted-foreground">
          <span className="tabular-nums">{formatCompact(tokensOf(call))} tok</span>
          <span className="tabular-nums">{formatCost(call.cost)}</span>
          <span className="tabular-nums">{formatDuration(call.latency_ms)}</span>
          <span className="tabular-nums">{formatClock(call.created_at)}</span>
        </span>
      </button>
      {open && (
        <div className="border-t border-border px-3 py-3">
          {isLoading && <Skeleton className="h-24 w-full rounded" />}
          {detail?.spans && detail.spans.length > 0 ? (
            <TraceWaterfall spans={detail.spans} />
          ) : (
            !isLoading && (
              <p className="text-xs text-muted-foreground">
                No trace recorded for this call.{" "}
                <Link href={`/logs?request=${encodeURIComponent(call.request_id)}`} className="underline hover:text-foreground">
                  Open in Logs
                </Link>
              </p>
            )
          )}
        </div>
      )}
    </div>
  );
}

// ---- Page --------------------------------------------------------------------

export default function SessionJourneyPage({ params }: { params: Promise<{ id: string }> }) {
  const { id } = use(params);
  const sessionId = decodeURIComponent(id);
  const [weight, setWeight] = useState<Weight>("count");
  const [showFlow, setShowFlow] = useState(false);

  const { data, isLoading, isError: isErr, error } = useQuery({
    queryKey: ["session", sessionId],
    queryFn: () => getSession(sessionId),
    retry: false,
    refetchInterval: 10000,
  });

  const calls = useMemo(() => data?.calls ?? [], [data]);
  const flow = useMemo(() => buildFlow(calls, weight), [calls, weight]);
  const fmtValue = useCallback((v: number) => (weight === "cost" ? formatCost(v) : v.toLocaleString()), [weight]);
  const now = Date.now();

  if (isLoading) return <Skeleton className="h-96 w-full rounded-xl" />;

  if (isErr) {
    const msg = (error as Error).message;
    return (
      <Card className="py-6">
        <CardContent className="flex flex-col gap-2 text-sm text-muted-foreground">
          <Link href="/sessions" className="inline-flex items-center gap-1 text-xs hover:text-foreground">
            <ArrowLeft size={12} /> All sessions
          </Link>
          {msg === "session not found"
            ? "This session isn't in the recent log window (only recent sessions are grouped)."
            : msg === "admin token required"
              ? "This gateway requires an admin token — set it on the Sessions page."
              : `Could not load this session: ${msg}`}
        </CardContent>
      </Card>
    );
  }

  const summary = data!.summary;
  const avgLatency = calls.length ? calls.reduce((a, c) => a + c.latency_ms, 0) / calls.length : 0;

  return (
    <div className="flex flex-col gap-5">
      <Header summary={summary} calls={calls} now={now} />

      <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-6">
        <Stat label="Calls" value={summary.call_count.toLocaleString()} />
        <Stat label="Tokens" value={formatCompact(summary.total_tokens)} />
        <Stat label="Cost" value={formatCost(summary.total_cost)} />
        <Stat label="Errors" value={summary.error_count.toLocaleString()} tone="error" />
        <Stat label="Avg latency" value={formatDuration(avgLatency)} />
        <Stat label="Models" value={summary.models.length.toLocaleString()} />
      </div>

      {calls.length > 0 && (
        <>
          <ChartCard title={`Journey — ${calls.length} calls over ${formatDuration(summary.last_ts - summary.first_ts)}`}>
            <JourneyStrip calls={calls} />
          </ChartCard>

          <div className="grid gap-3 lg:grid-cols-2">
            <ChartCard title="Where the spend went — by model">
              <ByModel calls={calls} />
            </ChartCard>
            <ChartCard title="Outcomes">
              <Outcomes calls={calls} />
            </ChartCard>
          </div>

          {/* Flow diagram: kept but demoted behind a toggle — it's most useful when a
              session spans several providers/models/outcomes. */}
          <ChartCard
            title="Flow — provider → model → outcome"
            controls={
              <div className="flex items-center gap-2">
                {showFlow && <SegmentedControl value={weight} options={WEIGHTS} onChange={setWeight} />}
                <button
                  type="button"
                  onClick={() => setShowFlow((v) => !v)}
                  className="rounded border border-border px-2 py-1 text-xs text-muted-foreground hover:text-foreground"
                >
                  {showFlow ? "Hide" : "Show"}
                </button>
              </div>
            }
          >
            {showFlow ? (
              <Sankey nodes={flow.nodes} links={flow.links} height={200} formatValue={fmtValue} nodeColors={flow.colors} />
            ) : (
              <p className="text-xs text-muted-foreground">
                A Sankey of how this session&apos;s calls flowed from provider to model to outcome.
              </p>
            )}
          </ChartCard>
        </>
      )}

      <div className="flex flex-col gap-2">
        <div className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">Calls</div>
        {calls.map((c, i) => (
          <CallRow key={c.request_id} call={c} index={i} />
        ))}
      </div>
    </div>
  );
}
