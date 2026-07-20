"use client";

// Playground: a multi-turn chat thread through the gateway with live streaming,
// a params sidebar (model / system prompt / temperature / stream), per-response
// latency + token metadata, and Stop (abort) for in-flight requests. Model
// suggestions merge the aggregated /v1/models listing (live upstream inventory,
// no admin token) with configured providers and recently-logged provider/model
// pairs (admin-gated; those two are simply empty without a token).

import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Eraser, Send, Square } from "lucide-react";
import {
  chatCompletion,
  chatCompletionStream,
  getLogs,
  getModels,
  getProviders,
  type ChatMessage,
} from "@/lib/api";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Slider } from "@/components/ui/slider";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { OrnamentDivider } from "@/components/baroque/ornament-divider";
import { RenderProviderIcon } from "@/lib/provider-icons";

interface MsgMeta {
  model: string;
  ms?: number;
  promptTokens?: number;
  completionTokens?: number;
  error?: string;
  stopped?: boolean;
}

interface ThreadMsg {
  role: "user" | "assistant";
  content: string;
  meta?: MsgMeta;
}

function MessageBubble({ msg, busy }: { msg: ThreadMsg; busy: boolean }) {
  if (msg.role === "user") {
    return (
      <div className="flex justify-end">
        <div className="max-w-[85%] whitespace-pre-wrap rounded-xl rounded-br-sm border border-primary/30 bg-primary/10 px-4 py-2.5 text-sm">
          {msg.content}
        </div>
      </div>
    );
  }

  const meta = msg.meta;
  const provider = meta?.model.split("/")[0] ?? "";
  return (
    <div className="flex justify-start">
      <div className="max-w-[85%] rounded-xl rounded-bl-sm border border-border bg-card px-4 py-2.5">
        <div className="mb-1.5 flex items-center gap-2 text-[10px] uppercase tracking-wide text-muted-foreground">
          <RenderProviderIcon provider={provider} size={14} />
          {meta?.model ?? "assistant"}
        </div>
        {meta?.error ? (
          <div className="text-sm" style={{ color: "var(--error)" }}>
            {meta.error}
          </div>
        ) : (
          <div className="whitespace-pre-wrap text-sm">
            {msg.content}
            {busy && <span className="animate-pulse text-primary">▍</span>}
          </div>
        )}
        {meta && !busy && !meta.error && (
          <div className="mt-1.5 flex flex-wrap gap-3 text-[10px] text-muted-foreground">
            {meta.ms !== undefined && <span>{Math.round(meta.ms).toLocaleString()} ms</span>}
            {meta.promptTokens !== undefined && (
              <span>
                {meta.promptTokens.toLocaleString()} → {meta.completionTokens?.toLocaleString() ?? 0}{" "}
                tokens
              </span>
            )}
            {meta.stopped && <span style={{ color: "var(--warning)" }}>stopped</span>}
          </div>
        )}
      </div>
    </div>
  );
}

export default function PlaygroundPage() {
  // --- params ---
  const [model, setModel] = useState("openai/gpt-4o");
  const [system, setSystem] = useState("You are a helpful assistant.");
  const [temperature, setTemperature] = useState(1.0);
  const [streaming, setStreaming] = useState(true);

  // --- thread ---
  const [messages, setMessages] = useState<ThreadMsg[]>([]);
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const abortRef = useRef<AbortController | null>(null);
  const scrollRef = useRef<HTMLDivElement>(null);

  // Model suggestions: real provider/model pairs from recent traffic + configured
  // provider prefixes. Both fail silently without an admin token.
  const { data: providers } = useQuery({
    queryKey: ["providers"],
    queryFn: getProviders,
    retry: false,
    staleTime: 60000,
  });
  const { data: recentPage } = useQuery({
    queryKey: ["logs", { limit: 100, sort_by: "created_at", order: "desc" }],
    queryFn: () => getLogs({ limit: 100, sort_by: "created_at", order: "desc" }),
    retry: false,
    staleTime: 60000,
  });
  // Live upstream inventory via the gateway's aggregated /v1/models (server-cached;
  // no admin token needed). The richest source: every id is directly routable.
  const { data: listedModels } = useQuery({
    queryKey: ["models"],
    queryFn: getModels,
    retry: false,
    staleTime: 60000,
  });
  const modelSuggestions = useMemo(() => {
    const set = new Set<string>();
    for (const m of listedModels ?? []) set.add(m.id);
    for (const l of recentPage?.logs ?? []) {
      if (l.provider && l.model) set.add(`${l.provider}/${l.model}`);
    }
    for (const p of providers ?? []) set.add(`${p.name}/`);
    return Array.from(set).sort();
  }, [listedModels, recentPage, providers]);

  // Keep the newest message in view while the thread grows / streams.
  useEffect(() => {
    scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
  }, [messages]);

  async function send() {
    const text = draft.trim();
    if (!text || busy) return;

    // History sent to the API: system prompt + all successful turns + the new message.
    const history: ChatMessage[] = [
      ...(system.trim() ? [{ role: "system" as const, content: system }] : []),
      ...messages
        .filter((m) => !m.meta?.error && m.content)
        .map((m) => ({ role: m.role, content: m.content })),
      { role: "user" as const, content: text },
    ];

    setMessages((m) => [
      ...m,
      { role: "user", content: text },
      { role: "assistant", content: "", meta: { model } },
    ]);
    setDraft("");
    setBusy(true);
    const ctrl = new AbortController();
    abortRef.current = ctrl;
    const t0 = performance.now();

    const patchLast = (patch: (last: ThreadMsg) => ThreadMsg) =>
      setMessages((m) => {
        const copy = [...m];
        copy[copy.length - 1] = patch(copy[copy.length - 1]);
        return copy;
      });

    try {
      if (streaming) {
        await chatCompletionStream(
          { model, messages: history, temperature },
          (delta) => patchLast((last) => ({ ...last, content: last.content + delta })),
          ctrl.signal,
        );
        patchLast((last) => ({
          ...last,
          meta: { ...last.meta!, ms: performance.now() - t0 },
        }));
      } else {
        const res = await chatCompletion({ model, messages: history, temperature }, ctrl.signal);
        patchLast((last) => ({
          ...last,
          content: res.choices[0]?.message.content ?? "",
          meta: {
            ...last.meta!,
            ms: performance.now() - t0,
            promptTokens: res.usage?.prompt_tokens,
            completionTokens: res.usage?.completion_tokens,
          },
        }));
      }
    } catch (e) {
      if (e instanceof DOMException && e.name === "AbortError") {
        // Keep whatever streamed in before Stop.
        patchLast((last) => ({
          ...last,
          meta: { ...last.meta!, ms: performance.now() - t0, stopped: true },
        }));
      } else {
        patchLast((last) => ({
          ...last,
          meta: { ...last.meta!, error: e instanceof Error ? e.message : String(e) },
        }));
      }
    } finally {
      abortRef.current = null;
      setBusy(false);
    }
  }

  function stop() {
    abortRef.current?.abort();
  }

  function clearThread() {
    if (busy) stop();
    setMessages([]);
  }

  function onComposerKeyDown(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void send();
    }
  }

  return (
    <div className="flex flex-col gap-6">
      <div>
        <h1 className="font-display text-3xl font-semibold tracking-wide">Playground</h1>
        <p className="text-sm text-muted-foreground">
          Hold a conversation through the gateway — streaming, multi-turn, any provider.
        </p>
      </div>

      <div className="grid gap-6 lg:grid-cols-[minmax(0,1fr)_290px]">
        {/* Chat thread */}
        <Card className="flex h-[calc(100dvh-16rem)] min-h-[440px] flex-col gap-0 py-0">
          <div ref={scrollRef} className="flex-1 overflow-y-auto px-5 py-5">
            {messages.length === 0 ? (
              <div className="flex h-full flex-col items-center justify-center gap-3 text-center">
                <OrnamentDivider className="w-40" />
                <div className="font-display text-lg">Begin a conversation</div>
                <p className="max-w-sm text-sm text-muted-foreground">
                  Messages route through the gateway as{" "}
                  <code className="font-mono">{model || "provider/model"}</code> — adjust the
                  model and parameters on the right.
                </p>
                <OrnamentDivider className="w-40" />
              </div>
            ) : (
              <div className="flex flex-col gap-4">
                {messages.map((m, i) => (
                  <MessageBubble
                    key={i}
                    msg={m}
                    busy={busy && i === messages.length - 1 && m.role === "assistant"}
                  />
                ))}
              </div>
            )}
          </div>

          {/* Composer */}
          <div className="border-t border-border p-3">
            <div className="flex items-end gap-2">
              <Textarea
                rows={2}
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                onKeyDown={onComposerKeyDown}
                placeholder="Send a message… (Enter to send, Shift+Enter for a new line)"
                className="max-h-40 min-h-[3rem] resize-y"
              />
              {busy ? (
                <Button variant="outline" onClick={stop} title="Stop generating">
                  <Square size={14} />
                  Stop
                </Button>
              ) : (
                <Button onClick={() => void send()} disabled={!draft.trim()} title="Send">
                  <Send size={14} />
                  Send
                </Button>
              )}
            </div>
          </div>
        </Card>

        {/* Params sidebar */}
        <div className="flex flex-col gap-4">
          <Card className="py-4">
            <CardContent className="flex flex-col gap-4">
              <div className="flex flex-col gap-1">
                <Label>Model</Label>
                <Input
                  value={model}
                  onChange={(e) => setModel(e.target.value)}
                  placeholder="provider/model"
                  list="model-suggestions"
                  className="font-mono text-xs"
                />
                <datalist id="model-suggestions">
                  {modelSuggestions.map((m) => (
                    <option key={m} value={m} />
                  ))}
                </datalist>
              </div>

              <div className="flex flex-col gap-1">
                <Label>System prompt</Label>
                <Textarea
                  rows={4}
                  value={system}
                  onChange={(e) => setSystem(e.target.value)}
                  className="text-xs"
                />
              </div>

              <div className="flex flex-col gap-2">
                <div className="flex items-center justify-between">
                  <Label>Temperature</Label>
                  <span className="font-mono text-xs text-muted-foreground">
                    {temperature.toFixed(1)}
                  </span>
                </div>
                <Slider
                  value={[temperature]}
                  onValueChange={([v]) => setTemperature(v)}
                  min={0}
                  max={2}
                  step={0.1}
                />
              </div>

              <div className="flex items-center justify-between">
                <Label>Stream response</Label>
                <Switch checked={streaming} onCheckedChange={setStreaming} />
              </div>
            </CardContent>
          </Card>

          <Button
            variant="outline"
            onClick={clearThread}
            disabled={messages.length === 0}
            className="w-full"
          >
            <Eraser size={14} />
            Clear conversation
          </Button>

          <p className="px-1 text-xs text-muted-foreground">
            Each turn resends the full thread — token usage grows with conversation length.
          </p>
        </div>
      </div>
    </div>
  );
}
