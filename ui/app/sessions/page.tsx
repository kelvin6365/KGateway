"use client";

// Sessions — grouped AI-usage journeys. Each row aggregates one session's calls
// (from the x-session-id header, or the OpenAI `user` / Anthropic metadata.user_id
// hint). Click through to the per-session journey (timeline + Sankey diagrams).

import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import Link from "next/link";
import { Route } from "lucide-react";
import {
  getSessions,
  getAdminToken,
  setAdminToken,
  type SessionSort,
  type SessionSummary,
} from "@/lib/api";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { SegmentedControl } from "@/components/charts";
import {
  formatCost,
  formatRelative,
  formatDuration,
  formatCount,
} from "@/lib/format";

const SORTS: { value: SessionSort; label: string }[] = [
  { value: "recent", label: "Recent" },
  { value: "cost", label: "Cost" },
  { value: "tokens", label: "Tokens" },
  { value: "calls", label: "Calls" },
];

function StatChip({ label, value }: { label: string; value: string }) {
  return (
    <span className="inline-flex items-baseline gap-1 text-xs">
      <span className="tabular-nums font-medium">{value}</span>
      <span className="text-muted-foreground">{label}</span>
    </span>
  );
}

function SessionRow({ s }: { s: SessionSummary }) {
  return (
    <Link
      href={`/sessions/${encodeURIComponent(s.session_id)}`}
      className="flex flex-col gap-2 rounded-lg border border-border p-3 transition-colors hover:bg-accent/50 focus-visible:ring-3 focus-visible:ring-ring/50 focus-visible:outline-none"
    >
      <div className="flex flex-wrap items-center justify-between gap-2">
        <span className="truncate font-mono text-sm font-medium" title={s.session_id}>
          {s.session_id}
        </span>
        <span className="shrink-0 text-xs text-muted-foreground">
          {formatRelative(s.last_ts)}
        </span>
      </div>
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1">
        <StatChip label="calls" value={formatCount(s.call_count)} />
        <StatChip label="tokens" value={formatCount(s.total_tokens)} />
        <StatChip label="" value={formatCost(s.total_cost)} />
        {s.error_count > 0 && (
          <span className="inline-flex items-baseline gap-1 text-xs" style={{ color: "var(--error)" }}>
            <span className="tabular-nums font-medium">{s.error_count}</span>
            <span>errors</span>
          </span>
        )}
        {s.cache_hits > 0 && <StatChip label="cache hits" value={formatCount(s.cache_hits)} />}
        <StatChip label="span" value={formatDuration(s.last_ts - s.first_ts)} />
      </div>
      <div className="flex flex-wrap items-center gap-1.5">
        {s.models.slice(0, 5).map((m) => (
          <span
            key={m}
            className="rounded-full border border-border px-2 py-0.5 font-mono text-[10px] text-muted-foreground"
          >
            {m}
          </span>
        ))}
        {s.models.length > 5 && (
          <span className="text-[10px] text-muted-foreground">+{s.models.length - 5}</span>
        )}
      </div>
    </Link>
  );
}

export default function SessionsPage() {
  const [sort, setSort] = useState<SessionSort>("recent");
  const [adminTok, setAdminTokState] = useState("");
  const [hasToken, setHasToken] = useState(false);

  useEffect(() => {
    const t = getAdminToken();
    setAdminTokState(t);
    setHasToken(!!t);
  }, []);

  function saveToken() {
    setAdminToken(adminTok);
    setHasToken(!!adminTok);
  }

  const { data, isLoading, isError, error } = useQuery({
    queryKey: ["sessions", sort],
    queryFn: () => getSessions({ sort, limit: 100 }),
    retry: false,
    refetchInterval: 10000,
  });

  const sessions = useMemo(() => data?.sessions ?? [], [data]);
  const needsToken = (error as Error | undefined)?.message === "admin token required";

  return (
    <div className="flex flex-col gap-6">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="font-display text-3xl font-semibold tracking-wide">Sessions</h1>
          <p className="text-sm text-muted-foreground">
            Every AI-usage session, grouped from its calls — click one to see its full journey.
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <SegmentedControl value={sort} options={SORTS} onChange={setSort} />
          <Input
            type="password"
            value={adminTok}
            onChange={(e) => setAdminTokState(e.target.value)}
            onBlur={saveToken}
            onKeyDown={(e) => e.key === "Enter" && saveToken()}
            placeholder="admin token"
            className="h-9 w-40 text-xs"
          />
        </div>
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

      {!isLoading && !isError && sessions.length === 0 && (
        <Card className="py-10">
          <CardContent className="flex flex-col items-center gap-2 text-center text-sm text-muted-foreground">
            <Route size={24} className="opacity-60" />
            <p>No sessions yet.</p>
            <p className="max-w-md text-xs">
              Send requests with an <code className="font-mono">x-session-id</code> header (or an
              OpenAI <code className="font-mono">user</code> field / Anthropic{" "}
              <code className="font-mono">metadata.user_id</code>) and they&apos;ll group here.
            </p>
          </CardContent>
        </Card>
      )}

      {sessions.length > 0 && (
        <div className="flex flex-col gap-2">
          {sessions.map((s) => (
            <SessionRow key={s.session_id} s={s} />
          ))}
          {hasToken && data && (
            <p className="px-1 pt-1 text-xs text-muted-foreground">
              {sessions.length} of {data.total} sessions
            </p>
          )}
        </div>
      )}
    </div>
  );
}
