// Client-side derivation that turns a raw SessionSummary into operator-meaningful signals:
// is it live, is it erroring, is it looping (runaway agent), how does its cost compare. All
// computed from the fields the /api/sessions summary already returns — no extra API calls.

import type { SessionSummary } from "@/lib/api";

/** A session counts as "live" if its last call landed within this window. */
export const LIVE_WINDOW_MS = 5 * 60 * 1000;

/** Above this call rate (with enough calls to be meaningful) a session looks like a burst
 *  or a runaway/looping agent — the thing an operator most wants to catch on a gateway. */
const BUSY_CALLS_PER_MIN = 20;
const BUSY_MIN_CALLS = 8;

export type SessionHealth = "error" | "live" | "busy" | "idle";

export interface SessionInsight {
  live: boolean;
  /** Wall-clock span of the session, ms. */
  spanMs: number;
  /** Calls per minute over the span (bursts/loops run hot). */
  callsPerMin: number;
  errorRate: number; // 0..1
  cacheRate: number; // 0..1
  busy: boolean; // possible loop / heavy burst
  health: SessionHealth;
  /** Ranking weight for the "Attention" sort — errors dominate, then live, then loops, then cost. */
  attentionScore: number;
}

export function insight(s: SessionSummary, now: number): SessionInsight {
  const spanMs = Math.max(0, s.last_ts - s.first_ts);
  const minutes = Math.max(spanMs / 60000, 1 / 60); // floor at 1s to avoid div blow-ups
  const callsPerMin = s.call_count / minutes;
  const errorRate = s.call_count > 0 ? s.error_count / s.call_count : 0;
  const cacheRate = s.call_count > 0 ? s.cache_hits / s.call_count : 0;
  const live = now - s.last_ts <= LIVE_WINDOW_MS;
  const busy = callsPerMin >= BUSY_CALLS_PER_MIN && s.call_count >= BUSY_MIN_CALLS;

  const health: SessionHealth =
    s.error_count > 0 ? "error" : live ? "live" : busy ? "busy" : "idle";

  // Errors first (×1000), then live (300), then loop-like bursts (200), then cost and rate as
  // tiebreakers. Keeps the most actionable sessions at the top of the default view.
  const attentionScore =
    s.error_count * 1000 +
    (live ? 300 : 0) +
    (busy ? 200 : 0) +
    s.total_cost * 10 +
    Math.min(callsPerMin, 120);

  return { live, spanMs, callsPerMin, errorRate, cacheRate, busy, health, attentionScore };
}

export interface SessionsOverview {
  /** Sessions active within LIVE_WINDOW_MS. */
  active: number;
  /** Total sessions matching the query (server-reported, may exceed the loaded page). */
  total: number;
  /** Loaded on this page (the KPI sums below are over these). */
  loaded: number;
  spend: number;
  tokens: number;
  calls: number;
  errored: number; // sessions with at least one error
  avgCost: number;
  maxCost: number; // for the per-row cost bars
}

export function overview(
  sessions: SessionSummary[],
  total: number,
  now: number,
): SessionsOverview {
  let active = 0;
  let spend = 0;
  let tokens = 0;
  let calls = 0;
  let errored = 0;
  let maxCost = 0;
  for (const s of sessions) {
    if (now - s.last_ts <= LIVE_WINDOW_MS) active++;
    spend += s.total_cost;
    tokens += s.total_tokens;
    calls += s.call_count;
    if (s.error_count > 0) errored++;
    if (s.total_cost > maxCost) maxCost = s.total_cost;
  }
  return {
    active,
    total,
    loaded: sessions.length,
    spend,
    tokens,
    calls,
    errored,
    avgCost: sessions.length > 0 ? spend / sessions.length : 0,
    maxCost,
  };
}

export type ClientSort = "attention" | "recent" | "cost" | "tokens" | "calls";

/** Sort loaded sessions client-side so we can offer the derived "Attention" order alongside
 *  the plain metric sorts. (Over the loaded page — the API caps how many we fetch.) */
export function sortSessions(
  sessions: SessionSummary[],
  by: ClientSort,
  now: number,
): SessionSummary[] {
  const arr = [...sessions];
  const cmp: Record<ClientSort, (a: SessionSummary, b: SessionSummary) => number> = {
    attention: (a, b) => insight(b, now).attentionScore - insight(a, now).attentionScore,
    recent: (a, b) => b.last_ts - a.last_ts,
    cost: (a, b) => b.total_cost - a.total_cost,
    tokens: (a, b) => b.total_tokens - a.total_tokens,
    calls: (a, b) => b.call_count - a.call_count,
  };
  return arr.sort(cmp[by]);
}
