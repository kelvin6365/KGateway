"use client";

// Request trace waterfall: every stage of one AI call on a shared timeline —
// governance, cache, each dispatch attempt (including the ones that failed before
// the successful retry), time-to-first-token, and the write-back.
//
// Laid out as label-above-bar rows rather than a two-column timeline: the detail
// drawer is ~448px wide, where a name column would leave the bars unreadable.

import { useState } from "react";
import type { TraceSpan } from "@/lib/api";

/** Colour band per category. Only existing theme tokens — no new palette. */
const CATEGORY: Record<string, { color: string; label: string }> = {
  gateway: { color: "var(--chart-3)", label: "Gateway" },
  policy: { color: "var(--chart-2)", label: "Governance" },
  cache: { color: "var(--success)", label: "Cache" },
  network: { color: "var(--chart-1)", label: "Upstream call" },
  wait: { color: "var(--muted-foreground)", label: "Waiting" },
  failed: { color: "var(--error)", label: "Failed attempt" },
  tools: { color: "var(--warning)", label: "MCP tools" },
  write: { color: "var(--chart-4)", label: "Write-back" },
};

const fallback = { color: "var(--muted-foreground)", label: "Other" };
const band = (c: string) => CATEGORY[c] ?? fallback;

/** Durations span microseconds to seconds; keep every row readable at its own scale. */
function formatDuration(us: number): string {
  const ms = us / 1000;
  if (ms >= 1000) return `${(ms / 1000).toFixed(2)} s`;
  if (ms >= 10) return `${Math.round(ms)} ms`;
  if (ms >= 1) return `${ms.toFixed(1)} ms`;
  return `${Math.round(us)} µs`;
}

export function TraceWaterfall({ spans }: { spans: TraceSpan[] }) {
  const [openIndex, setOpenIndex] = useState<number | null>(null);

  if (spans.length === 0) return null;

  // The timeline runs to the end of the last stage, not the longest one — a stage
  // starting late must not overflow the track.
  const total = Math.max(...spans.map((s) => s.start_us + s.dur_us), 1);
  const categoriesPresent = Array.from(new Set(spans.map((s) => s.category)));

  return (
    <div className="flex flex-col gap-2">
      <div className="flex items-baseline justify-between gap-2">
        <span className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
          Trace
        </span>
        <span className="font-mono text-[10px] text-muted-foreground">
          {spans.length} stages · {formatDuration(total)}
        </span>
      </div>

      <div className="flex flex-col gap-2.5">
        {spans.map((span, i) => {
          const { color, label } = band(span.category);
          const left = (span.start_us / total) * 100;
          // Sub-millisecond stages would round to an invisible bar; floor the width so
          // a fast governance check still reads as "it ran".
          const width = Math.max((span.dur_us / total) * 100, 0.8);
          const isOpen = openIndex === i;

          return (
            <div key={i} className="flex flex-col gap-1">
              <button
                type="button"
                onClick={() => setOpenIndex(isOpen ? null : i)}
                aria-expanded={isOpen}
                className="flex items-center gap-1.5 rounded text-left focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring"
                style={{ paddingLeft: `${span.depth * 10}px` }}
              >
                <span
                  className="size-1.5 shrink-0 rounded-[2px]"
                  style={{ background: color }}
                  aria-hidden
                />
                <span className="truncate font-mono text-[11px]">{span.name}</span>
                {span.outcome && (
                  <span
                    className="shrink-0 rounded-full border px-1.5 text-[9px] uppercase tracking-wide"
                    style={{
                      color: span.category === "failed" ? "var(--error)" : "var(--muted-foreground)",
                      borderColor:
                        span.category === "failed" ? "var(--error)" : "var(--border)",
                    }}
                  >
                    {span.outcome}
                  </span>
                )}
                <span className="ml-auto shrink-0 font-mono text-[10px] tabular-nums text-muted-foreground">
                  {formatDuration(span.dur_us)}
                </span>
              </button>

              <div
                className="h-1.5 rounded"
                style={{ background: "var(--border)", marginLeft: `${span.depth * 10}px` }}
                role="img"
                aria-label={`${span.name}: ${label}, starts at ${formatDuration(
                  span.start_us,
                )}, lasts ${formatDuration(span.dur_us)}`}
              >
                <div
                  className="h-1.5 rounded transition-all"
                  style={{
                    background: color,
                    marginLeft: `${left}%`,
                    width: `${Math.min(width, 100 - left)}%`,
                  }}
                />
              </div>

              {isOpen && (
                <div className="rounded-md border px-2.5 py-2 text-[11px] text-muted-foreground">
                  <div className="flex flex-wrap gap-x-4 gap-y-0.5 font-mono">
                    <span>{label}</span>
                    <span>starts +{formatDuration(span.start_us)}</span>
                    <span>takes {formatDuration(span.dur_us)}</span>
                  </div>
                  {span.detail && <p className="mt-1">{span.detail}</p>}
                </div>
              )}
            </div>
          );
        })}
      </div>

      <div className="flex flex-wrap gap-x-3 gap-y-1 pt-0.5">
        {categoriesPresent.map((c) => (
          <span key={c} className="flex items-center gap-1.5 text-[10px] text-muted-foreground">
            <span
              className="size-1.5 rounded-[2px]"
              style={{ background: band(c).color }}
              aria-hidden
            />
            {band(c).label}
          </span>
        ))}
      </div>
    </div>
  );
}
