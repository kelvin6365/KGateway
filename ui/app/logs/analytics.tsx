"use client";

// Analytics view for the logs page: requests-over-time chart, a distribution histogram,
// and top-N rankings tables. The shared chart components live in components/charts.tsx
// (also used by the Dashboard); only the histogram is specific to this view.

import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  getTimeseries,
  getHistogram,
  getRankings,
  type LogStatsFilters,
  type HistogramBucket,
  type HistogramMetric,
  type RankBy,
  type RankMetric,
} from "@/lib/api";
import { Card, CardContent } from "@/components/ui/card";
import {
  ChartCard,
  ChartEmpty,
  ChartLoading,
  RANK_METRICS,
  RankingsTable,
  SegmentedControl,
  TimeseriesChart,
  formatCostValue,
  formatTokens,
} from "@/components/charts";
import { bucketMsForRange } from "@/lib/time";

const POLL_MS = 10000;

/** Time range as used by the Logs page filter bar — re-exported from the shared helpers. */
export type { TimeRange } from "@/lib/time";

function formatLatency(v: number): string {
  return `${Math.round(v).toLocaleString()}ms`;
}

function formatHistogramValue(metric: HistogramMetric, v: number): string {
  switch (metric) {
    case "latency":
      return formatLatency(v);
    case "cost":
      return formatCostValue(v);
    case "tokens":
      return formatTokens(v);
  }
}

/** Distribution histogram: one bar per bucket, labeled by its lo–hi range. */
function HistogramChart({
  buckets,
  metric,
  loading,
}: {
  buckets: HistogramBucket[];
  metric: HistogramMetric;
  loading: boolean;
}) {
  if (loading && buckets.length === 0) return <ChartLoading />;
  if (buckets.length === 0 || buckets.every((b) => b.count === 0)) return <ChartEmpty />;

  const width = 640;
  const height = 200;
  const labelH = 28;
  const padding = { top: 8, right: 8, bottom: 8 + labelH, left: 8 };
  const innerW = width - padding.left - padding.right;
  const innerH = height - padding.top - padding.bottom;

  const maxCount = Math.max(1, ...buckets.map((b) => b.count));
  const slot = innerW / buckets.length;
  const barWidth = Math.max(1, slot - 2);
  // Show at most ~8 tick labels so they don't collide.
  const labelStep = Math.max(1, Math.ceil(buckets.length / 8));

  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      width="100%"
      style={{ height: "auto", display: "block", maxHeight: 200 }}
      preserveAspectRatio="none"
      role="img"
      aria-label={`${metric} distribution`}
    >
      <line
        x1={padding.left}
        y1={height - padding.bottom}
        x2={width - padding.right}
        y2={height - padding.bottom}
        stroke="var(--border)"
        strokeWidth={1}
      />
      {buckets.map((b, i) => {
        const h = (b.count / maxCount) * innerH;
        const x = padding.left + i * slot + (slot - barWidth) / 2;
        const y = height - padding.bottom - h;
        const showLabel = i % labelStep === 0 || i === buckets.length - 1;
        return (
          <g key={`${b.lo}-${b.hi}-${i}`}>
            <title>
              {`${formatHistogramValue(metric, b.lo)}–${formatHistogramValue(metric, b.hi)}: ${b.count.toLocaleString()}`}
            </title>
            {h > 0 && <rect x={x} y={y} width={barWidth} height={h} fill="var(--chart-1)" rx={1} />}
            {showLabel && (
              <text
                x={x + barWidth / 2}
                y={height - padding.bottom + 12}
                fontSize={9}
                fill="var(--muted-foreground)"
                textAnchor="end"
                transform={`rotate(-40 ${x + barWidth / 2} ${height - padding.bottom + 12})`}
              >
                {formatHistogramValue(metric, b.lo)}
              </text>
            )}
          </g>
        );
      })}
    </svg>
  );
}

const HISTOGRAM_METRICS: { value: HistogramMetric; label: string }[] = [
  { value: "latency", label: "Latency" },
  { value: "cost", label: "Cost" },
  { value: "tokens", label: "Tokens" },
];

/**
 * The Analytics view: requests-over-time, a distribution histogram, and top-model /
 * top-provider rankings — all driven by the filters already active on the Logs page.
 */
export function AnalyticsPanel({
  filters,
  timeRange,
  active,
}: {
  filters: LogStatsFilters;
  timeRange: import("@/lib/time").TimeRange;
  active: boolean;
}) {
  const [histMetric, setHistMetric] = useState<HistogramMetric>("latency");
  const [rankMetric, setRankMetric] = useState<RankMetric>("count");

  const bucketMs = bucketMsForRange(timeRange);

  const {
    data: timeseries,
    isLoading: tsLoading,
    isError: tsError,
    error: tsErrorObj,
  } = useQuery({
    queryKey: ["logs-timeseries", filters, bucketMs],
    queryFn: () => getTimeseries({ ...filters, bucket_ms: bucketMs }),
    enabled: active,
    retry: false,
    refetchInterval: active ? POLL_MS : false,
    placeholderData: (prev) => prev,
  });

  const { data: histogram, isLoading: histLoading } = useQuery({
    queryKey: ["logs-histogram", filters, histMetric],
    queryFn: () => getHistogram({ ...filters, metric: histMetric, buckets: 20 }),
    enabled: active,
    retry: false,
    refetchInterval: active ? POLL_MS : false,
    placeholderData: (prev) => prev,
  });

  const { data: modelRankings, isLoading: modelLoading } = useQuery({
    queryKey: ["logs-rankings", "model" satisfies RankBy, filters, rankMetric],
    queryFn: () => getRankings({ ...filters, by: "model", metric: rankMetric, limit: 10 }),
    enabled: active,
    retry: false,
    refetchInterval: active ? POLL_MS : false,
    placeholderData: (prev) => prev,
  });

  const { data: providerRankings, isLoading: providerLoading } = useQuery({
    queryKey: ["logs-rankings", "provider" satisfies RankBy, filters, rankMetric],
    queryFn: () => getRankings({ ...filters, by: "provider", metric: rankMetric, limit: 10 }),
    enabled: active,
    retry: false,
    refetchInterval: active ? POLL_MS : false,
    placeholderData: (prev) => prev,
  });

  const authError = tsError && (tsErrorObj as Error | undefined)?.message === "admin token required";

  if (authError) {
    return (
      <Card className="py-16">
        <CardContent className="flex flex-col items-center justify-center gap-2 text-center">
          <div className="text-base font-semibold">Could not load analytics</div>
          <div className="max-w-md text-sm text-muted-foreground">
            The gateway requires an admin token — click &lsquo;set admin token&rsquo; above.
          </div>
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <ChartCard title="Requests over time">
        <TimeseriesChart points={timeseries?.points ?? []} loading={tsLoading} />
      </ChartCard>

      <ChartCard
        title="Distribution"
        controls={<SegmentedControl value={histMetric} options={HISTOGRAM_METRICS} onChange={setHistMetric} />}
      >
        <HistogramChart buckets={histogram?.buckets ?? []} metric={histMetric} loading={histLoading} />
        <div className="text-[10px] text-muted-foreground">
          Total: {(histogram?.total ?? 0).toLocaleString()}
        </div>
      </ChartCard>

      <div>
        <div className="mb-2 flex items-center justify-between">
          <div className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">Rankings</div>
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
    </div>
  );
}
