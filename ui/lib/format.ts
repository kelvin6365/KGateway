// Shared formatting helpers for the dashboard (cost, counts, durations, relative time).
// Kept framework-free so any client component can import them.

/** USD cost, trimmed to a readable precision (sub-cent shown to 4dp). */
export function formatCost(v: number | null | undefined): string {
  if (v == null) return "—";
  if (v === 0) return "$0";
  return v < 0.01 ? `$${v.toFixed(4)}` : `$${v.toFixed(2)}`;
}

/** Integer count with thousands separators. */
export function formatCount(v: number): string {
  return Math.round(v).toLocaleString();
}

/** Large counts compacted: 896210 → "896K", 1_240_000 → "1.2M". */
export function formatCompact(v: number): string {
  if (v < 1000) return Math.round(v).toString();
  if (v < 1_000_000) {
    const k = v / 1000;
    return `${k < 10 ? k.toFixed(1) : Math.round(k)}K`;
  }
  const m = v / 1_000_000;
  return `${m < 10 ? m.toFixed(1) : Math.round(m)}M`;
}

/** A duration in ms as a compact human string ("2.4s", "3m 12s", "1h 4m"). */
export function formatDuration(ms: number): string {
  if (ms < 0) ms = 0;
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(s < 10 ? 1 : 0)}s`;
  const m = Math.floor(s / 60);
  const rem = Math.round(s % 60);
  if (m < 60) return rem ? `${m}m ${rem}s` : `${m}m`;
  const h = Math.floor(m / 60);
  const mm = m % 60;
  return mm ? `${h}h ${mm}m` : `${h}h`;
}

/** Relative time from a unix-ms timestamp ("just now", "5m ago", "3h ago", or a date). */
export function formatRelative(ts: number, now = Date.now()): string {
  const diff = now - ts;
  if (diff < 0) return "just now";
  const s = Math.floor(diff / 1000);
  if (s < 45) return "just now";
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.floor(h / 24);
  if (d < 7) return `${d}d ago`;
  return new Date(ts).toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
    year: "numeric",
  });
}

/** Absolute clock time for a unix-ms timestamp. */
export function formatClock(ts: number): string {
  return new Date(ts).toLocaleTimeString(undefined, {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}
