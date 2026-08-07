// Friendly labels for raw User-Agent strings, so the dashboard says "Claude Code"
// instead of "claude-cli/1.0.83 (external, cli)". Kept framework-free (a dumb ordered
// prefix table) — extend it as new clients show up in the logs.

const KNOWN: [prefix: string, label: string][] = [
  ["claude-cli", "Claude Code"],
  ["anthropic-sdk-", "Anthropic SDK"],
  ["OpenAI/Python", "openai-python"],
  ["openai-python", "openai-python"],
  ["openai-node", "openai-node"],
  ["OpenAI/JS", "openai-node"],
  ["python-requests", "python-requests"],
  ["python-httpx", "httpx"],
  ["aiohttp", "aiohttp"],
  ["curl/", "curl"],
  ["axios/", "axios"],
  ["node-fetch", "node-fetch"],
  ["undici", "undici"],
  ["Go-http-client", "Go client"],
  ["PostmanRuntime", "Postman"],
  ["Mozilla/", "Browser"],
];

export const UNKNOWN_CLIENT = "Unknown client";

/**
 * Map a raw User-Agent to a friendly client label. Unrecognized agents fall back to
 * their first product token (the part before "/"), and a missing UA becomes
 * "Unknown client" — deliberately conspicuous, since a client that sends no UA is
 * exactly the kind of connection worth noticing.
 */
export function clientLabel(ua: string | null | undefined): string {
  if (!ua) return UNKNOWN_CLIENT;
  for (const [prefix, label] of KNOWN) {
    if (ua.startsWith(prefix)) return label;
  }
  const token = ua.split("/")[0]?.trim();
  return token || UNKNOWN_CLIENT;
}

/**
 * Mask a virtual key (a raw bearer token) for display: "sk-ab…wxyz". Short keys keep only
 * a two-char prefix — even a low-entropy test key never renders in full.
 */
export function maskKey(key: string): string {
  if (key.length <= 12) return `${key.slice(0, 2)}…`;
  return `${key.slice(0, 4)}…${key.slice(-4)}`;
}
