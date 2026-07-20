"use client";

// API reference. Everything on this page is rendered from the gateway's own
// /openapi.json, which is generated from the route table — so the page cannot describe
// an endpoint the gateway doesn't serve, and no content is duplicated into the UI.

import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Check, Copy, Download, ExternalLink } from "lucide-react";
import { BASE_URL, getOpenApi, type OpenApiSpec } from "@/lib/api";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { cn } from "@/lib/utils";

type Lang = "curl" | "python" | "javascript";

interface Operation {
  method: string;
  path: string;
  summary: string;
  description: string;
  group: string;
  auth: string;
  slug: string;
  params: {
    name: string;
    location: string;
    type: string;
    required: boolean;
    description: string;
  }[];
  curl: string;
  order: number;
}

const METHOD_COLOR: Record<string, string> = {
  GET: "var(--success)",
  POST: "var(--chart-1)",
  PUT: "var(--warning)",
  DELETE: "var(--error)",
};

/** Flatten the spec into a render-ready list, preserving tag order for the sidebar. */
function toOperations(spec: OpenApiSpec | undefined): Operation[] {
  if (!spec?.paths) return [];
  const out: Operation[] = [];
  for (const [path, item] of Object.entries(spec.paths)) {
    for (const method of ["get", "post", "put", "delete"] as const) {
      const op = item[method];
      if (!op) continue;
      const body =
        op.requestBody?.content?.["application/json"]?.schema ?? undefined;
      const bodyRequired: string[] = body?.required ?? [];
      out.push({
        method: method.toUpperCase(),
        path,
        summary: op.summary ?? "",
        description: op.description ?? "",
        group: op.tags?.[0] ?? "Other",
        auth: op["x-kgateway-auth"] ?? "none",
        slug: (op.operationId ?? "").replace(/_/g, "-"),
        params: [
          ...(op.parameters ?? []).map((p) => ({
            name: p.name,
            location: p.in,
            type: p.schema?.type ?? "string",
            required: !!p.required,
            description: p.description ?? "",
          })),
          ...Object.entries(body?.properties ?? {}).map(([name, s]) => ({
            name,
            location: "body",
            type: s.type ?? "string",
            required: bodyRequired.includes(name),
            description: s.description ?? "",
          })),
        ],
        curl: op["x-codeSamples"]?.[0]?.source ?? "",
        order: op["x-order"] ?? Number.MAX_SAFE_INTEGER,
      });
      // OpenAPI `properties` is an object, so its order is lost in serialization.
      // Required first is the reading order that matters: `model` and `messages`
      // belong above `temperature`, not alphabetically below `fallbacks`.
      out[out.length - 1].params.sort((a, b) =>
        a.required === b.required ? a.name.localeCompare(b.name) : a.required ? -1 : 1,
      );
    }
  }
  // The spec's paths serialize in sorted key order; x-order carries the catalog's
  // intended reading order, which leads with the endpoints people came for.
  return out.sort((a, b) => a.order - b.order);
}

/**
 * Derive Python / JavaScript from the curl sample rather than storing three copies of
 * every example in the catalog — three copies is three chances to drift.
 *
 * Returns null when the example can't be faithfully translated (a multipart upload, or
 * a shell snippet that isn't a single curl). Showing a sample that silently drops the
 * file or the query string is worse than showing none: the reader copies it, gets a
 * 400, and blames the API.
 */
function asLanguage(op: Operation, lang: Lang): string | null {
  if (lang === "curl") return op.curl;
  // `-F` is multipart; there's no honest one-line requests/fetch equivalent here.
  if (/\s-F\s/.test(op.curl)) return null;

  // Take the URL from the example itself so query strings and filled-in path params
  // survive; rebuilding it from op.path drops `?token=…` and leaves `{id}` literal.
  const urlMatch = op.curl.match(/https?:\/\/[^\s'"]+/);
  if (!urlMatch) return null;
  const url = urlMatch[0].replace("http://localhost:8080", BASE_URL);

  const bodyParams = op.params.filter((p) => p.location === "body");
  const bodyMatch = op.curl.match(/-d '([\s\S]*?)'/);
  const body = bodyMatch?.[1]?.trim();
  const needsAuth = op.auth !== "none";

  if (lang === "python") {
    const lines = ["import requests", ""];
    if (needsAuth) lines.push('headers = {"authorization": "Bearer " + TOKEN}', "");
    const args = [`"${url}"`];
    if (needsAuth) args.push("headers=headers");
    if (body && bodyParams.length) args.push(`json=${body}`);
    lines.push(
      `resp = requests.${op.method.toLowerCase()}(${args.join(", ")})`,
      "print(resp.json())",
    );
    return lines.join("\n");
  }

  const init: string[] = [`method: "${op.method}"`];
  const headers: string[] = [];
  if (body && bodyParams.length) headers.push('"content-type": "application/json"');
  if (needsAuth) headers.push('authorization: `Bearer ${TOKEN}`');
  if (headers.length) init.push(`headers: { ${headers.join(", ")} }`);
  if (body && bodyParams.length) init.push(`body: JSON.stringify(${body})`);
  return [
    `const resp = await fetch("${url}", {`,
    `  ${init.join(",\n  ")},`,
    "});",
    "console.log(await resp.json());",
  ].join("\n");
}

function CopyButton({ text, label = "Copy" }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(text);
          setCopied(true);
          setTimeout(() => setCopied(false), 1500);
        } catch {
          // Clipboard unavailable (insecure context) — nothing sensible to do.
        }
      }}
      className="inline-flex items-center gap-1 rounded text-xs text-muted-foreground outline-none hover:text-foreground focus-visible:ring-3 focus-visible:ring-ring/50"
    >
      {copied ? <Check size={12} /> : <Copy size={12} />}
      {copied ? "Copied" : label}
    </button>
  );
}

function EndpointCard({ op }: { op: Operation }) {
  const [lang, setLang] = useState<Lang>("curl");
  const sample = asLanguage(op, lang);

  return (
    <Card id={op.slug} className="scroll-mt-6 overflow-hidden py-0">
      <div className="flex flex-wrap items-center gap-2 border-b px-4 py-3">
        <span
          className="rounded border px-1.5 py-0.5 font-mono text-[10px] font-semibold"
          style={{ color: METHOD_COLOR[op.method], borderColor: METHOD_COLOR[op.method] }}
        >
          {op.method}
        </span>
        <span className="font-mono text-sm">{op.path}</span>
        <span className="ml-auto rounded-full border px-2 py-0.5 font-mono text-[10px] uppercase tracking-wide text-muted-foreground">
          {op.auth}
        </span>
      </div>

      <CardContent className="flex flex-col gap-4 px-4 py-4">
        <p className="text-sm text-muted-foreground">{op.description || op.summary}</p>

        {op.params.length > 0 && (
          <div className="flex flex-col gap-2">
            <span className="text-[10px] font-semibold uppercase tracking-wide text-muted-foreground">
              Parameters
            </span>
            <div className="overflow-x-auto">
              <table className="w-full min-w-[520px] text-xs">
                <thead>
                  <tr className="text-[10px] uppercase tracking-wide text-muted-foreground">
                    <th className="border-b py-1.5 pr-3 text-left font-medium">Name</th>
                    <th className="border-b py-1.5 pr-3 text-left font-medium">In</th>
                    <th className="border-b py-1.5 pr-3 text-left font-medium">Type</th>
                    <th className="border-b py-1.5 text-left font-medium">Description</th>
                  </tr>
                </thead>
                <tbody>
                  {op.params.map((p) => (
                    <tr key={`${p.location}-${p.name}`}>
                      <td className="border-b py-1.5 pr-3 align-top font-mono whitespace-nowrap">
                        {p.name}
                        {p.required && (
                          <span className="ml-1 text-[9px] uppercase" style={{ color: "var(--error)" }}>
                            req
                          </span>
                        )}
                      </td>
                      <td className="border-b py-1.5 pr-3 align-top text-muted-foreground">
                        {p.location}
                      </td>
                      <td className="border-b py-1.5 pr-3 align-top font-mono text-muted-foreground">
                        {p.type}
                      </td>
                      <td className="border-b py-1.5 align-top text-muted-foreground">
                        {p.description}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>
        )}

        <div className="flex flex-col gap-2">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <ToggleGroup
              type="single"
              value={lang}
              onValueChange={(v) => v && setLang(v as Lang)}
              variant="outline"
              size="sm"
              spacing={0}
              className="overflow-hidden rounded-md"
            >
              <ToggleGroupItem value="curl" className="px-2 py-1 text-xs">
                cURL
              </ToggleGroupItem>
              <ToggleGroupItem value="python" className="px-2 py-1 text-xs">
                Python
              </ToggleGroupItem>
              <ToggleGroupItem value="javascript" className="px-2 py-1 text-xs">
                JavaScript
              </ToggleGroupItem>
            </ToggleGroup>
            <div className="flex items-center gap-3">
              <a
                href={`${BASE_URL}/docs/${op.slug}.md`}
                target="_blank"
                rel="noreferrer"
                className="inline-flex items-center gap-1 rounded text-xs text-muted-foreground outline-none hover:text-foreground focus-visible:ring-3 focus-visible:ring-ring/50"
              >
                <ExternalLink size={12} />
                Markdown
              </a>
              {sample && <CopyButton text={sample} />}
            </div>
          </div>
          {sample ? (
            <pre className="overflow-x-auto rounded-md border bg-background/60 px-3 py-2 font-mono text-xs whitespace-pre">
              {sample}
            </pre>
          ) : (
            <div className="rounded-md border px-3 py-2 text-xs text-muted-foreground">
              This example is a multipart upload; see the cURL tab, which is exact.
            </div>
          )}
        </div>
      </CardContent>
    </Card>
  );
}

export default function DocsPage() {
  const { data: spec, isLoading, isError } = useQuery({
    queryKey: ["openapi"],
    queryFn: getOpenApi,
    retry: false,
    staleTime: 300000,
  });

  const operations = useMemo(() => toOperations(spec), [spec]);
  const groups = useMemo(() => {
    const order: string[] = spec?.tags?.map((t) => t.name) ?? [];
    const seen = new Map<string, Operation[]>();
    for (const g of order) seen.set(g, []);
    for (const op of operations) {
      if (!seen.has(op.group)) seen.set(op.group, []);
      seen.get(op.group)!.push(op);
    }
    return Array.from(seen.entries()).filter(([, ops]) => ops.length > 0);
  }, [operations, spec]);

  const [active, setActive] = useState<string>("");
  useEffect(() => {
    // Highlight the endpoint currently in view.
    const observer = new IntersectionObserver(
      (entries) => {
        const visible = entries.filter((e) => e.isIntersecting);
        if (visible.length > 0) setActive(visible[0].target.id);
      },
      { rootMargin: "-10% 0px -75% 0px" },
    );
    for (const el of document.querySelectorAll("[id^='get-'],[id^='post-'],[id^='put-'],[id^='delete-']")) {
      observer.observe(el);
    }
    return () => observer.disconnect();
  }, [operations]);

  /** The whole reference as Markdown, for pasting into an issue or an agent. */
  const pageMarkdown = useMemo(
    () =>
      groups
        .map(
          ([group, ops]) =>
            `# ${group}\n\n` +
            ops
              .map(
                (op) =>
                  `## ${op.method} ${op.path}\n\n> ${op.summary}\n\n**Auth:** ${op.auth}\n\n${op.description}\n\n\`\`\`bash\n${op.curl}\n\`\`\`\n`,
              )
              .join("\n"),
        )
        .join("\n"),
    [groups],
  );

  return (
    <div className="flex flex-col gap-6">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="font-display text-3xl font-semibold tracking-wide">API reference</h1>
          <p className="text-sm text-muted-foreground">
            Every endpoint this gateway serves, generated from its own route table.
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <CopyButton text={pageMarkdown} label="Copy page" />
          <Button variant="outline" size="sm" className="text-xs" asChild>
            <a href={`${BASE_URL}/llms.txt`} target="_blank" rel="noreferrer">
              <Download size={12} />
              llms.txt
            </a>
          </Button>
          <Button variant="outline" size="sm" className="text-xs" asChild>
            <a href={`${BASE_URL}/llms-full.txt`} target="_blank" rel="noreferrer">
              <Download size={12} />
              llms-full.txt
            </a>
          </Button>
          <Button variant="outline" size="sm" className="text-xs" asChild>
            <a href={`${BASE_URL}/openapi.json`} target="_blank" rel="noreferrer">
              <Download size={12} />
              openapi.json
            </a>
          </Button>
        </div>
      </div>

      {isLoading && <Skeleton className="h-96 w-full rounded-xl" />}

      {isError && (
        <Card className="py-4">
          <CardContent className="text-sm text-muted-foreground">
            Could not reach the gateway at{" "}
            <code className="font-mono">{BASE_URL}</code>. The reference is generated by the
            gateway itself, so it needs to be running.
          </CardContent>
        </Card>
      )}

      {operations.length > 0 && (
        <div className="grid gap-6 lg:grid-cols-[210px_minmax(0,1fr)]">
          <nav className="top-6 hidden h-fit flex-col gap-4 lg:sticky lg:flex">
            {groups.map(([group, ops]) => (
              <div key={group} className="flex flex-col gap-1">
                <span className="text-[10px] font-semibold uppercase tracking-wide text-muted-foreground">
                  {group}
                </span>
                {ops.map((op) => (
                  <a
                    key={op.slug}
                    href={`#${op.slug}`}
                    className={cn(
                      "truncate rounded px-2 py-0.5 font-mono text-[11px] outline-none focus-visible:ring-3 focus-visible:ring-ring/50",
                      active === op.slug
                        ? "bg-accent text-foreground"
                        : "text-muted-foreground hover:text-foreground",
                    )}
                  >
                    <span
                      className="mr-1.5 text-[9px] font-semibold"
                      style={{ color: METHOD_COLOR[op.method] }}
                    >
                      {op.method}
                    </span>
                    {op.path}
                  </a>
                ))}
              </div>
            ))}
          </nav>

          <div className="flex min-w-0 flex-col gap-8">
            {groups.map(([group, ops]) => (
              <section key={group} className="flex flex-col gap-3">
                <h2 className="font-display text-xl font-semibold tracking-wide">{group}</h2>
                {ops.map((op) => (
                  <EndpointCard key={op.slug} op={op} />
                ))}
              </section>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
