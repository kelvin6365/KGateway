"use client";

import { useEffect, useState } from "react";

/** Now, refreshed on an interval so relative times ("5m ago") stay fresh across renders. */
export function useNow(intervalMs = 30000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), intervalMs);
    return () => clearInterval(t);
  }, [intervalMs]);
  return now;
}
