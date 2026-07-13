"use client";

import * as React from "react";
import { X } from "lucide-react";
import { cn } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";

interface TagInputProps {
  /** Current tokens. */
  value: string[];
  onChange: (next: string[]) => void;
  placeholder?: string;
  /** Optional autocomplete suggestions (e.g. known models). Free text is always allowed. */
  suggestions?: string[];
  id?: string;
  disabled?: boolean;
  /** Visual accent for the chips. */
  variant?: "secondary" | "destructive";
}

/**
 * Multi-select tag input that also accepts free text: type a value and press Enter or comma
 * (or blur) to add it as a removable chip. Backspace on an empty field removes the last chip.
 * Suggestions are optional and non-restrictive — any string can be entered.
 */
export function TagInput({
  value,
  onChange,
  placeholder,
  suggestions = [],
  id,
  disabled,
  variant = "secondary",
}: TagInputProps) {
  const [draft, setDraft] = React.useState("");
  const [open, setOpen] = React.useState(false);
  const inputRef = React.useRef<HTMLInputElement>(null);

  function add(raw: string) {
    const items = raw
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);
    if (!items.length) return;
    const next = [...value];
    for (const it of items) if (!next.includes(it)) next.push(it);
    onChange(next);
    setDraft("");
  }

  function removeAt(i: number) {
    onChange(value.filter((_, idx) => idx !== i));
  }

  const query = draft.trim().toLowerCase();
  const filtered = suggestions
    .filter((s) => !value.includes(s))
    .filter((s) => s.toLowerCase().includes(query))
    .slice(0, 8);

  function onKeyDown(e: React.KeyboardEvent<HTMLInputElement>) {
    if (e.key === "Enter" || e.key === ",") {
      e.preventDefault();
      add(draft);
    } else if (e.key === "Backspace" && !draft && value.length) {
      removeAt(value.length - 1);
    }
  }

  return (
    <div className="relative">
      <div
        className={cn(
          "flex min-h-8 w-full flex-wrap items-center gap-1.5 rounded-lg border border-input bg-transparent px-2 py-1 transition-colors focus-within:border-ring focus-within:ring-3 focus-within:ring-ring/50 dark:bg-input/30",
          disabled && "pointer-events-none opacity-50",
        )}
        onClick={() => inputRef.current?.focus()}
      >
        {value.map((tag, i) => (
          <Badge key={tag} variant={variant} className="gap-1 font-mono">
            {tag}
            <button
              type="button"
              onClick={(e) => {
                e.stopPropagation();
                removeAt(i);
              }}
              className="rounded-full opacity-60 transition-opacity hover:opacity-100"
              aria-label={`Remove ${tag}`}
            >
              <X size={11} />
            </button>
          </Badge>
        ))}
        <input
          id={id}
          ref={inputRef}
          value={draft}
          disabled={disabled}
          onChange={(e) => {
            setDraft(e.target.value);
            setOpen(true);
          }}
          onKeyDown={onKeyDown}
          onFocus={() => setOpen(true)}
          onBlur={() => {
            add(draft);
            // Delay so a suggestion mousedown registers before the list unmounts.
            setTimeout(() => setOpen(false), 120);
          }}
          placeholder={value.length ? "" : placeholder}
          className="h-6 min-w-24 flex-1 bg-transparent px-1 text-sm outline-none placeholder:text-muted-foreground"
        />
      </div>

      {open && filtered.length > 0 && (
        <div className="absolute z-20 mt-1 max-h-56 w-full overflow-y-auto rounded-lg bg-popover text-popover-foreground shadow-md ring-1 ring-foreground/10">
          {filtered.map((s) => (
            <button
              key={s}
              type="button"
              // mousedown (not click) so it fires before the input's blur.
              onMouseDown={(e) => {
                e.preventDefault();
                add(s);
                inputRef.current?.focus();
              }}
              className="block w-full px-3 py-1.5 text-left font-mono text-sm transition-colors hover:bg-muted"
            >
              {s}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
