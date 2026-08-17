"use client";

// "Star on GitHub" — sidebar link to the repo with a live star count. The count is
// best-effort (unauthenticated GitHub API, cached for an hour); when the fetch fails
// or is rate-limited the button still renders, just without a number.

import { Github, Star } from "lucide-react";
import { useQuery } from "@tanstack/react-query";

const REPO_URL = "https://github.com/kelvin6365/KGateway";
const REPO_API = "https://api.github.com/repos/kelvin6365/KGateway";

async function fetchStarCount(): Promise<number> {
  const res = await fetch(REPO_API, {
    headers: { accept: "application/vnd.github+json" },
  });
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  const body = (await res.json()) as { stargazers_count?: number };
  if (typeof body.stargazers_count !== "number") throw new Error("no count");
  return body.stargazers_count;
}

/** 1234 → "1.2k"; below 1000 the plain number. */
function formatStars(n: number): string {
  if (n >= 1000) {
    const k = n / 1000;
    return `${k >= 10 ? Math.round(k) : Math.round(k * 10) / 10}k`;
  }
  return String(n);
}

export function GitHubStarButton() {
  const { data: stars } = useQuery({
    queryKey: ["github-stars"],
    queryFn: fetchStarCount,
    staleTime: 3_600_000,
    gcTime: 3_600_000,
    retry: false,
    refetchOnWindowFocus: false,
  });

  return (
    <a
      href={REPO_URL}
      target="_blank"
      rel="noreferrer"
      className="flex w-full items-center gap-3 rounded-md px-3 py-2 text-sm text-sidebar-foreground/70 transition-colors hover:bg-sidebar-accent/60 hover:text-sidebar-foreground"
      title="Star KGateway on GitHub"
    >
      <Github size={16} />
      Star on GitHub
      {stars !== undefined && (
        <span className="ml-auto inline-flex items-center gap-1 rounded-full border border-sidebar-border px-1.5 py-0.5 text-[11px] text-muted-foreground">
          <Star size={11} className="text-primary" aria-hidden />
          {formatStars(stars)}
        </span>
      )}
    </a>
  );
}
