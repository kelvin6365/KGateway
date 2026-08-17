"use client";

import { useQuery } from "@tanstack/react-query";
import { AuthRequiredError, getStatus, getLogStats, getLogs } from "@/lib/api";
import { TokenGate } from "@/components/token-gate";
import { Card, CardContent } from "@/components/ui/card";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { OrnamentDivider } from "@/components/baroque/ornament-divider";
import { EmptyState } from "@/components/baroque/empty-state";
import { formatDateTime } from "@/lib/format";

function StatTile({ label, value, note }: { label: string; value: string; note?: string }) {
  return (
    <Card>
      <CardContent>
        <div className="text-xs uppercase tracking-wide text-muted-foreground">{label}</div>
        <div className="mt-2 text-2xl font-semibold">{value}</div>
        {note && <div className="mt-1 text-xs text-muted-foreground">{note}</div>}
      </CardContent>
    </Card>
  );
}

function ConfigTile({ label, value }: { label: string; value: string }) {
  return (
    <Card>
      <CardContent>
        <div className="text-xs uppercase tracking-wide text-muted-foreground">{label}</div>
        <div className="mt-2 truncate font-mono text-sm font-medium" title={value}>
          {value}
        </div>
      </CardContent>
    </Card>
  );
}

export default function CachePage() {
  const {
    data: status,
    isLoading: statusLoading,
    isError: statusError,
    error: statusErrorObj,
  } = useQuery({
    queryKey: ["status"],
    queryFn: getStatus,
    retry: false,
  });

  const semanticCache = status?.semantic_cache ?? null;

  const { data: stats } = useQuery({
    queryKey: ["logs-stats"],
    queryFn: () => getLogStats(),
    enabled: !!semanticCache,
    retry: false,
    refetchInterval: 5000,
  });

  const {
    data: cacheLogs,
    isLoading: cacheLogsLoading,
    isError: cacheLogsError,
  } = useQuery({
    queryKey: ["logs-cache-hits"],
    queryFn: () => getLogs({ cache_hit: true, limit: 10 }),
    enabled: !!semanticCache,
    retry: false,
    refetchInterval: 10000,
  });

  const authError = statusErrorObj instanceof AuthRequiredError;

  const hits = stats?.cache_hits ?? 0;
  const total = stats?.total ?? 0;
  const hitRate = total > 0 ? (hits / total) * 100 : 0;

  return (
    <div className="flex flex-col gap-6">
      <TokenGate need="token">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="font-display text-3xl font-semibold tracking-wide">Cache</h1>
          <p className="text-sm text-muted-foreground">
            Semantic response cache — embedding-similarity matches served without hitting
            the upstream provider.
          </p>
        </div>
      </div>

      {statusError ? (
        <EmptyState
          title="Could not load cache status"
          hint={
            authError
              ? "Access token required — see Settings."
              : "The gateway did not respond to GET /api/status."
          }
        />
      ) : !statusLoading && !semanticCache ? (
        <EmptyState
          title="Semantic cache is not configured"
          hint="Set a `semantic_cache` block in your gateway config (embedding_provider, embedding_model, threshold) to enable response caching by embedding similarity. See docs/16-configuration.md for the full schema."
        />
      ) : (
        <>
          <div className="grid grid-cols-2 gap-4 sm:grid-cols-3 lg:grid-cols-5">
            <StatTile label="Cache hits" value={hits.toLocaleString()} />
            <StatTile
              label="Hit rate"
              value={`${hitRate.toFixed(1)}%`}
              note={`of ${total.toLocaleString()} requests`}
            />
            <ConfigTile label="Embedding provider" value={semanticCache?.embedding_provider ?? "—"} />
            <ConfigTile label="Embedding model" value={semanticCache?.embedding_model ?? "—"} />
            <ConfigTile
              label="Threshold"
              value={semanticCache ? semanticCache.threshold.toFixed(3) : "—"}
            />
          </div>

          <OrnamentDivider />

          <div>
            <h2 className="mb-3 font-display text-lg font-semibold tracking-wide">
              Recent cache-served requests
            </h2>
            {cacheLogsError ? (
              <EmptyState
                title="Could not load recent cache hits"
                hint="The gateway did not respond to GET /api/logs."
              />
            ) : !cacheLogsLoading && (cacheLogs?.logs.length ?? 0) === 0 ? (
              <EmptyState
                title="No cache hits yet"
                hint="Once a request matches a prior response within the similarity threshold, it will appear here."
              />
            ) : (
              <div className="overflow-hidden rounded-xl border bg-card">
                <Table className="text-sm">
                  <TableHeader>
                    <TableRow className="text-left text-xs uppercase tracking-wide text-muted-foreground hover:bg-transparent">
                      <TableHead className="px-4 py-3 font-medium">Time</TableHead>
                      <TableHead className="px-4 py-3 font-medium">Model</TableHead>
                      <TableHead className="px-4 py-3 font-medium">Tokens</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {(cacheLogs?.logs ?? []).map((l) => (
                      <TableRow key={l.request_id}>
                        <TableCell className="whitespace-nowrap px-4 py-3 text-muted-foreground">
                          {formatDateTime(l.created_at)}
                        </TableCell>
                        <TableCell className="px-4 py-3 font-medium">{l.model}</TableCell>
                        <TableCell className="px-4 py-3 text-muted-foreground">
                          {l.prompt_tokens.toLocaleString()} / {l.completion_tokens.toLocaleString()}
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </div>
            )}
          </div>
        </>
      )}
      </TokenGate>
    </div>
  );
}
