"use client";

// GSAP count-up for stat-tile numbers. Animates only on first mount (and only
// when motion is allowed); later value changes — e.g. the dashboard's 5s poll
// refetches — snap instantly so live metrics don't flicker distractingly.

import { useEffect, useRef, useState } from "react";
import gsap from "gsap";
import { usePrefersReducedMotion } from "./use-reduced-motion";

export function useCountUp(target: number, durationSec = 1.1): number {
  const reducedMotion = usePrefersReducedMotion();
  const [display, setDisplay] = useState(reducedMotion ? target : 0);
  const animatedOnce = useRef(false);

  useEffect(() => {
    if (reducedMotion || animatedOnce.current) {
      setDisplay(target);
      return;
    }
    animatedOnce.current = true;
    const counter = { value: 0 };
    const tween = gsap.to(counter, {
      value: target,
      duration: durationSec,
      ease: "power2.out",
      onUpdate: () => setDisplay(counter.value),
    });
    return () => {
      tween.kill();
    };
  }, [target, durationSec, reducedMotion]);

  return display;
}
