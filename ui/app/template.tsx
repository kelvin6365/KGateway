"use client";

// Route-enter transition. template.tsx (unlike layout.tsx) remounts on every
// navigation, so the fade+rise plays each time the route changes.

import { useRef } from "react";
import gsap from "gsap";
import { useGSAP } from "@gsap/react";
import { usePrefersReducedMotion } from "@/components/baroque/use-reduced-motion";

gsap.registerPlugin(useGSAP);

export default function Template({ children }: { children: React.ReactNode }) {
  const ref = useRef<HTMLDivElement>(null);
  const reducedMotion = usePrefersReducedMotion();

  useGSAP(
    () => {
      if (reducedMotion || !ref.current) return;
      gsap.from(ref.current, {
        opacity: 0,
        y: 12,
        duration: 0.4,
        ease: "power2.out",
        clearProps: "opacity,transform",
      });
    },
    { scope: ref, dependencies: [reducedMotion] },
  );

  return <div ref={ref}>{children}</div>;
}
