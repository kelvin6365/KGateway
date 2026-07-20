"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import {
  BookOpen,
  LayoutDashboard,
  MessageSquare,
  Boxes,
  KeyRound,
  ScrollText,
  Database,
  Wrench,
  Puzzle,
  Settings,
  Sun,
  Moon,
} from "lucide-react";
import { OrnamentDivider } from "@/components/baroque/ornament-divider";
import { useTheme } from "@/components/baroque/use-theme";
import { cn } from "@/lib/utils";

const NAV = [
  { href: "/", label: "Dashboard", icon: LayoutDashboard },
  { href: "/playground", label: "Playground", icon: MessageSquare },
  { href: "/providers", label: "Providers", icon: Boxes },
  { href: "/virtual-keys", label: "Virtual Keys", icon: KeyRound },
  { href: "/logs", label: "Logs", icon: ScrollText },
  { href: "/cache", label: "Cache", icon: Database },
  { href: "/mcp", label: "MCP", icon: Wrench },
  { href: "/plugins", label: "Plugins", icon: Puzzle },
  { href: "/docs", label: "API Docs", icon: BookOpen },
  { href: "/settings", label: "Settings", icon: Settings },
];

export function Sidebar() {
  const pathname = usePathname();
  const [theme, setTheme] = useTheme();
  return (
    <aside className="flex w-60 shrink-0 flex-col gap-1 border-r border-sidebar-border bg-sidebar/80 p-3 backdrop-blur-md">
      <div className="mb-1 flex items-center gap-3 px-2 py-2">
        <div className="glow-primary border-engraved flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 font-wordmark text-sm font-bold text-primary">
          KG
        </div>
        <div>
          <div className="font-wordmark text-sm font-semibold leading-tight tracking-wide text-sidebar-foreground">
            KGateway
          </div>
          <div className="text-[11px] uppercase tracking-[0.18em] text-muted-foreground">
            AI Gateway
          </div>
        </div>
      </div>
      <OrnamentDivider className="mb-3 px-1" />
      {NAV.map(({ href, label, icon: Icon }) => {
        const active = href === "/" ? pathname === "/" : pathname.startsWith(href);
        return (
          <Link
            key={href}
            href={href}
            className={cn(
              "relative flex items-center gap-3 rounded-md px-3 py-2 text-sm transition-colors",
              active
                ? "bg-sidebar-accent text-sidebar-primary"
                : "text-sidebar-foreground/70 hover:bg-sidebar-accent/60 hover:text-sidebar-foreground",
            )}
          >
            {/* gilt rule marking the active leaf */}
            <span
              aria-hidden
              className={cn(
                "absolute inset-y-1.5 left-0 w-px rounded-full transition-opacity",
                active ? "glow-primary bg-primary opacity-100" : "opacity-0",
              )}
            />
            <Icon size={16} className={active ? "glow-primary" : undefined} />
            {label}
          </Link>
        );
      })}
      {/* Theme switch: Midnight (dark) ⇄ Porcelain (light) */}
      <div className="mt-auto">
        <OrnamentDivider className="mb-2 px-1" />
        <button
          onClick={() => setTheme(theme === "dark" ? "light" : "dark")}
          className="flex w-full items-center gap-3 rounded-md px-3 py-2 text-sm text-sidebar-foreground/70 transition-colors hover:bg-sidebar-accent/60 hover:text-sidebar-foreground"
          title={theme === "dark" ? "Switch to Porcelain (light)" : "Switch to Midnight (dark)"}
        >
          {theme === "dark" ? <Sun size={16} /> : <Moon size={16} />}
          {theme === "dark" ? "Porcelain" : "Midnight"}
        </button>
      </div>
    </aside>
  );
}
