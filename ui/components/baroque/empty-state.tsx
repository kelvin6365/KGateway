// Themed empty state on the ornate hero surface — replaces the old
// hand-rolled EmptyState from components/ui-legacy.tsx.

import { OrnateCard } from "./filigree";
import { OrnamentDivider } from "./ornament-divider";

export function EmptyState({ title, hint }: { title: string; hint: string }) {
  return (
    <OrnateCard className="flex flex-col items-center justify-center gap-3 px-8 py-16 text-center">
      <div className="font-display text-xl font-semibold tracking-wide">{title}</div>
      <OrnamentDivider className="w-48" />
      <div className="max-w-md text-sm text-muted-foreground">{hint}</div>
    </OrnateCard>
  );
}
