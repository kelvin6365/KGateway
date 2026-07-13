// Baroque SVG corner ornaments + the OrnateCard hero surface.
// One hand-drawn top-left flourish, mirrored via scale transforms for the
// other three corners. Stroke-only, currentColor, so it inherits any text
// color utility (defaults to a translucent primary blue).

import * as React from "react";
import { Card } from "@/components/ui/card";
import { cn } from "@/lib/utils";

type Corner = "tl" | "tr" | "bl" | "br";

const CORNER_TRANSFORM: Record<Corner, string | undefined> = {
  tl: undefined,
  tr: "scale(-1,1)",
  bl: "scale(1,-1)",
  br: "scale(-1,-1)",
};

const CORNER_POSITION: Record<Corner, string> = {
  tl: "left-0 top-0",
  tr: "right-0 top-0",
  bl: "bottom-0 left-0",
  br: "bottom-0 right-0",
};

export function Filigree({
  corner = "tl",
  size = 44,
  className,
}: {
  corner?: Corner;
  size?: number;
  className?: string;
}) {
  return (
    <svg
      viewBox="0 0 48 48"
      width={size}
      height={size}
      aria-hidden
      className={cn(
        "pointer-events-none absolute text-primary/35",
        CORNER_POSITION[corner],
        className,
      )}
      style={CORNER_TRANSFORM[corner] ? { transform: CORNER_TRANSFORM[corner] } : undefined}
      fill="none"
      stroke="currentColor"
      strokeWidth="1.1"
      strokeLinecap="round"
    >
      {/* outer sweep hugging the corner */}
      <path d="M1.5 30 C1.5 12 12 1.5 30 1.5" />
      {/* inner echo line */}
      <path d="M5.5 27 C5.5 15 15 5.5 27 5.5" />
      {/* volute — the classic baroque spiral curl */}
      <path d="M27 5.5 C 33 6.5 34.5 12 30.5 14 C 27.5 15.5 24.5 13 26 10.2 C 27 8.4 29.6 8.6 30 10.5" />
      {/* mirrored volute down the left edge */}
      <path d="M5.5 27 C 6.5 33 12 34.5 14 30.5 C 15.5 27.5 13 24.5 10.2 26 C 8.4 27 8.6 29.6 10.5 30" />
      {/* acanthus leaf flick into the corner */}
      <path d="M9 9 C 12 10.5 13.5 13 14 16.5 M9 9 C 10.5 12 13 13.5 16.5 14" strokeWidth="0.9" />
      {/* terminal dot */}
      <circle cx="9" cy="9" r="1" fill="currentColor" stroke="none" />
    </svg>
  );
}

/**
 * Hero-surface card: shadcn Card + engraved border + four filigree corners.
 * Reserved for headline surfaces (stat tiles, dashboard header, empty states)
 * — everyday data surfaces use the plain Card to keep the ornament special.
 */
export function OrnateCard({
  className,
  children,
  ...props
}: React.ComponentProps<typeof Card>) {
  return (
    <Card className={cn("border-engraved relative overflow-hidden", className)} {...props}>
      <Filigree corner="tl" />
      <Filigree corner="tr" />
      <Filigree corner="bl" />
      <Filigree corner="br" />
      {children}
    </Card>
  );
}
