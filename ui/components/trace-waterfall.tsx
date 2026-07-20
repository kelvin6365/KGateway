"use client";

// Request trace waterfall: every stage of one AI call on a shared timeline —
// governance, cache, each dispatch attempt (including the ones that failed before
// the successful retry), time-to-first-token, and the write-back.
//
// Three fixed columns: stage name | timeline | duration. Durations live in their own
// column rather than floating beside their bar — a bar that starts at zero and runs
// the full width leaves a floating label nowhere to go, and it ends up on top of the
// bar or the stage name.

import { useMemo, useState } from "react";
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

/**
 * Stages faster than this are hidden by default. A governance check that takes 40µs
 * is real but tells the reader nothing; one that takes 5ms is a finding, and stays
 * visible because the threshold is on duration, not on the stage's name.
 */
const TRIVIAL_US = 1000;

/**
 * ...except when the stage carries a finding. A failed attempt is usually the FASTEST
 * span in a failover trace — a refused connection dies in under a millisecond — and it
 * is the whole reason someone opened the trace. Duration alone would hide it.
 */
const isTrivial = (s: TraceSpan) =>
  s.dur_us < TRIVIAL_US && s.category !== "failed" && !s.outcome;

/** Durations span microseconds to seconds; keep every row readable at its own scale. */
function formatDuration(us: number): string {
  const ms = us / 1000;
  if (ms >= 1000) return `${(ms / 1000).toFixed(2)} s`;
  if (ms >= 10) return `${Math.round(ms)} ms`;
  if (ms >= 1) return `${ms.toFixed(1)} ms`;
  return `${Math.round(us)} µs`;
}

/** name | timeline | duration — shared by the ruler and every row so they stay aligned. */
const GRID =
  "grid grid-cols-[minmax(120px,260px)_minmax(0,1fr)_120px] items-center gap-3";

export function TraceWaterfall({ spans }: { spans: TraceSpan[] }) {
  const [openIndex, setOpenIndex] = useState<number | null>(null);
  const [showTrivial, setShowTrivial] = useState(false);

  // The timeline runs to the end of the last stage, not the longest one — a stage
  // starting late must not overflow the track.
  const total = useMemo(
    () => Math.max(...spans.map((s) => s.start_us + s.dur_us), 1),
    [spans],
  );
  // Keep each span's original index so selection survives filtering.
  const indexed = useMemo(() => spans.map((span, index) => ({ span, index })), [spans]);
  const trivialCount = useMemo(() => spans.filter(isTrivial).length, [spans]);
  const visible = useMemo(
    () => (showTrivial ? indexed : indexed.filter(({ span }) => !isTrivial(span))),
    [indexed, showTrivial],
  );

  if (spans.length === 0) return null;

  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <span className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
          Trace
        </span>
        <span className="font-mono text-[11px] tabular-nums text-muted-foreground">
          {visible.length} of {spans.length} stages · {formatDuration(total)}
        </span>
      </div>

      <div className="overflow-x-auto">
        <div className="min-w-[420px]">
          {/* Ruler — the shared scale every bar below is measured against. */}
          <div className={`${GRID} border-b pb-1`}>
            <span className="text-[10px] uppercase tracking-wide text-muted-foreground">
              Stage
            </span>
            {/* Absolute offsets, not `justify-between`: the latter spaces boxes of
                differing widths evenly, so a label's centre drifts off the gridline it
                annotates and a bar reads against the wrong time. */}
            <div className="relative h-4">
              {[0, 0.25, 0.5, 0.75, 1].map((t, i, arr) => (
                <span
                  key={t}
                  className="absolute top-0 font-mono text-[10px] tabular-nums whitespace-nowrap text-muted-foreground"
                  style={{
                    left: `${t * 100}%`,
                    transform:
                      i === 0
                        ? "none"
                        : i === arr.length - 1
                          ? "translateX(-100%)"
                          : "translateX(-50%)",
                  }}
                >
                  {t === 0 ? "0" : formatDuration(total * t)}
                </span>
              ))}
            </div>
            <span className="text-right text-[10px] uppercase tracking-wide text-muted-foreground">
              Took
            </span>
          </div>

          <div className="flex flex-col pt-1">
            {visible.map(({ span, index }) => {
              const { color, label } = band(span.category);
              const left = (span.start_us / total) * 100;
              const width = Math.max((span.dur_us / total) * 100, 0.6);
              const isOpen = openIndex === index;

              return (
                <div key={index} className="flex flex-col">
                  <button
                    type="button"
                    onClick={() => setOpenIndex(isOpen ? null : index)}
                    aria-expanded={isOpen}
                    title={span.name}
                    className={`${GRID} rounded py-1.5 text-left outline-none hover:bg-accent focus-visible:ring-3 focus-visible:ring-ring/50`}
                  >
                    <span
                      className="flex min-w-0 items-center gap-1.5"
                      style={{ paddingLeft: `${span.depth * 10}px` }}
                    >
                      <span
                        className="size-2 shrink-0 rounded-[2px]"
                        style={{ background: color }}
                        aria-hidden
                      />
                      <span className="truncate font-mono text-[11px]">{span.name}</span>
                    </span>

                    <span
                      className="relative block h-5"
                      role="img"
                      aria-label={`${span.name}: ${label}, starts at ${formatDuration(
                        span.start_us,
                      )}, lasts ${formatDuration(span.dur_us)}`}
                    >
                      {/* Quarter gridlines, matching the ruler's ticks. */}
                      <span
                        className="absolute inset-0"
                        style={{
                          backgroundImage:
                            "repeating-linear-gradient(to right, var(--border) 0 1px, transparent 1px 25%)",
                          opacity: 0.55,
                        }}
                        aria-hidden
                      />
                      <span
                        className="absolute top-1 h-3 rounded-[3px]"
                        style={{
                          background: color,
                          left: `${left}%`,
                          width: `${Math.min(width, 100 - left)}%`,
                        }}
                      />
                    </span>

                    <span className="flex items-center justify-end gap-1">
                      {span.outcome && (
                        <span
                          className="shrink-0 rounded-full border px-1 text-[9px] uppercase"
                          style={{
                            color:
                              span.category === "failed"
                                ? "var(--error)"
                                : "var(--muted-foreground)",
                            borderColor:
                              span.category === "failed" ? "var(--error)" : "var(--border)",
                          }}
                        >
                          {span.outcome}
                        </span>
                      )}
                      <span className="font-mono text-[11px] tabular-nums whitespace-nowrap text-muted-foreground">
                        {formatDuration(span.dur_us)}
                      </span>
                    </span>
                  </button>

                  {isOpen && (
                    <div className="mb-1 rounded-md border px-3 py-2 text-xs text-muted-foreground">
                      <p className="font-mono text-[11px] break-all text-foreground">
                        {span.name}
                      </p>
                      <div className="mt-1 flex flex-wrap gap-x-5 gap-y-0.5 font-mono text-[11px]">
                        <span>{label}</span>
                        <span>starts +{formatDuration(span.start_us)}</span>
                        <span>takes {formatDuration(span.dur_us)}</span>
                      </div>
                      {span.detail && <p className="mt-1.5">{span.detail}</p>}
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        </div>
      </div>

      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
        <div className="flex flex-wrap gap-x-3 gap-y-1">
          {Array.from(new Set(visible.map(({ span }) => span.category))).map((c) => (
            <span
              key={c}
              className="flex items-center gap-1.5 text-[11px] text-muted-foreground"
            >
              <span
                className="size-2 rounded-[2px]"
                style={{ background: band(c).color }}
                aria-hidden
              />
              {band(c).label}
            </span>
          ))}
        </div>
        {trivialCount > 0 && (
          <button
            type="button"
            onClick={() => setShowTrivial((v) => !v)}
            className="rounded text-[11px] text-muted-foreground underline underline-offset-2 outline-none hover:text-foreground focus-visible:ring-3 focus-visible:ring-ring/50"
          >
            {showTrivial ? "Hide" : "Show"} {trivialCount} sub-millisecond{" "}
            {trivialCount === 1 ? "stage" : "stages"}
          </button>
        )}
      </div>
    </div>
  );
}
