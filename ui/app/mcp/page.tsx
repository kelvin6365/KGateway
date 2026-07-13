"use client";

import { useQuery } from "@tanstack/react-query";
import { getMcpTools } from "@/lib/api";
import { Card } from "@/components/ui/card";
import { EmptyState } from "@/components/baroque/empty-state";

export default function McpPage() {
  const { data: tools = [], isLoading, isError } = useQuery({
    queryKey: ["mcp-tools"],
    queryFn: getMcpTools,
    retry: false,
  });

  return (
    <div className="flex flex-col gap-6">
      <div>
        <h1 className="font-display text-3xl font-semibold tracking-wide">MCP</h1>
        <p className="text-sm text-muted-foreground">
          Tools discovered by the gateway&rsquo;s MCP client, exposed to models.
        </p>
      </div>

      {isError ? (
        <EmptyState
          title="Could not load MCP tools"
          hint="The gateway did not respond to GET /api/mcp/tools. Confirm it is running and reachable."
        />
      ) : tools.length === 0 && !isLoading ? (
        <EmptyState
          title="No MCP tools registered"
          hint="Enable mcp.builtin_tools in config to expose tools through the gateway."
        />
      ) : (
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
          {tools.map((tool) => (
            <Card key={tool.function.name} className="flex flex-col gap-3 p-5">
              <div>
                <div className="font-mono text-sm font-semibold">
                  {tool.function.name}
                </div>
                {tool.function.description && (
                  <p className="mt-1 text-sm text-muted-foreground">
                    {tool.function.description}
                  </p>
                )}
              </div>
              <div>
                <div className="mb-1 text-xs uppercase tracking-wide text-muted-foreground">
                  Parameters
                </div>
                <pre className="max-h-64 overflow-auto rounded-md border bg-background/60 p-3 font-mono text-xs">
                  {JSON.stringify(tool.function.parameters ?? {}, null, 2)}
                </pre>
              </div>
            </Card>
          ))}
        </div>
      )}
    </div>
  );
}
