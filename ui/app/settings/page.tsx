"use client";

import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Sun, Moon } from "lucide-react";
import { getStatus, getAdminToken, setAdminToken, type StatusFeatures } from "@/lib/api";
import { Card, CardHeader, CardTitle, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Badge } from "@/components/ui/badge";
import { OrnamentDivider } from "@/components/baroque/ornament-divider";
import { EmptyState } from "@/components/baroque/empty-state";
import { useTheme } from "@/components/baroque/use-theme";

function Row({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-4 border-b py-2 text-sm last:border-b-0">
      <span className="text-muted-foreground">{label}</span>
      <span className="text-right font-mono text-xs">{value}</span>
    </div>
  );
}

const FEATURE_LABELS: Record<keyof StatusFeatures, string> = {
  content_logging: "Content logging",
  redaction: "Redaction",
  semantic_cache: "Semantic cache",
  governance: "Governance",
  mcp: "MCP",
  otlp: "OTLP",
};

export default function SettingsPage() {
  const qc = useQueryClient();
  const [adminTok, setAdminTok] = useState("");
  const [showAdmin, setShowAdmin] = useState(false);
  const [tokenVersion, setTokenVersion] = useState(0);
  const [hasToken, setHasToken] = useState(false);
  const [theme, setTheme] = useTheme();

  useEffect(() => {
    setHasToken(!!getAdminToken());
  }, [tokenVersion]);

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

  function saveAdmin() {
    setAdminToken(adminTok);
    setShowAdmin(false);
    setTokenVersion((v) => v + 1);
    qc.invalidateQueries();
  }

  function clearAdmin() {
    setAdminToken("");
    setAdminTok("");
    setTokenVersion((v) => v + 1);
    qc.invalidateQueries();
  }

  return (
    <div className="flex flex-col gap-6">
      <div>
        <h1 className="font-display text-3xl font-semibold tracking-wide">Settings</h1>
        <p className="text-sm text-muted-foreground">
          Read-only summary of the gateway&apos;s live configuration, plus the admin token
          used by this dashboard for control-plane calls.
        </p>
      </div>

      {/* Admin token */}
      <Card>
        <CardHeader>
          <CardTitle>Admin token</CardTitle>
        </CardHeader>
        <CardContent className="flex flex-col gap-3">
          <div className="text-sm text-muted-foreground">
            Status:{" "}
            {hasToken ? (
              <span className="font-medium text-success">set</span>
            ) : (
              <span className="font-medium">not set</span>
            )}
            . Only needed if the gateway has <code>admin_token</code> configured. Stored in
            this browser&apos;s <code>localStorage</code> and sent as{" "}
            <code>Authorization: Bearer &lt;token&gt;</code> to all <code>/api/*</code> calls.
          </div>
          {!showAdmin ? (
            <div>
              <Button
                variant="outline"
                onClick={() => {
                  setAdminTok(getAdminToken());
                  setShowAdmin(true);
                }}
              >
                {hasToken ? "Change token" : "Set token"}
              </Button>
            </div>
          ) : (
            <div className="flex flex-col gap-2">
              <Label>Admin token</Label>
              <div className="flex gap-2">
                <Input
                  type="password"
                  value={adminTok}
                  onChange={(e) => setAdminTok(e.target.value)}
                  placeholder="Bearer token for /api/*"
                />
                <Button onClick={saveAdmin}>Save</Button>
                <Button variant="ghost" onClick={() => setShowAdmin(false)}>
                  Cancel
                </Button>
              </div>
              {hasToken && (
                <div>
                  <button
                    onClick={clearAdmin}
                    className="text-xs text-muted-foreground underline"
                  >
                    Clear stored token
                  </button>
                </div>
              )}
            </div>
          )}
        </CardContent>
      </Card>

      {/* Appearance */}
      <Card>
        <CardHeader>
          <CardTitle>Appearance</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="flex items-center justify-between gap-4">
            <div className="text-sm text-muted-foreground">
              Theme — {theme === "dark" ? "Midnight (dark)" : "Porcelain (light)"}
            </div>
            <Button
              variant="outline"
              onClick={() => setTheme(theme === "dark" ? "light" : "dark")}
            >
              {theme === "dark" ? <Sun size={15} /> : <Moon size={15} />}
              {theme === "dark" ? "Switch to Porcelain" : "Switch to Midnight"}
            </Button>
          </div>
        </CardContent>
      </Card>

      <OrnamentDivider />

      {/* Config summary */}
      {isError ? (
        <EmptyState
          title="Could not load gateway status"
          hint={
            authError
              ? "The gateway requires an admin token — set one above."
              : "The gateway did not respond to GET /api/status."
          }
        />
      ) : !status && isLoading ? (
        <EmptyState title="Loading configuration…" hint="Fetching GET /api/status." />
      ) : status ? (
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
          <Card>
            <CardHeader>
              <CardTitle>Runtime</CardTitle>
            </CardHeader>
            <CardContent className="flex flex-col">
              <Row label="Version" value={status.version} />
              <Row label="Port" value={status.port} />
              <Row label="Database" value={status.database} />
              <Row
                label="Auth"
                value={status.auth === "enabled" ? "enabled" : "open"}
              />
              <Row
                label="Log retention"
                value={
                  status.log_retention_days ? `${status.log_retention_days} days` : "unlimited"
                }
              />
              <Row label="Request timeout" value={`${status.request_timeout_secs}s`} />
              <Row
                label="CORS origins"
                value={
                  status.cors_allow_origins && status.cors_allow_origins.length > 0
                    ? status.cors_allow_origins.join(", ")
                    : "permissive"
                }
              />
              <Row label="Redaction reveal" value={status.redaction_reveal ? "enabled" : "disabled"} />
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle>Providers &amp; keys</CardTitle>
            </CardHeader>
            <CardContent className="flex flex-col">
              <Row label="Provider count" value={status.providers.length} />
              <Row
                label="Providers"
                value={status.providers.length ? status.providers.join(", ") : "—"}
              />
              <Row label="Virtual keys" value={status.virtual_keys_count} />
            </CardContent>
          </Card>

          <Card className="lg:col-span-2">
            <CardHeader>
              <CardTitle>Features</CardTitle>
            </CardHeader>
            <CardContent>
              <div className="flex flex-wrap gap-2">
                {(Object.keys(FEATURE_LABELS) as (keyof StatusFeatures)[]).map((key) => {
                  const enabled = status.features[key];
                  return (
                    <Badge
                      key={key}
                      variant={enabled ? "default" : "outline"}
                      className={enabled ? "bg-success text-primary-foreground" : "text-muted-foreground"}
                    >
                      {FEATURE_LABELS[key]} · {enabled ? "on" : "off"}
                    </Badge>
                  );
                })}
              </div>
            </CardContent>
          </Card>
        </div>
      ) : null}
    </div>
  );
}
