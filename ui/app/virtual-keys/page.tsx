"use client";

import { useMemo, useState } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { Trash2, Plus, KeyRound, Pencil, X, Save } from "lucide-react";
import {
  getVirtualKeys,
  putVirtualKey,
  deleteVirtualKey,
  getFilterData,
  getAdminToken,
  setAdminToken,
  type VirtualKey,
  type VirtualKeyInput,
} from "@/lib/api";
import { Card, CardHeader, CardTitle, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Badge } from "@/components/ui/badge";
import { TagInput } from "@/components/ui/tag-input";
import {
  Select,
  SelectTrigger,
  SelectValue,
  SelectContent,
  SelectItem,
} from "@/components/ui/select";
import { OrnamentDivider } from "@/components/baroque/ornament-divider";
import { EmptyState } from "@/components/baroque/empty-state";

const PERIODS: { secs: string; label: string }[] = [
  { secs: "60", label: "per minute" },
  { secs: "3600", label: "per hour" },
  { secs: "86400", label: "per day" },
  { secs: "604800", label: "per week" },
  { secs: "2592000", label: "per 30 days" },
];

function periodLabel(secs?: number | null): string {
  return PERIODS.find((p) => Number(p.secs) === secs)?.label ?? `per ${secs ?? 0}s`;
}

const emptyForm = {
  id: "",
  name: "",
  allowed: [] as string[],
  denied: [] as string[],
  rpm: "",
  tokens: "",
  cost: "",
  period: "86400",
};

export default function VirtualKeysPage() {
  const qc = useQueryClient();
  const [adminTok, setAdminTok] = useState("");
  const [showAdmin, setShowAdmin] = useState(false);

  const {
    data: keys = [],
    isLoading,
    isError,
    error,
  } = useQuery({ queryKey: ["virtual-keys"], queryFn: getVirtualKeys, retry: false });

  // Model suggestions from models actually seen in logs (free text is always allowed).
  const { data: filterData } = useQuery({
    queryKey: ["filter-data"],
    queryFn: getFilterData,
    retry: false,
  });
  const modelSuggestions = useMemo(() => filterData?.models ?? [], [filterData]);

  const [form, setForm] = useState({ ...emptyForm });
  const [editingId, setEditingId] = useState<string | null>(null);
  const [formErr, setFormErr] = useState<string | null>(null);
  const set = <K extends keyof typeof form>(k: K, v: (typeof form)[K]) =>
    setForm((f) => ({ ...f, [k]: v }));

  function resetForm() {
    setForm({ ...emptyForm });
    setEditingId(null);
    setFormErr(null);
  }

  function startEdit(k: VirtualKey) {
    setEditingId(k.id);
    setFormErr(null);
    setForm({
      id: k.id,
      name: k.name ?? "",
      allowed: k.allowed_models ?? [],
      denied: k.denied_models ?? [],
      rpm: k.max_requests_per_min != null ? String(k.max_requests_per_min) : "",
      tokens: k.max_total_tokens != null ? String(k.max_total_tokens) : "",
      cost: k.max_cost_per_period != null ? String(k.max_cost_per_period) : "",
      period: k.max_cost_period_secs != null ? String(k.max_cost_period_secs) : "86400",
    });
    document.getElementById("vk-form")?.scrollIntoView({ behavior: "smooth", block: "start" });
  }

  const upsert = useMutation({
    mutationFn: (vars: { id: string; input: VirtualKeyInput }) =>
      putVirtualKey(vars.id, vars.input),
    onSuccess: () => {
      resetForm();
      qc.invalidateQueries({ queryKey: ["virtual-keys"] });
    },
    onError: (e: Error) => setFormErr(e.message),
  });

  const remove = useMutation({
    mutationFn: (i: string) => deleteVirtualKey(i),
    onSuccess: (_d, i) => {
      if (editingId === i) resetForm();
      qc.invalidateQueries({ queryKey: ["virtual-keys"] });
    },
  });

  function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!form.id.trim()) return setFormErr("key id (the bearer token) is required");
    const hasCost = form.cost.trim() !== "";
    const input: VirtualKeyInput = {
      name: form.name.trim(),
      allowed_models: form.allowed,
      denied_models: form.denied,
      max_requests_per_min: form.rpm.trim() ? Number(form.rpm) : null,
      max_total_tokens: form.tokens.trim() ? Number(form.tokens) : null,
      max_cost_per_period: hasCost ? Number(form.cost) : null,
      max_cost_period_secs: hasCost ? Number(form.period) : null,
    };
    upsert.mutate({ id: form.id.trim(), input });
  }

  function saveAdmin() {
    setAdminToken(adminTok);
    setShowAdmin(false);
    qc.invalidateQueries();
  }

  const editing = editingId !== null;

  return (
    <div className="flex flex-col gap-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="font-display text-3xl font-semibold tracking-wide">Virtual Keys</h1>
          <p className="text-sm text-muted-foreground">
            Per-tenant keys with model allow/deny-lists, rate limits, and token + cost budgets.
            Clients send <code>Authorization: Bearer &lt;id&gt;</code>.
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

      <Card className="border-[var(--warning)]">
        <CardContent className="text-sm">
          <strong className="text-warning">Heads up:</strong> the first virtual key switches the
          gateway to <strong>strict mode</strong> — every request then requires a valid{" "}
          <code>Authorization: Bearer &lt;id&gt;</code>. Delete all keys to return to open mode.
        </CardContent>
      </Card>

      {/* Create / edit form */}
      <Card id="vk-form" className={editing ? "ring-1 ring-primary/40" : undefined}>
        <CardHeader className="flex flex-row items-center justify-between gap-2">
          <CardTitle>
            {editing ? (
              <span className="flex items-center gap-2">
                Editing <code className="font-mono text-primary">{editingId}</code>
              </span>
            ) : (
              "Create a virtual key"
            )}
          </CardTitle>
          {editing && (
            <Button variant="ghost" size="sm" onClick={resetForm} type="button">
              <X size={14} /> Cancel
            </Button>
          )}
        </CardHeader>
        <CardContent>
          <form onSubmit={submit} className="flex flex-col gap-4">
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <div className="flex flex-col gap-1">
                <Label htmlFor="vk-id">Key id (bearer token)</Label>
                <Input
                  id="vk-id"
                  value={form.id}
                  disabled={editing}
                  onChange={(e) => set("id", e.target.value)}
                  placeholder="vk_team_alpha"
                  className="font-mono"
                />
                {editing && (
                  <span className="text-xs text-muted-foreground">
                    The id is the identity — delete and recreate to rename it.
                  </span>
                )}
              </div>
              <div className="flex flex-col gap-1">
                <Label htmlFor="vk-name">Name</Label>
                <Input
                  id="vk-name"
                  value={form.name}
                  onChange={(e) => set("name", e.target.value)}
                  placeholder="Team Alpha"
                />
              </div>
            </div>

            <div className="flex flex-col gap-1">
              <Label htmlFor="vk-allowed">Allowed models</Label>
              <TagInput
                id="vk-allowed"
                value={form.allowed}
                onChange={(v) => set("allowed", v)}
                suggestions={modelSuggestions}
                placeholder="Type a model + Enter (e.g. openai/gpt-4o). Empty = all allowed."
              />
            </div>

            <div className="flex flex-col gap-1">
              <Label htmlFor="vk-denied">Denied models</Label>
              <TagInput
                id="vk-denied"
                value={form.denied}
                onChange={(v) => set("denied", v)}
                suggestions={modelSuggestions}
                variant="destructive"
                placeholder="Blocked models — deny always wins over the allow-list."
              />
            </div>

            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <div className="flex flex-col gap-1">
                <Label htmlFor="vk-rpm">Max requests / min</Label>
                <Input
                  id="vk-rpm"
                  type="number"
                  min={0}
                  value={form.rpm}
                  onChange={(e) => set("rpm", e.target.value)}
                  placeholder="60 — empty = unlimited"
                />
              </div>
              <div className="flex flex-col gap-1">
                <Label htmlFor="vk-tokens">Max total tokens</Label>
                <Input
                  id="vk-tokens"
                  type="number"
                  min={0}
                  value={form.tokens}
                  onChange={(e) => set("tokens", e.target.value)}
                  placeholder="1000000 — empty = unlimited"
                />
              </div>
            </div>

            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <div className="flex flex-col gap-1">
                <Label htmlFor="vk-cost">Max cost (USD)</Label>
                <Input
                  id="vk-cost"
                  type="number"
                  min={0}
                  step="0.01"
                  value={form.cost}
                  onChange={(e) => set("cost", e.target.value)}
                  placeholder="50.00 — empty = unlimited"
                />
              </div>
              <div className="flex flex-col gap-1">
                <Label htmlFor="vk-period">Cost period</Label>
                <Select
                  value={form.period}
                  onValueChange={(v) => v && set("period", v)}
                >
                  <SelectTrigger id="vk-period" className="w-full" disabled={!form.cost.trim()}>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {PERIODS.map((p) => (
                      <SelectItem key={p.secs} value={p.secs}>
                        {p.label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
            </div>

            {formErr && <div className="text-sm text-error">{formErr}</div>}
            <div className="flex items-center gap-2">
              <Button type="submit" disabled={upsert.isPending}>
                {editing ? <Save size={15} /> : <Plus size={15} />}
                {upsert.isPending ? "Saving…" : editing ? "Update key" : "Create key"}
              </Button>
              {editing && (
                <Button type="button" variant="outline" onClick={resetForm}>
                  Cancel
                </Button>
              )}
            </div>
          </form>
        </CardContent>
      </Card>

      <OrnamentDivider />

      {/* Existing keys */}
      {isError ? (
        <EmptyState
          title="Could not load virtual keys"
          hint={
            (error as Error)?.message === "admin token required"
              ? "The gateway requires an admin token — click ‘set admin token’ above."
              : "The gateway did not respond to GET /api/config/virtual-keys."
          }
        />
      ) : keys.length === 0 && !isLoading ? (
        <EmptyState
          title="No virtual keys — open mode"
          hint="The gateway currently accepts any request. Create a key above to enable per-tenant governance (strict mode)."
        />
      ) : (
        <div className="flex flex-col gap-3">
          {keys.map((k) => (
            <Card
              key={k.id}
              className={editingId === k.id ? "ring-1 ring-primary/40" : undefined}
            >
              <CardContent className="flex flex-col gap-3">
                <div className="flex flex-wrap items-start justify-between gap-3">
                  <div className="flex items-center gap-3">
                    <KeyRound size={16} className="text-primary" />
                    <div>
                      <div className="font-mono text-sm font-semibold">{k.id}</div>
                      <div className="text-xs text-muted-foreground">{k.name || "—"}</div>
                    </div>
                  </div>
                  <div className="flex items-center gap-1">
                    <Button
                      variant="ghost"
                      size="icon"
                      onClick={() => startEdit(k)}
                      title="Edit key"
                    >
                      <Pencil size={14} />
                    </Button>
                    <Button
                      variant="ghost"
                      size="icon"
                      className="text-destructive"
                      onClick={() => {
                        if (confirm(`Delete virtual key "${k.id}"?`)) remove.mutate(k.id);
                      }}
                      title="Delete key"
                    >
                      <Trash2 size={15} />
                    </Button>
                  </div>
                </div>

                {/* Models */}
                <div className="flex flex-col gap-2 text-xs">
                  <div className="flex flex-wrap items-center gap-1.5">
                    <span className="text-muted-foreground">Allowed:</span>
                    {k.allowed_models.length ? (
                      k.allowed_models.map((m) => (
                        <Badge key={m} variant="secondary" className="font-mono">
                          {m}
                        </Badge>
                      ))
                    ) : (
                      <span className="text-muted-foreground">all models</span>
                    )}
                  </div>
                  {k.denied_models && k.denied_models.length > 0 && (
                    <div className="flex flex-wrap items-center gap-1.5">
                      <span className="text-muted-foreground">Denied:</span>
                      {k.denied_models.map((m) => (
                        <Badge key={m} variant="destructive" className="font-mono">
                          {m}
                        </Badge>
                      ))}
                    </div>
                  )}
                </div>

                {/* Limits */}
                <div className="flex flex-wrap gap-2 text-xs">
                  <Badge variant="outline">rate: {k.max_requests_per_min ?? "∞"}/min</Badge>
                  <Badge variant="outline">
                    tokens: {k.max_total_tokens?.toLocaleString() ?? "∞"}
                  </Badge>
                  <Badge variant="outline">
                    cost:{" "}
                    {k.max_cost_per_period != null
                      ? `$${k.max_cost_per_period} ${periodLabel(k.max_cost_period_secs)}`
                      : "∞"}
                  </Badge>
                </div>
              </CardContent>
            </Card>
          ))}
        </div>
      )}
    </div>
  );
}
