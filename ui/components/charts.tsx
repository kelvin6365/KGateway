"use client";

// Shared hand-rolled SVG chart components (no charting library), extracted from
// the Logs → Analytics view so the Dashboard can reuse them. Scaled responsively
// via viewBox and themed through the --chart-* / --error CSS tokens.

import { Card, CardContent } from "@/components/ui/card";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import type { Rank, RankMetric, TimePoint } from "@/lib/api";

export const ERROR_COLOR = "var(--error)";

export function formatAxisTime(ms: number): string {
  return new Date(ms).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function formatCostValue(v: number): string {
  return `$${v.toFixed(4)}`;
}

export function formatTokens(v: number): string {
  return Math.round(v).toLocaleString();
}

export function formatRankValue(metric: RankMetric, v: number): string {
  switch (metric) {
    case "count":
    case "errors":
      return v.toLocaleString();
    case "cost":
      return formatCostValue(v);
    case "tokens":
      return formatTokens(v);
  }
}

export function rankValueFor(r: Rank, metric: RankMetric): number {
  switch (metric) {
    case "count":
      return r.count;
    case "cost":
      return r.cost;
    case "tokens":
      return r.tokens;
    case "errors":
      return r.errors;
  }
}

export const RANK_METRICS: { value: RankMetric; label: string }[] = [
  { value: "count", label: "Count" },
  { value: "cost", label: "Cost" },
  { value: "tokens", label: "Tokens" },
  { value: "errors", label: "Errors" },
];

/** Section wrapper matching the dashboard's Card look, with a title + optional controls row. */
export function ChartCard({
  title,
  controls,
  children,
}: {
  title: string;
  controls?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <Card className="gap-3 py-4">
      <CardContent className="flex flex-col gap-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">{title}</div>
          {controls}
        </div>
        {children}
      </CardContent>
    </Card>
  );
}

/** Small segmented control reused for metric selectors — matches the time-range toggle style. */
export function SegmentedControl<T extends string>({
  value,
  options,
  onChange,
}: {
  value: T;
  options: { value: T; label: string }[];
  onChange: (v: T) => void;
}) {
  return (
    <ToggleGroup
      type="single"
      variant="outline"
      size="sm"
      value={value}
      onValueChange={(v) => {
        // Radix fires onValueChange("") when clicking the already-active item; ignore that.
        if (v) onChange(v as T);
      }}
    >
      {options.map((opt) => (
        <ToggleGroupItem key={opt.value} value={opt.value} aria-label={opt.label}>
          {opt.label}
        </ToggleGroupItem>
      ))}
    </ToggleGroup>
  );
}

export function ChartEmpty({ message = "No data for the current filters" }: { message?: string }) {
  return (
    <div className="flex h-[180px] items-center justify-center rounded-md border border-border text-sm text-muted-foreground">
      {message}
    </div>
  );
}

export function ChartLoading() {
  return (
    <div className="flex h-[180px] items-center justify-center rounded-md border border-border text-sm text-muted-foreground">
      Loading…
    </div>
  );
}

export function LegendDot({ color, label }: { color: string; label: string }) {
  return (
    <span className="inline-flex items-center gap-1.5">
      <span className="h-2 w-2 rounded-full" style={{ background: color }} aria-hidden />
      {label}
    </span>
  );
}

/**
 * Requests-over-time chart: one stacked bar per bucket (total height = requests,
 * a red segment at the top = errors within that bucket). Single axis (count).
 */
export function TimeseriesChart({ points, loading }: { points: TimePoint[]; loading: boolean }) {
  if (loading && points.length === 0) return <ChartLoading />;
  if (points.length === 0) return <ChartEmpty />;

  const width = 640;
  const height = 200;
  const padding = { top: 8, right: 8, bottom: 8, left: 8 };
  const innerW = width - padding.left - padding.right;
  const innerH = height - padding.top - padding.bottom;

  const maxCount = Math.max(1, ...points.map((p) => p.count));
  const slot = innerW / points.length;
  const barWidth = Math.max(1, slot - 2);

  let peak = points[0];
  for (const p of points) if (p.count > peak.count) peak = p;

  return (
    <div className="flex flex-col gap-2">
      <svg
        viewBox={`0 0 ${width} ${height}`}
        width="100%"
        style={{ height: "auto", display: "block", maxHeight: 200 }}
        preserveAspectRatio="none"
        role="img"
        aria-label="Requests over time"
      >
        <defs>
          <linearGradient id="ts-bar-gradient" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="var(--chart-1)" />
            <stop offset="100%" stopColor="var(--chart-3)" />
          </linearGradient>
        </defs>
        <line
          x1={padding.left}
          y1={height - padding.bottom}
          x2={width - padding.right}
          y2={height - padding.bottom}
          stroke="var(--border)"
          strokeWidth={1}
        />
        {points.map((p, i) => {
          const h = (p.count / maxCount) * innerH;
          const errH = maxCount > 0 ? Math.min(h, (p.errors / maxCount) * innerH) : 0;
          const x = padding.left + i * slot + (slot - barWidth) / 2;
          const yTop = height - padding.bottom - h;
          const successH = Math.max(0, h - errH);
          return (
            <g key={p.ts}>
              <title>
                {`${formatAxisTime(p.ts)} — ${p.count.toLocaleString()} requests, ${p.errors.toLocaleString()} errors`}
              </title>
              {successH > 0 && (
                <rect x={x} y={yTop} width={barWidth} height={successH} fill="url(#ts-bar-gradient)" rx={1} />
              )}
              {errH > 0 && (
                <rect
                  x={x}
                  y={yTop + successH}
                  width={barWidth}
                  height={Math.max(errH, 1)}
                  fill={ERROR_COLOR}
                  rx={1}
                />
              )}
              {h === 0 && <rect x={x} y={height - padding.bottom - 1} width={barWidth} height={1} fill="var(--border)" />}
            </g>
          );
        })}
      </svg>
      <div className="flex items-center justify-between text-[10px] text-muted-foreground">
        <span>{formatAxisTime(points[0].ts)}</span>
        <span>
          Peak {peak.count.toLocaleString()} @ {formatAxisTime(peak.ts)}
        </span>
        <span>{formatAxisTime(points[points.length - 1].ts)}</span>
      </div>
      <div className="flex items-center gap-4 text-[10px] text-muted-foreground">
        <LegendDot color="var(--chart-1)" label="Requests" />
        <LegendDot color={ERROR_COLOR} label="Errors" />
      </div>
    </div>
  );
}

/** One ranked entity (model/provider) as a labeled row with a proportional inline bar. */
export function RankingsTable({
  title,
  rankings,
  metric,
  loading,
}: {
  title: string;
  rankings: Rank[];
  metric: RankMetric;
  loading: boolean;
}) {
  return (
    <Card className="gap-3 py-4">
      <CardContent className="flex flex-col gap-3">
        <div className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">{title}</div>
        {loading && rankings.length === 0 ? (
          <div className="py-6 text-center text-xs text-muted-foreground">Loading…</div>
        ) : rankings.length === 0 ? (
          <div className="py-6 text-center text-xs text-muted-foreground">No data for the current filters</div>
        ) : (
          <div className="flex flex-col gap-2.5">
            {(() => {
              const max = Math.max(1, ...rankings.map((r) => rankValueFor(r, metric)));
              return rankings.map((r) => {
                const v = rankValueFor(r, metric);
                const pct = Math.max(v > 0 ? 2 : 0, (v / max) * 100);
                return (
                  <div key={r.key}>
                    <div className="mb-1 flex items-center justify-between gap-2 text-xs">
                      <span className="truncate font-medium" title={r.key}>
                        {r.key}
                      </span>
                      <span className="shrink-0 text-muted-foreground">{formatRankValue(metric, v)}</span>
                    </div>
                    <div className="h-1.5 rounded" style={{ background: "var(--border)" }}>
                      <div
                        className="h-1.5 rounded transition-all"
                        style={{ width: `${pct}%`, background: "var(--chart-1)" }}
                        role="progressbar"
                        aria-label={`${r.key}: ${formatRankValue(metric, v)}`}
                      />
                    </div>
                  </div>
                );
              });
            })()}
          </div>
        )}
      </CardContent>
    </Card>
  );
}
