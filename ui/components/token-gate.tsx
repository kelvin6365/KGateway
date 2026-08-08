"use client";

// The one token prompt. Wrap a page (or a section) in <TokenGate> and it renders the
// children when the caller may see them, or a single, consistent sign-in card when the
// gateway wants a credential — replacing the per-page prompts this dashboard used to
// duplicate.

import { useState } from "react";
import { KeyRound } from "lucide-react";
import { useAuth } from "@/lib/auth-context";
import { Card, CardHeader, CardTitle, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";

interface TokenGateProps {
  children: React.ReactNode;
  /**
   * What the gated content needs beyond plain read access:
   * - "token": a control-plane token of any role — virtual keys are turned away
   *   (config pages, metrics; their APIs reject scoped callers).
   * - "write": the `config:write` permission (operator / admin).
   * - "reveal": the `logs:reveal` permission (admin).
   * Omit for content any accepted identity may see (logs, sessions, analytics).
   */
  need?: "token" | "write" | "reveal";
}

export function TokenGate({ children, need }: TokenGateProps) {
  const { status, identity, isScoped, can } = useAuth();

  // Open gateway, still resolving, or unreachable: let the page render (it owns its
  // loading / error states — an unreachable gateway is not an auth problem).
  if (status === "loading" || status === "open" || status === "error") {
    return <>{children}</>;
  }

  if (status === "unauthenticated") {
    return <TokenPrompt />;
  }

  // Authed. Scoped (virtual-key) identities can't reach token-only surfaces.
  if (need && isScoped) {
    return (
      <GateCard title="Admin credential required">
        You are signed in with a <strong>virtual key</strong>
        {identity?.scoped_to ? (
          <>
            {" "}
            (<code>{identity.scoped_to}</code>)
          </>
        ) : null}
        , which only sees its own traffic on the Logs, Sessions and Dashboard views. This
        page needs a control-plane <strong>access token</strong> — switch tokens below.
        <div className="mt-3">
          <TokenPrompt inline />
        </div>
      </GateCard>
    );
  }
  if (need === "write" && !can("config:write")) {
    return (
      <GateCard title="Operator access required">
        Your token&apos;s role is <code>{identity?.role ?? "viewer"}</code>, which can
        view but not change configuration. Editing here needs an{" "}
        <code>operator</code> or <code>admin</code> token.
        <div className="mt-3">
          <TokenPrompt inline />
        </div>
      </GateCard>
    );
  }
  if (need === "reveal" && !can("logs:reveal")) {
    return (
      <GateCard title="Admin access required">
        Revealing redacted content needs an <code>admin</code> token; your token&apos;s
        role is <code>{identity?.role ?? "viewer"}</code>.
        <div className="mt-3">
          <TokenPrompt inline />
        </div>
      </GateCard>
    );
  }

  return <>{children}</>;
}

function GateCard({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <Card className="mx-auto mt-8 max-w-xl">
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <KeyRound size={16} className="text-primary" />
          {title}
        </CardTitle>
      </CardHeader>
      <CardContent className="text-sm text-muted-foreground">{children}</CardContent>
    </Card>
  );
}

/**
 * The sign-in form itself. Standalone card by default; `inline` renders just the
 * input row for embedding inside another card.
 */
export function TokenPrompt({ inline = false }: { inline?: boolean }) {
  const { hasToken, status, setToken, clearToken } = useAuth();
  const [draft, setDraft] = useState("");

  const form = (
    <div className="flex flex-col gap-2">
      {hasToken && status === "unauthenticated" && (
        <p className="text-xs text-destructive">
          The stored token was rejected by the gateway — enter a different one.
        </p>
      )}
      <div className="flex gap-2">
        <Input
          type="password"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && draft) setToken(draft);
          }}
          placeholder="Access token or virtual key"
          aria-label="Access token"
        />
        <Button onClick={() => draft && setToken(draft)}>Sign in</Button>
      </div>
      {hasToken && (
        <div>
          <button
            onClick={clearToken}
            className="text-xs text-muted-foreground underline"
          >
            Clear stored token
          </button>
        </div>
      )}
    </div>
  );

  if (inline) return form;

  return (
    <Card className="mx-auto mt-8 max-w-xl">
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <KeyRound size={16} className="text-primary" />
          Sign in to the gateway
        </CardTitle>
      </CardHeader>
      <CardContent className="flex flex-col gap-3">
        <p className="text-sm text-muted-foreground">
          This gateway requires a credential for its dashboard APIs. Two kinds work here:
        </p>
        <ul className="ml-4 list-disc text-sm text-muted-foreground">
          <li>
            <strong className="text-foreground">Access token</strong> — an{" "}
            <code>admin_token</code> / <code>api_tokens</code> entry (viewer, operator or
            admin). Sees traffic from <em>all</em> virtual keys; the role decides whether
            it can also edit config or reveal redacted content.
          </li>
          <li>
            <strong className="text-foreground">Virtual key</strong> — a data-plane API
            key. Signs in <em>scoped</em>: it sees only the logs, sessions and analytics
            of its own traffic.
          </li>
        </ul>
        <div className="flex flex-col gap-2">
          <Label>Token</Label>
          {form}
        </div>
        <p className="text-xs text-muted-foreground">
          Stored in this browser&apos;s <code>localStorage</code> and sent as{" "}
          <code>Authorization: Bearer &lt;token&gt;</code> on every <code>/api/*</code>{" "}
          call.
        </p>
      </CardContent>
    </Card>
  );
}
