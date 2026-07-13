/** Color for an HTTP status family, as CSS variables from the theme:
 *  success 2xx, warning 4xx, error 5xx, muted otherwise. */
export function statusColor(status: number | string): string {
  const code = typeof status === "string" ? parseInt(status, 10) : status;
  if (code >= 200 && code < 300) return "var(--success)";
  if (code >= 400 && code < 500) return "var(--warning)";
  if (code >= 500) return "var(--error)";
  return "var(--muted-foreground)";
}
