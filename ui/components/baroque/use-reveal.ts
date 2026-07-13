"use client";

// Staggered entrance reveal for page content, GSAP-driven.
// Usage:
//   const scope = useStaggerReveal<HTMLDivElement>();
//   <div ref={scope}> ... <section data-reveal /> ... </div>
// Every descendant carrying `data-reveal` fades in and rises, staggered.

import { useRef } from "react";
import gsap from "gsap";
import { useGSAP } from "@gsap/react";
import { usePrefersReducedMotion } from "./use-reduced-motion";

gsap.registerPlugin(useGSAP);

export function useStaggerReveal<T extends HTMLElement>(options?: {
  stagger?: number;
  y?: number;
  duration?: number;
}) {
  const scope = useRef<T>(null);
  const reducedMotion = usePrefersReducedMotion();

  useGSAP(
    () => {
      if (reducedMotion || !scope.current) return;
      const items = scope.current.querySelectorAll("[data-reveal]");
      if (items.length === 0) return;
      gsap.from(items, {
        opacity: 0,
        y: options?.y ?? 24,
        duration: options?.duration ?? 0.5,
        stagger: options?.stagger ?? 0.06,
        ease: "power2.out",
        clearProps: "opacity,transform",
      });
    },
    { scope, dependencies: [reducedMotion] },
  );

  return scope;
}
