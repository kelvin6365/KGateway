"use client";

import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { getStatus, getAdminToken, setAdminToken, type PluginStatus } from "@/lib/api";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Badge } from "@/components/ui/badge";
import { EmptyState } from "@/components/baroque/empty-state";

/** "content_capture" -> "Content Capture" */
function humanize(name: string): string {
  return name
    .split(/[_-]/)
    .map((w) => (w ? w[0].toUpperCase() + w.slice(1) : w))
    .join(" ");
}

function PluginCard({ plugin }: { plugin: PluginStatus }) {
  return (
    <Card>
      <CardContent className="flex flex-col gap-2">
        <div className="flex items-center justify-between gap-2">
          <div className="text-base font-semibold">{humanize(plugin.name)}</div>
          <Badge
            variant={plugin.enabled ? "default" : "outline"}
            className={plugin.enabled ? "bg-success text-primary-foreground" : "text-muted-foreground"}
          >
            {plugin.enabled ? "Enabled" : "Disabled"}
          </Badge>
        </div>
        <div className="text-sm text-muted-foreground">{plugin.description}</div>
      </CardContent>
    </Card>
  );
}

export default function PluginsPage() {
  const qc = useQueryClient();
  const [adminTok, setAdminTok] = useState("");
  const [showAdmin, setShowAdmin] = useState(false);

  const {
    data: status,
    isLoading,
    isError,
    error,
  } = useQuery({
    queryKey: ["status"],
    queryFn: getStatus,
    retry: false,
  });

  const authError = (error as Error | undefined)?.message === "admin token required";

  const plugins = [...(status?.plugins ?? [])].sort((a, b) => {
    if (a.enabled === b.enabled) return 0;
    return a.enabled ? -1 : 1;
  });

  function saveAdmin() {
    setAdminToken(adminTok);
    setShowAdmin(false);
    qc.invalidateQueries();
  }

  return (
    <div className="flex flex-col gap-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="font-display text-3xl font-semibold tracking-wide">Plugins</h1>
          <p className="text-sm text-muted-foreground">
            The request pipeline — every request runs through these observers/plugins in
            order. Enable a stage by configuring it in your gateway config.
          </p>
        </div>
        <button
          onClick={() => {
            setAdminTok(getAdminToken());
            setShowAdmin((s) => !s);
          }}
          className="text-xs text-muted-foreground underline"
        >
          {getAdminToken() ? "admin token set" : "set admin token"}
        </button>
      </div>

      {showAdmin && (
        <Card>
          <CardContent className="flex flex-col gap-2">
            <Label>
              Admin token (only needed if the gateway has <code>admin_token</code> set)
            </Label>
            <div className="flex gap-2">
              <Input
                type="password"
                value={adminTok}
                onChange={(e) => setAdminTok(e.target.value)}
                placeholder="Bearer token for /api/*"
              />
              <Button onClick={saveAdmin}>Save</Button>
            </div>
          </CardContent>
        </Card>
      )}

      {isError ? (
        <EmptyState
          title="Could not load plugins"
          hint={
            authError
              ? "The gateway requires an admin token — click ‘set admin token’ above."
              : "The gateway did not respond to GET /api/status."
          }
        />
      ) : plugins.length === 0 && !isLoading ? (
        <EmptyState
          title="No plugins reported"
          hint="The gateway did not report any pipeline stages."
        />
      ) : (
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {plugins.map((p) => (
            <PluginCard key={p.name} plugin={p} />
          ))}
        </div>
      )}
    </div>
  );
}
