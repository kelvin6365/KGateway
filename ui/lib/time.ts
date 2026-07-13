// Shared time-range helpers for the dashboard and analytics views.
// The Logs page still keeps a local copy of its 15m/1h/24h/all subset
// (see app/logs/page.tsx) — TODO: absorb it here in a later pass.

export type TimeRange = "15m" | "1h" | "24h" | "7d" | "all";

/** Start of the window as unix ms for GET /api/logs* `since_ms`, or undefined for "all". */
export function sinceMsForRange(range: TimeRange, now = Date.now()): number | undefined {
  switch (range) {
    case "15m":
      return now - 15 * 60 * 1000;
    case "1h":
      return now - 60 * 60 * 1000;
    case "24h":
      return now - 24 * 60 * 60 * 1000;
    case "7d":
      return now - 7 * 24 * 60 * 60 * 1000;
    case "all":
    default:
      return undefined;
  }
}

/** Picks a timeseries bucket size so each range renders a readable number of bars. */
export function bucketMsForRange(range: TimeRange): number {
  switch (range) {
    case "15m":
      return 30_000; // 30s buckets -> ~30 bars
    case "1h":
      return 120_000; // 2m buckets -> ~30 bars
    case "24h":
      return 3_600_000; // 1h buckets -> 24 bars
    case "7d":
      return 21_600_000; // 6h buckets -> 28 bars
    case "all":
    default:
      return 3_600_000; // 1h buckets
  }
}
