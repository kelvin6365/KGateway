"use client";

// Who am I signed in as? Lives at the bottom of the sidebar: identity kind + role at a
// glance, permissions on hover, and a link to Settings to change the token.

import Link from "next/link";
import { KeyRound, ShieldCheck, ShieldOff, User } from "lucide-react";
import { useAuth } from "@/lib/auth-context";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

export function IdentityBadge() {
  const { status, identity } = useAuth();

  let icon = <User size={16} />;
  let label = "…";
  let sub: string | null = null;
  let tone = "text-sidebar-foreground/70";

  switch (status) {
    case "loading":
      label = "Resolving identity";
      break;
    case "error":
      label = "Gateway unreachable";
      tone = "text-destructive";
      break;
    case "open":
      icon = <ShieldOff size={16} />;
      label = "Open mode";
      sub = "no auth configured";
      break;
    case "unauthenticated":
      icon = <KeyRound size={16} />;
      label = "Not signed in";
      sub = "token required";
      tone = "text-warning";
      break;
    case "authed":
      if (identity?.kind === "virtual_key") {
        icon = <KeyRound size={16} className="glow-primary" />;
        label = identity.name || identity.scoped_to || "virtual key";
        sub = "virtual key · scoped";
      } else {
        icon = <ShieldCheck size={16} className="glow-primary" />;
        label = identity?.name || String(identity?.role ?? "token");
        sub = identity?.role ? `token · ${identity.role}` : "token";
      }
      tone = "text-sidebar-foreground";
      break;
  }

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Link
          href="/settings"
          className={cn(
            "flex items-center gap-3 rounded-md px-3 py-2 text-sm transition-colors hover:bg-sidebar-accent/60",
            tone,
          )}
        >
          {icon}
          <span className="min-w-0">
            <span className="block truncate leading-tight">{label}</span>
            {sub && (
              <span className="block text-[10px] uppercase tracking-[0.14em] text-muted-foreground">
                {sub}
              </span>
            )}
          </span>
        </Link>
      </TooltipTrigger>
      <TooltipContent side="right" className="max-w-60">
        {status === "authed" && identity ? (
          <div className="flex flex-col gap-1 text-xs">
            {identity.kind === "virtual_key" ? (
              <p>
                Signed in with a virtual key — this dashboard shows only that key&apos;s
                own traffic.
              </p>
            ) : (
              <p>Signed in with a control-plane access token.</p>
            )}
            <p className="text-muted-foreground">
              Permissions: {identity.permissions.join(", ") || "none"}
            </p>
            <p className="text-muted-foreground">Change the token in Settings.</p>
          </div>
        ) : status === "open" ? (
          <p className="text-xs">
            The gateway has no tokens or virtual keys configured — every dashboard view
            is open with full (admin) access.
          </p>
        ) : status === "unauthenticated" ? (
          <p className="text-xs">Set an access token or virtual key in Settings.</p>
        ) : (
          <p className="text-xs">Identity comes from GET /api/whoami.</p>
        )}
      </TooltipContent>
    </Tooltip>
  );
}
