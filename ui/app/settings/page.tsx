"use client";

import { useQuery } from "@tanstack/react-query";
import { Sun, Moon } from "lucide-react";
import { AuthRequiredError, getStatus, type StatusFeatures } from "@/lib/api";
import { useAuth } from "@/lib/auth-context";
import { TokenGate, TokenPrompt } from "@/components/token-gate";
import { Card, CardHeader, CardTitle, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
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

/** Who the stored credential resolves to, as a row of badges. */
function IdentityBadges() {
  const { identity, status, hasToken } = useAuth();

  if (status === "loading") {
    return <span className="text-sm text-muted-foreground">Resolving identity…</span>;
  }
  if (status === "open") {
    return (
      <div className="flex flex-wrap items-center gap-2 text-sm">
        <Badge variant="outline">open gateway</Badge>
        <span className="text-muted-foreground">
          No <code>admin_token</code>, <code>api_tokens</code> or virtual keys are configured
          — every caller has full access.
        </span>
      </div>
    );
  }
  if (status !== "authed" || !identity) {
    return (
      <div className="flex flex-wrap items-center gap-2 text-sm">
        <Badge variant="outline" className="text-muted-foreground">
          {hasToken ? "token rejected" : "not signed in"}
        </Badge>
        <span className="text-muted-foreground">
          {status === "error"
            ? "The gateway did not answer GET /api/whoami."
            : "This gateway requires a credential — enter one below."}
        </span>
      </div>
    );
  }

  const isVkey = identity.kind === "virtual_key";
  return (
    <div className="flex flex-wrap items-center gap-2 text-sm">
      <Badge variant="default">{isVkey ? "virtual key" : "access token"}</Badge>
      {identity.role && <Badge variant="outline">role: {identity.role}</Badge>}
      {identity.name && <span className="font-medium">{identity.name}</span>}
      {identity.scoped_to && (
        <Badge variant="outline" className="font-mono text-muted-foreground">
          scoped to {identity.scoped_to}
        </Badge>
      )}
    </div>
  );
}

export default function SettingsPage() {
  const [theme, setTheme] = useTheme();

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

  return (
    <div className="flex flex-col gap-6">
      <div>
        <h1 className="font-display text-3xl font-semibold tracking-wide">Settings</h1>
        <p className="text-sm text-muted-foreground">
          Read-only summary of the gateway&apos;s live configuration, plus the credential
          this dashboard uses for control-plane calls.
        </p>
      </div>

      {/* Tokens & access — deliberately outside the gate so the credential can always
          be changed, including after signing in with one that can't see this page. */}
      <Card>
        <CardHeader>
          <CardTitle>Tokens &amp; access</CardTitle>
        </CardHeader>
        <CardContent className="flex flex-col gap-4">
          <div className="flex flex-col gap-2">
            <div className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
              Signed in as
            </div>
            <IdentityBadges />
          </div>

          <div className="flex flex-col gap-2">
            <div className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
              Change credential
            </div>
            <TokenPrompt inline />
            <p className="text-xs text-muted-foreground">
              Stored in this browser&apos;s <code>localStorage</code> and sent as{" "}
              <code>Authorization: Bearer &lt;token&gt;</code> on every <code>/api/*</code>{" "}
              call.
            </p>
          </div>

          <div className="flex flex-col gap-2 border-t pt-4">
            <div className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
              How access works
            </div>
            <ul className="ml-4 flex list-disc flex-col gap-1.5 text-sm text-muted-foreground">
              <li>
                <code>admin_token</code> — a single admin access token: full read, config
                writes, and reveal of redacted content.
              </li>
              <li>
                <code>api_tokens</code> — named tokens with a role. <strong>viewer</strong>{" "}
                reads all data; <strong>operator</strong> adds config writes;{" "}
                <strong>admin</strong> adds revealing redacted content.
              </li>
              <li>
                <strong>Virtual keys</strong> — data-plane API keys. They may also sign in
                here, but every read is scoped to their own traffic.
              </li>
            </ul>
          </div>
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

      {/* Config summary — /api/status rejects virtual keys, so it needs a real token. */}
      <TokenGate need="token">
      {isError ? (
        <EmptyState
          title="Could not load gateway status"
          hint={
            error instanceof AuthRequiredError
              ? "Access token required — see Tokens & access above."
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
      </TokenGate>
    </div>
  );
}
