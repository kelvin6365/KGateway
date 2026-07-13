"use client";

// Providers page: a brand-logo catalog grid to connect any
// supported provider (click a tile → config sheet, prefilled with the right
// kind/base-URL semantics), and logo cards for everything already configured.
// Changes persist to config.json and hot-reload without a restart.

import { useState } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { Check, Pencil, Trash2 } from "lucide-react";
import {
  getProviders,
  putProvider,
  deleteProvider,
  getAdminToken,
  setAdminToken,
  type ProviderConfigInput,
} from "@/lib/api";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Badge } from "@/components/ui/badge";
import {
  Select,
  SelectTrigger,
  SelectValue,
  SelectContent,
  SelectItem,
} from "@/components/ui/select";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { OrnamentDivider } from "@/components/baroque/ornament-divider";
import { EmptyState } from "@/components/baroque/empty-state";
import { RenderProviderIcon } from "@/lib/provider-icons";

const CAP_COLORS: Record<string, string> = {
  chat: "#4f46e5",
  embeddings: "#0891b2",
  images: "#7c3aed",
  audio: "#db2777",
  rerank: "#16a34a",
};

/**
 * Everything the gateway can connect to, with the field semantics each entry
 * needs (mirrors the kind/name dispatch in kgateway-server/src/app.rs):
 *  - native wire formats: openai / anthropic / cohere / gemini / azure / bedrock
 *  - known OpenAI-compatible vendors: name-inferred, default base URL built in
 *  - custom: any name + explicit kind + base URL
 */
interface CatalogEntry {
  name: string; // default routing prefix (editable in the sheet)
  label: string;
  kind?: string; // explicit kind sent to the API (omitted → name inference)
  base_url?: string; // prefilled AND sent (e.g. zai)
  defaultBaseUrl?: string; // server-side default — shown as placeholder, not sent
  baseUrlRequired?: boolean;
  baseUrlLabel?: string; // e.g. bedrock uses base_url for the AWS region
  keyPlaceholder?: string;
  keyHint?: string;
}

const CATALOG: CatalogEntry[] = [
  { name: "openai", label: "OpenAI", defaultBaseUrl: "https://api.openai.com/v1" },
  { name: "anthropic", label: "Anthropic", defaultBaseUrl: "https://api.anthropic.com" },
  { name: "gemini", label: "Google Gemini", kind: "gemini", defaultBaseUrl: "https://generativelanguage.googleapis.com/v1beta" },
  {
    name: "azure",
    label: "Azure OpenAI",
    kind: "azure",
    baseUrlRequired: true,
    baseUrlLabel: "Resource endpoint",
    defaultBaseUrl: "https://<resource>.openai.azure.com",
    keyHint: "Model in requests = your deployment name.",
  },
  {
    name: "bedrock",
    label: "AWS Bedrock",
    kind: "bedrock",
    baseUrlLabel: "AWS region",
    defaultBaseUrl: "us-east-1",
    keyPlaceholder: "ACCESS_KEY_ID:SECRET_ACCESS_KEY",
    keyHint: "Key value is ACCESS_KEY_ID:SECRET_ACCESS_KEY.",
  },
  { name: "cohere", label: "Cohere" },
  { name: "groq", label: "Groq", defaultBaseUrl: "https://api.groq.com/openai/v1" },
  { name: "openrouter", label: "OpenRouter", defaultBaseUrl: "https://openrouter.ai/api/v1" },
  { name: "xai", label: "xAI Grok", defaultBaseUrl: "https://api.x.ai/v1" },
  { name: "deepseek", label: "DeepSeek", defaultBaseUrl: "https://api.deepseek.com" },
  { name: "cerebras", label: "Cerebras", defaultBaseUrl: "https://api.cerebras.ai/v1" },
  { name: "perplexity", label: "Perplexity", defaultBaseUrl: "https://api.perplexity.ai" },
  { name: "together", label: "Together AI", defaultBaseUrl: "https://api.together.xyz/v1" },
  { name: "mistral", label: "Mistral", defaultBaseUrl: "https://api.mistral.ai/v1" },
  { name: "nebius", label: "Nebius", defaultBaseUrl: "https://api.studio.nebius.ai/v1" },
  { name: "huggingface", label: "Hugging Face", defaultBaseUrl: "https://router.huggingface.co/v1" },
  {
    name: "zai",
    label: "z.ai GLM",
    kind: "anthropic",
    base_url: "https://api.z.ai/api/anthropic",
    keyHint: "Anthropic-compatible — GLM Coding Plan keys work here.",
  },
  {
    name: "ollama",
    label: "Ollama",
    defaultBaseUrl: "http://localhost:11434/v1",
    keyPlaceholder: "ollama",
    keyHint: "Local server — the key can be any placeholder value.",
  },
  {
    name: "vllm",
    label: "vLLM",
    defaultBaseUrl: "http://localhost:8000/v1",
    keyPlaceholder: "vllm",
    keyHint: "Self-hosted — the key can be any placeholder value.",
  },
  {
    name: "sglang",
    label: "SGLang",
    defaultBaseUrl: "http://localhost:30000/v1",
    keyPlaceholder: "sglang",
    keyHint: "Self-hosted — the key can be any placeholder value.",
  },
  {
    name: "",
    label: "Custom",
    baseUrlRequired: true,
    keyHint: "Any OpenAI- or Anthropic-compatible endpoint.",
  },
];

const CUSTOM_KINDS = [
  { value: "openai", label: "OpenAI-compatible" },
  { value: "anthropic", label: "Anthropic-compatible" },
];

export default function ProvidersPage() {
  const qc = useQueryClient();
  const [adminTok, setAdminTok] = useState("");
  const [showAdmin, setShowAdmin] = useState(false);

  const { data: providers = [], isLoading, isError, error } = useQuery({
    queryKey: ["providers"],
    queryFn: getProviders,
    retry: false,
  });
  const configuredNames = new Set(providers.map((p) => p.name));

  // --- config sheet ---
  const [entry, setEntry] = useState<CatalogEntry | null>(null);
  const [name, setName] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [customKind, setCustomKind] = useState("openai");
  const [apiKey, setApiKey] = useState("");
  const [formErr, setFormErr] = useState<string | null>(null);

  function openSheet(e: CatalogEntry) {
    setEntry(e);
    setName(e.name);
    setBaseUrl(e.base_url ?? "");
    setCustomKind("openai");
    setApiKey("");
    setFormErr(null);
  }

  /** Open the sheet for an already-configured provider (re-enter key to update). */
  function openForConfigured(providerName: string) {
    const known = CATALOG.find((c) => c.name === providerName);
    if (known) {
      openSheet(known);
    } else {
      openSheet({ ...CATALOG[CATALOG.length - 1], name: providerName });
    }
  }

  const upsert = useMutation({
    mutationFn: (vars: { name: string; config: ProviderConfigInput }) =>
      putProvider(vars.name, vars.config),
    onSuccess: () => {
      setEntry(null);
      qc.invalidateQueries({ queryKey: ["providers"] });
    },
    onError: (e: Error) => setFormErr(e.message),
  });

  const remove = useMutation({
    mutationFn: (n: string) => deleteProvider(n),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["providers"] }),
  });

  function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!entry) return;
    if (!name.trim()) return setFormErr("provider name (routing prefix) is required");
    if (entry.baseUrlRequired && !baseUrl.trim())
      return setFormErr(`${entry.baseUrlLabel ?? "base URL"} is required for ${entry.label}`);
    if (!apiKey.trim()) return setFormErr("API key is required");
    const isCustom = entry.label === "Custom";
    const config: ProviderConfigInput = {
      kind: isCustom ? customKind : entry.kind,
      base_url: baseUrl.trim() || entry.base_url,
      keys: [{ id: "default", value: apiKey.trim(), weight: 1 }],
    };
    upsert.mutate({ name: name.trim().toLowerCase(), config });
  }

  function saveAdmin() {
    setAdminToken(adminTok);
    setShowAdmin(false);
    qc.invalidateQueries();
  }

  return (
    <div className="flex flex-col gap-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="font-display text-3xl font-semibold tracking-wide">Providers</h1>
          <p className="text-sm text-muted-foreground">
            Connect any provider — changes persist to config.json and hot-reload without a
            restart.
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

      {/* Configured providers */}
      {isError ? (
        <EmptyState
          title="Could not load providers"
          hint={
            (error as Error)?.message === "admin token required"
              ? "The gateway requires an admin token — click ‘set admin token’ above."
              : "The gateway did not respond to GET /api/providers."
          }
        />
      ) : providers.length === 0 && !isLoading ? (
        <EmptyState
          title="No providers configured"
          hint="Pick a provider from the catalog below to connect your first one."
        />
      ) : (
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {providers.map((p) => (
            <Card key={p.name} className="border-engraved">
              <CardContent className="flex flex-col gap-3">
                <div className="flex items-center justify-between gap-2">
                  <div className="flex min-w-0 items-center gap-3">
                    <RenderProviderIcon provider={p.name} size={28} />
                    <div className="truncate text-base font-semibold capitalize">{p.name}</div>
                  </div>
                  <div className="flex shrink-0 items-center">
                    <Button
                      variant="ghost"
                      size="icon"
                      onClick={() => openForConfigured(p.name)}
                      title="Update provider (re-enter key)"
                    >
                      <Pencil size={14} />
                    </Button>
                    <Button
                      variant="ghost"
                      size="icon"
                      className="text-destructive"
                      onClick={() => {
                        if (confirm(`Remove provider "${p.name}"?`)) remove.mutate(p.name);
                      }}
                      title="Remove provider"
                    >
                      <Trash2 size={14} />
                    </Button>
                  </div>
                </div>
                <div className="flex flex-wrap gap-2">
                  {p.capabilities.map((cap) => (
                    <Badge key={cap} variant="outline" className="gap-1.5">
                      <span
                        className="h-2 w-2 rounded-full"
                        style={{ background: CAP_COLORS[cap] ?? "var(--muted-foreground)" }}
                        aria-hidden
                      />
                      {cap}
                    </Badge>
                  ))}
                </div>
                <div className="text-xs text-muted-foreground">
                  {p.key_count} key{p.key_count === 1 ? "" : "s"}
                </div>
              </CardContent>
            </Card>
          ))}
        </div>
      )}

      <OrnamentDivider />

      {/* Catalog */}
      <div>
        <div className="mb-3 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
          Connect a provider
        </div>
        <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-4">
          {CATALOG.map((c) => {
            const configured = !!c.name && configuredNames.has(c.name);
            return (
              <button
                key={c.label}
                onClick={() => openSheet(c)}
                className="group relative flex items-center gap-3 rounded-lg border border-border bg-card px-3 py-3 text-left text-sm transition-colors hover:border-[var(--primary)]/50 hover:bg-accent"
              >
                <RenderProviderIcon provider={c.name || "custom"} size={24} />
                <span className="min-w-0">
                  <span className="block truncate font-medium">{c.label}</span>
                  {configured && (
                    <span className="flex items-center gap-1 text-[10px] uppercase tracking-wide text-primary">
                      <Check size={10} /> configured
                    </span>
                  )}
                </span>
              </button>
            );
          })}
        </div>
      </div>

      {/* Config sheet */}
      <Sheet open={!!entry} onOpenChange={(o) => !o && setEntry(null)}>
        <SheetContent side="right" className="w-full max-w-md overflow-y-auto sm:max-w-md">
          {entry && (
            <>
              <SheetHeader>
                <SheetTitle className="flex items-center gap-3">
                  <RenderProviderIcon provider={entry.name || name || "custom"} size={26} />
                  {entry.label === "Custom" ? "Custom provider" : `Connect ${entry.label}`}
                </SheetTitle>
                <SheetDescription>
                  {configuredNames.has(name)
                    ? "Already configured — saving replaces its key and settings."
                    : "The API key is stored in config.json (use ${ENV_VAR} to reference an environment variable)."}
                </SheetDescription>
              </SheetHeader>
              <form onSubmit={submit} className="flex flex-col gap-4 px-4 pb-6">
                <div className="flex flex-col gap-1">
                  <Label>Name (routing prefix)</Label>
                  <Input
                    value={name}
                    onChange={(e) => setName(e.target.value)}
                    placeholder="e.g. openai, zai"
                  />
                  <p className="text-xs text-muted-foreground">
                    Requests route as <code className="font-mono">{name.trim() || "<name>"}/model</code>
                  </p>
                </div>

                {entry.label === "Custom" && (
                  <div className="flex flex-col gap-1">
                    <Label>Wire format</Label>
                    <Select value={customKind} onValueChange={(v) => v && setCustomKind(v)}>
                      <SelectTrigger className="w-full">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        {CUSTOM_KINDS.map((k) => (
                          <SelectItem key={k.value} value={k.value}>
                            {k.label}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                  </div>
                )}

                <div className="flex flex-col gap-1">
                  <Label>
                    {entry.baseUrlLabel ?? "Base URL"}
                    {!entry.baseUrlRequired && (
                      <span className="ml-1 font-normal text-muted-foreground">(optional)</span>
                    )}
                  </Label>
                  <Input
                    value={baseUrl}
                    onChange={(e) => setBaseUrl(e.target.value)}
                    placeholder={entry.defaultBaseUrl ?? "https://…"}
                  />
                  {entry.defaultBaseUrl && !entry.baseUrlRequired && (
                    <p className="text-xs text-muted-foreground">
                      Defaults to <code className="font-mono">{entry.defaultBaseUrl}</code>
                    </p>
                  )}
                </div>

                <div className="flex flex-col gap-1">
                  <Label>API key</Label>
                  <Input
                    type="password"
                    value={apiKey}
                    onChange={(e) => setApiKey(e.target.value)}
                    placeholder={entry.keyPlaceholder ?? "sk-… (or ${ENV_VAR})"}
                  />
                  {entry.keyHint && (
                    <p className="text-xs text-muted-foreground">{entry.keyHint}</p>
                  )}
                </div>

                {formErr && <div className="text-sm text-error">{formErr}</div>}

                <Button type="submit" disabled={upsert.isPending}>
                  {upsert.isPending
                    ? "Saving…"
                    : configuredNames.has(name)
                      ? "Update provider"
                      : "Connect provider"}
                </Button>
              </form>
            </>
          )}
        </SheetContent>
      </Sheet>
    </div>
  );
}
