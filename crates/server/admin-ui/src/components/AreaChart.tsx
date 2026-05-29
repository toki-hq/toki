import { useId } from "react";

export interface ChartSeries {
  values: number[];
  /** CSS color for the line + gradient fill (e.g. "hsl(var(--primary))"). */
  color: string;
  label: string;
}

/**
 * Dependency-free SVG area/line chart. Renders one or more series sharing
 * a y-scale, with a soft gradient fill and non-scaling strokes so the
 * line weight stays crisp when the SVG stretches to its container width.
 * Matches the phosphor design's hand-rolled traffic chart.
 */
export function AreaChart({
  series,
  height = 150,
  yMax,
}: {
  series: ChartSeries[];
  height?: number;
  /** Force a y-axis ceiling; defaults to the max across all series. */
  yMax?: number;
}) {
  const uid = useId().replace(/:/g, "");
  const w = 760;
  const h = height;
  const n = Math.max(0, ...series.map((s) => s.values.length));

  if (n < 2) {
    return (
      <div
        className="flex items-center justify-center text-xs text-muted-foreground"
        style={{ height: h }}
      >
        Collecting data…
      </div>
    );
  }

  const pad = 6;
  const max = yMax ?? Math.max(1, ...series.flatMap((s) => s.values));
  const xAt = (i: number) => (i / (n - 1)) * w;
  const yAt = (v: number) => h - (Math.max(0, v) / max) * (h - 2 * pad) - pad;
  const line = (vals: number[]) =>
    vals.map((v, i) => `${xAt(i).toFixed(1)},${yAt(v).toFixed(1)}`).join(" ");
  const area = (vals: number[]) => `0,${h} ${line(vals)} ${w},${h}`;

  return (
    <svg
      viewBox={`0 0 ${w} ${h}`}
      preserveAspectRatio="none"
      className="block w-full"
      style={{ height: h }}
    >
      <defs>
        {series.map((s, idx) => (
          <linearGradient key={idx} id={`g-${uid}-${idx}`} x1="0" x2="0" y1="0" y2="1">
            <stop offset="0%" stopColor={s.color} stopOpacity="0.3" />
            <stop offset="100%" stopColor={s.color} stopOpacity="0" />
          </linearGradient>
        ))}
      </defs>
      {[0.25, 0.5, 0.75].map((p) => (
        <line
          key={p}
          x1="0"
          x2={w}
          y1={p * h}
          y2={p * h}
          stroke="hsl(var(--border))"
          strokeWidth="1"
          vectorEffect="non-scaling-stroke"
        />
      ))}
      {series.map((s, idx) => (
        <g key={idx}>
          <polygon points={area(s.values)} fill={`url(#g-${uid}-${idx})`} />
          <polyline
            points={line(s.values)}
            fill="none"
            stroke={s.color}
            strokeWidth="1.6"
            vectorEffect="non-scaling-stroke"
          />
        </g>
      ))}
    </svg>
  );
}
