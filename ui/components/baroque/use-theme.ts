"use client";

// Theme state: "dark" (Midnight Baroque) | "light" (Porcelain Baroque).
// The active theme is the `dark` class on <html>, persisted in localStorage.
// A pre-paint inline script in app/layout.tsx applies the stored choice before
// first paint so there is no flash; this hook just mirrors + mutates it.

import { useCallback, useSyncExternalStore } from "react";

export type Theme = "dark" | "light";

export const THEME_STORAGE_KEY = "kgateway_theme";

const listeners = new Set<() => void>();

function subscribe(onChange: () => void): () => void {
  listeners.add(onChange);
  return () => listeners.delete(onChange);
}

function getSnapshot(): Theme {
  return document.documentElement.classList.contains("dark") ? "dark" : "light";
}

export function useTheme(): [Theme, (t: Theme) => void] {
  // Server snapshot says "dark" — matches the SSR'd class on <html>.
  const theme = useSyncExternalStore(subscribe, getSnapshot, () => "dark" as Theme);

  const setTheme = useCallback((t: Theme) => {
    document.documentElement.classList.toggle("dark", t === "dark");
    try {
      window.localStorage.setItem(THEME_STORAGE_KEY, t);
    } catch {
      // storage unavailable (private mode) — theme still applies for the session
    }
    listeners.forEach((l) => l());
  }, []);

  return [theme, setTheme];
}
