"use client";

// Generic layered Sankey diagram, hand-rolled in SVG (no charting library) to match the
// dashboard's other charts. Expects a directed acyclic graph whose links only flow forward
// (each builder guarantees this — e.g. Provider→Model→Outcome, or a bipartite from→to for
// transitions). Node columns are derived by longest-path; ribbon widths are proportional to
// value. Themed via the --chart-* CSS tokens so it tracks light/dark.

import { useMemo } from "react";

export interface SankeyNode {
  id: string;
  /** Display label (defaults to id). */
  label?: string;
}

export interface SankeyLink {
  source: string;
  target: string;
  value: number;
}

const NODE_COLORS = [
  "var(--chart-1)",
  "var(--chart-2)",
  "var(--chart-3)",
  "var(--chart-4)",
];

interface Laid {
  width: number;
  height: number;
  nodes: {
    id: string;
    label: string;
    x: number;
    y: number;
    h: number;
    color: string;
  }[];
  paths: {
    key: string;
    d: string;
    width: number;
    color: string;
    title: string;
  }[];
}

const NODE_W = 12;
const H_PAD = 4;

/** Compute a layered layout from the graph. Pure; no DOM. */
function layout(
  nodes: SankeyNode[],
  links: SankeyLink[],
  width: number,
  height: number,
  formatValue: (v: number) => string,
): Laid | null {
  const ids = nodes.map((n) => n.id);
  const idSet = new Set(ids);
  const clean = links.filter((l) => idSet.has(l.source) && idSet.has(l.target) && l.value > 0);
  if (ids.length === 0 || clean.length === 0) return null;

  // Layer assignment by longest path: relax layer[target] = max(_, layer[source]+1) until
  // stable. Bounded by node count (the graph is acyclic by construction).
  const layer = new Map<string, number>(ids.map((id) => [id, 0]));
  for (let iter = 0; iter < ids.length; iter++) {
    let changed = false;
    for (const l of clean) {
      const next = (layer.get(l.source) ?? 0) + 1;
      if (next > (layer.get(l.target) ?? 0)) {
        layer.set(l.target, next);
        changed = true;
      }
    }
    if (!changed) break;
  }
  const maxLayer = Math.max(...layer.values());

  // Throughput per node = max(incoming, outgoing) sum.
  const inSum = new Map<string, number>();
  const outSum = new Map<string, number>();
  for (const l of clean) {
    outSum.set(l.source, (outSum.get(l.source) ?? 0) + l.value);
    inSum.set(l.target, (inSum.get(l.target) ?? 0) + l.value);
  }
  const nodeValue = (id: string) => Math.max(inSum.get(id) ?? 0, outSum.get(id) ?? 0);

  // Group nodes by layer, and pick a value→pixel scale so the tallest column fits.
  const byLayer = new Map<number, string[]>();
  for (const id of ids) {
    const ln = layer.get(id) ?? 0;
    if (!byLayer.has(ln)) byLayer.set(ln, []);
    byLayer.get(ln)!.push(id);
  }
  const gap = 8;
  let maxColValue = 1;
  for (const [, group] of byLayer) {
    const total = group.reduce((a, id) => a + nodeValue(id), 0);
    if (total > maxColValue) maxColValue = total;
  }
  // Reserve gap space in the tallest column.
  const tallestCount = Math.max(...Array.from(byLayer.values(), (g) => g.length));
  const usableH = Math.max(20, height - gap * (tallestCount - 1));
  const scale = usableH / maxColValue;

  const labelOf = (id: string) => nodes.find((n) => n.id === id)?.label ?? id;
  const colW = maxLayer > 0 ? (width - NODE_W - H_PAD * 2) / maxLayer : 0;

  // Place nodes: within a layer, order by descending value; stack from the top, centering
  // shorter columns vertically.
  const pos = new Map<string, { x: number; y: number; h: number; color: string }>();
  const colorFor = new Map<string, string>();
  let colorIdx = 0;
  for (const [ln, group] of Array.from(byLayer.entries()).sort((a, b) => a[0] - b[0])) {
    const ordered = [...group].sort((a, b) => nodeValue(b) - nodeValue(a));
    const colTotal = ordered.reduce((a, id) => a + nodeValue(id) * scale, 0);
    const colGaps = gap * (ordered.length - 1);
    let y = (height - (colTotal + colGaps)) / 2;
    const x = H_PAD + ln * colW;
    for (const id of ordered) {
      const h = Math.max(2, nodeValue(id) * scale);
      // Source-layer nodes seed the palette; others inherit their dominant source color.
      let color = colorFor.get(id);
      if (!color) {
        color = NODE_COLORS[colorIdx % NODE_COLORS.length];
        colorIdx++;
        colorFor.set(id, color);
      }
      pos.set(id, { x, y, h, color });
      y += h + gap;
    }
  }

  // Ribbons: track a running vertical offset at each node's out/in edge.
  const outOff = new Map<string, number>();
  const inOff = new Map<string, number>();
  const paths = clean
    .slice()
    .sort((a, b) => (layer.get(a.source) ?? 0) - (layer.get(b.source) ?? 0))
    .map((l, i) => {
      const s = pos.get(l.source)!;
      const t = pos.get(l.target)!;
      const w = Math.max(1, l.value * scale);
      const so = outOff.get(l.source) ?? 0;
      const to = inOff.get(l.target) ?? 0;
      outOff.set(l.source, so + w);
      inOff.set(l.target, to + w);
      const x0 = s.x + NODE_W;
      const x1 = t.x;
      const y0 = s.y + so + w / 2;
      const y1 = t.y + to + w / 2;
      const mx = (x0 + x1) / 2;
      return {
        key: `${l.source}->${l.target}-${i}`,
        d: `M${x0},${y0} C${mx},${y0} ${mx},${y1} ${x1},${y1}`,
        width: w,
        color: s.color,
        title: `${labelOf(l.source)} → ${labelOf(l.target)}: ${formatValue(l.value)}`,
      };
    });

  const laidNodes = ids
    .filter((id) => pos.has(id))
    .map((id) => {
      const p = pos.get(id)!;
      return { id, label: labelOf(id), x: p.x, y: p.y, h: p.h, color: p.color };
    });

  return { width, height, nodes: laidNodes, paths };
}

export function Sankey({
  nodes,
  links,
  height = 260,
  formatValue = (v) => v.toLocaleString(),
}: {
  nodes: SankeyNode[];
  links: SankeyLink[];
  height?: number;
  formatValue?: (v: number) => string;
}) {
  const width = 720;
  const laid = useMemo(
    () => layout(nodes, links, width, height, formatValue),
    [nodes, links, height, formatValue],
  );

  if (!laid) {
    return (
      <div
        className="flex items-center justify-center rounded-md border border-border text-sm text-muted-foreground"
        style={{ height }}
      >
        Not enough data to chart
      </div>
    );
  }

  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      width="100%"
      style={{ height: "auto", display: "block" }}
      role="img"
      aria-label="Session flow diagram"
    >
      {/* Ribbons first, so nodes paint on top. */}
      <g fill="none">
        {laid.paths.map((p) => (
          <path key={p.key} d={p.d} stroke={p.color} strokeWidth={p.width} strokeOpacity={0.35}>
            <title>{p.title}</title>
          </path>
        ))}
      </g>
      {laid.nodes.map((n) => (
        <g key={n.id}>
          <rect x={n.x} y={n.y} width={NODE_W} height={n.h} fill={n.color} rx={2}>
            <title>{n.label}</title>
          </rect>
          <text
            x={n.x + NODE_W + 6}
            y={n.y + n.h / 2}
            dominantBaseline="middle"
            fontSize={11}
            fill="var(--foreground)"
          >
            {n.label}
          </text>
        </g>
      ))}
    </svg>
  );
}
