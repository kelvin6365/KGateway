"use client";

// Single source of truth for the dashboard's access token + resolved identity.
// Wraps the localStorage-backed token (see lib/api.ts) in React state so every page
// re-renders when it changes, and resolves who the token is via GET /api/whoami.

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useState,
} from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AuthRequiredError,
  getAdminToken,
  getWhoami,
  setAdminToken,
  type Whoami,
} from "@/lib/api";

/** Where the dashboard stands with the gateway's control plane. */
export type AuthStatus =
  | "loading"
  /** Gateway has no tokens and no virtual keys — everything is open. */
  | "open"
  /** The stored token was accepted; `identity` says who it is. */
  | "authed"
  /** The gateway wants a credential and the stored one (or none) was rejected. */
  | "unauthenticated"
  /** The gateway didn't answer — unreachable, not an auth problem. */
  | "error";

export interface AuthState {
  /** The raw stored token ("" when none). */
  token: string;
  hasToken: boolean;
  /** Resolved identity from /api/whoami; null until known. */
  identity: Whoami | null;
  status: AuthStatus;
  /** True when the identity is a virtual key — reads are server-side scoped. */
  isScoped: boolean;
  /** Convenience permission checks against the resolved identity. */
  can: (permission: "logs:view" | "config:write" | "logs:reveal") => boolean;
  setToken: (token: string) => void;
  clearToken: () => void;
}

const AuthContext = createContext<AuthState | null>(null);

export function AuthProvider({ children }: { children: React.ReactNode }) {
  const qc = useQueryClient();
  // Token lives in localStorage; mirror it into state after mount so SSR and the first
  // client render agree (both see "").
  const [token, setTokenState] = useState("");
  const [hydrated, setHydrated] = useState(false);
  useEffect(() => {
    setTokenState(getAdminToken());
    setHydrated(true);
  }, []);

  const whoami = useQuery<Whoami, Error>({
    queryKey: ["whoami", token],
    queryFn: getWhoami,
    enabled: hydrated,
    retry: false,
    staleTime: 60_000,
  });

  const setToken = useCallback(
    (t: string) => {
      setAdminToken(t);
      setTokenState(t);
      // Drop stale whoami entries first — invalidating alone would refetch the
      // still-mounted old-token query with the NEW stored token and cache the new
      // identity under the old key. Then refresh everything else.
      qc.removeQueries({ queryKey: ["whoami"] });
      qc.invalidateQueries();
    },
    [qc],
  );
  const clearToken = useCallback(() => setToken(""), [setToken]);

  let status: AuthStatus = "loading";
  let identity: Whoami | null = null;
  if (hydrated && !whoami.isPending) {
    if (whoami.isError) {
      status =
        whoami.error instanceof AuthRequiredError ||
        whoami.error.message === "admin token required"
          ? "unauthenticated"
          : "error";
    } else if (whoami.data) {
      identity = whoami.data;
      status = whoami.data.kind === "open" ? "open" : "authed";
    }
  }

  const permissions = identity?.permissions ?? [];
  const value: AuthState = {
    token,
    hasToken: token.length > 0,
    identity,
    status,
    isScoped: identity?.kind === "virtual_key" || identity?.scoped === true,
    can: (permission) =>
      status === "open" || permissions.includes(permission),
    setToken,
    clearToken,
  };
  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth(): AuthState {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error("useAuth must be used inside <AuthProvider>");
  return ctx;
}
