// Thin ornamental rule for section breaks: hairlines flanking a central
// acanthus-scroll motif. Stroke-only, inherits currentColor.

import { cn } from "@/lib/utils";

export function OrnamentDivider({ className }: { className?: string }) {
  return (
    <div
      aria-hidden
      className={cn("flex items-center gap-3 text-primary/30", className)}
    >
      <span className="h-px flex-1 bg-gradient-to-r from-transparent to-current" />
      <svg
        viewBox="0 0 72 12"
        width="72"
        height="12"
        fill="none"
        stroke="currentColor"
        strokeWidth="1"
        strokeLinecap="round"
      >
        {/* mirrored scroll curls around a center bud */}
        <path d="M2 6 C 12 6 16 2.5 22 2.5 C 27 2.5 28 6.5 24.5 7.5 C 22 8.2 20.5 5.8 22.5 4.8" />
        <path d="M70 6 C 60 6 56 9.5 50 9.5 C 45 9.5 44 5.5 47.5 4.5 C 50 3.8 51.5 6.2 49.5 7.2" />
        <circle cx="36" cy="6" r="1.6" fill="currentColor" stroke="none" />
        <path d="M31 6 C 33 4.5 33 7.5 31 6 M41 6 C 39 4.5 39 7.5 41 6" strokeWidth="0.9" />
      </svg>
      <span className="h-px flex-1 bg-gradient-to-l from-transparent to-current" />
    </div>
  );
}
