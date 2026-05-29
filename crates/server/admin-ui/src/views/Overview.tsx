import { useEffect, useState } from "react";
import type {
  Snapshot,
  ServerInfo,
  MetricSample,
  ServerHealth,
  AuditEntry,
} from "@/gen/admin_pb";
import { MetricsWindow, AuditFilter } from "@/gen/admin_pb";
import { admin } from "@/lib/client";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { ChartWithAxes } from "@/components/AreaChart";
import {
  ALL_FREQUENCIES,
  channelNumber,
  cn,
  formatBytes,
  formatClock,
  formatRate,
  formatUptime,
} from "@/lib/utils";
import { auditTone, auditIcon } from "@/lib/audit";

const PRIMARY = "hsl(var(--primary))";
const WARNING = "hsl(var(--warning))";

/** A point in the client-side live buffer (1 Hz, off the Watch stream). */
interface LivePoint {
  rx: number;
  tx: number;
  users: number;
}
/** ~3 min of 1 Hz live history. */
const LIVE_CAP = 180;

type Win = "live" | "hour" | "day" | "week";
const WINDOWS: { id: Win; label: string }[] = [
  { id: "live", label: "LIVE" },
  { id: "hour", label: "1H" },
  { id: "day", label: "24H" },
  { id: "week", label: "7D" },
];
const WIN_ENUM: Record<Exclude<Win, "live">, MetricsWindow> = {
  hour: MetricsWindow.HOUR,
  day: MetricsWindow.DAY,
  week: MetricsWindow.WEEK,
};

export function Overview({
  snapshot,
  info,
}: {
  snapshot: Snapshot | null;
  info: ServerInfo | null;
}) {
  const rooms = snapshot?.rooms ?? [];
  const lobby = snapshot?.lobby ?? [];
  const peers = rooms.reduce((n, r) => n + r.members.length, 0) + lobby.length;
  const transmitting = rooms.filter((r) => r.holder).length;
  const activeChans = rooms.filter((r) => r.members.length > 0).length;

  const busiest = [...rooms]
    .filter((r) => r.members.length > 0)
    .sort((a, b) => b.members.length - a.members.length)
    .slice(0, 8);
  const maxMembers = Math.max(1, ...busiest.map((r) => r.members.length));

  // ── Data: live buffer (Watch stream), windowed history, health, audit ──
  const [window, setWindow] = useState<Win>("live");
  const [samples, setSamples] = useState<MetricSample[]>([]);
  const [live, setLive] = useState<LivePoint[]>([]);
  const [health, setHealth] = useState<ServerHealth | null>(null);
  const [recent, setRecent] = useState<AuditEntry[]>([]);

  // Append a point to the live ring on every Watch snapshot (~1 Hz). This
  // is the real-time bandwidth source — no polling lag.
  useEffect(() => {
    if (!snapshot) return;
    const u = rooms.reduce((n, r) => n + r.members.length, 0) + lobby.length;
    setLive((prev) =>
      [
        ...prev,
        {
          rx: Number(snapshot.rxBytesPerSec),
          tx: Number(snapshot.txBytesPerSec),
          users: u,
        },
      ].slice(-LIVE_CAP),
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [snapshot]);

  // Historical windows poll the persisted 1-minute series; LIVE doesn't.
  useEffect(() => {
    if (window === "live") return;
    let alive = true;
    const load = () =>
      admin
        .getMetrics({ window: WIN_ENUM[window] })
        .then((r) => alive && setSamples(r.samples))
        .catch(() => {});
    void load();
    const t = setInterval(load, 30_000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [window]);

  useEffect(() => {
    let alive = true;
    const load = () =>
      admin
        .getServerHealth({})
        .then((r) => alive && setHealth(r))
        .catch(() => {});
    void load();
    const t = setInterval(load, 5_000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, []);

  useEffect(() => {
    let alive = true;
    const load = () =>
      admin
        .getAuditLog({ filter: AuditFilter.ALL, limit: 7, beforeId: 0n })
        .then((r) => alive && setRecent(r.entries))
        .catch(() => {});
    void load();
    const t = setInterval(load, 10_000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, []);

  const isLive = window === "live";
  const rx = isLive ? live.map((p) => p.rx) : samples.map((s) => Number(s.rxBytesPerSec));
  const tx = isLive ? live.map((p) => p.tx) : samples.map((s) => Number(s.txBytesPerSec));
  const usersSeries = isLive ? live.map((p) => p.users) : samples.map((s) => s.users);
  const nowRx = isLive ? (live.at(-1)?.rx ?? 0) : Number(samples.at(-1)?.rxBytesPerSec ?? 0);
  const nowTx = isLive ? (live.at(-1)?.tx ?? 0) : Number(samples.at(-1)?.txBytesPerSec ?? 0);
  const nowUsers = isLive ? (live.at(-1)?.users ?? peers) : (samples.at(-1)?.users ?? peers);

  // X-axis labels (oldest → newest). LIVE counts seconds back from now;
  // historical uses sample clock times (or dates for the 7-day window).
  const xLabels: string[] = (() => {
    if (isLive) {
      const n = live.length;
      if (n < 2) return [];
      return [`-${n}s`, `-${Math.floor(n / 2)}s`, "now"];
    }
    if (samples.length < 2) return [];
    const at = (i: number) => Number(samples[i].tsUnix);
    const mid = Math.floor(samples.length / 2);
    const fmt =
      window === "week"
        ? (ts: number) => {
            const d = new Date(ts * 1000);
            return `${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
          }
        : formatClock;
    return [fmt(at(0)), fmt(at(mid)), fmt(at(samples.length - 1))];
  })();

  return (
    <div className="flex flex-col gap-6">
      <h1 className="font-mono text-xs uppercase tracking-widest text-muted-foreground">
        01 · Overview
      </h1>

      <div className="grid grid-cols-2 gap-4 lg:grid-cols-4">
        <Kpi label="Uptime" value={formatUptime(Number(snapshot?.serverUptimeSecs ?? 0n))} />
        <Kpi label="Peers online" value={String(peers)} sub="clients" />
        <Kpi
          label="Transmitting"
          value={String(transmitting)}
          sub={transmitting > 0 ? "active now" : "silent"}
          accent={transmitting > 0}
        />
        <Kpi label="Active channels" value={`${activeChans}`} sub={`of ${ALL_FREQUENCIES.length}`} />
      </div>

      {/* ── Priority: bandwidth + users over time ─────────────────── */}
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader className="flex-row items-center justify-between space-y-0">
            <CardTitle>Bandwidth (voice relay)</CardTitle>
            <WindowPicker value={window} onChange={setWindow} />
          </CardHeader>
          <CardContent>
            <ChartWithAxes
              height={150}
              yFormat={formatRate}
              xLabels={xLabels}
              series={[
                { values: rx, color: PRIMARY, label: "ingress" },
                { values: tx, color: WARNING, label: "egress" },
              ]}
            />
            <div className="mt-3 flex gap-5 border-t border-border pt-3 font-mono text-xs">
              <Legend color={PRIMARY} label={`IN  ${formatRate(nowRx)}`} />
              <Legend color={WARNING} label={`OUT ${formatRate(nowTx)}`} />
              <span className="ml-auto text-muted-foreground">
                {isLive ? "live · 1 s" : "UDP audio only"}
              </span>
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardHeader className="flex-row items-center justify-between space-y-0">
            <CardTitle>Users over time</CardTitle>
            <WindowPicker value={window} onChange={setWindow} />
          </CardHeader>
          <CardContent>
            <ChartWithAxes
              height={150}
              yFormat={(v) => String(Math.round(v))}
              xLabels={xLabels}
              series={[{ values: usersSeries, color: PRIMARY, label: "users" }]}
            />
            <div className="mt-3 flex gap-5 border-t border-border pt-3 font-mono text-xs">
              <Legend color={PRIMARY} label={`NOW ${nowUsers}`} />
              <span className="text-muted-foreground">PEAK {Math.max(0, ...usersSeries)}</span>
              <span className="ml-auto font-mono text-muted-foreground">users</span>
            </div>
          </CardContent>
        </Card>
      </div>

      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>Busiest channels</CardTitle>
          </CardHeader>
          <CardContent className="flex flex-col gap-2">
            {busiest.length === 0 && (
              <p className="text-sm text-muted-foreground">No active channels.</p>
            )}
            {busiest.map((r) => (
              <div key={r.frequency} className="flex items-center gap-3">
                <span className="w-6 font-mono text-xs text-muted-foreground tabular">
                  {String(channelNumber(r.frequency)).padStart(2, "0")}
                </span>
                <span className="w-16 font-mono text-sm tabular">{r.frequency}</span>
                <div className="h-2 flex-1 overflow-hidden rounded bg-muted">
                  <div
                    className="h-full rounded bg-primary"
                    style={{ width: `${(r.members.length / maxMembers) * 100}%` }}
                  />
                </div>
                <span className="w-6 text-right font-mono text-sm tabular">
                  {r.members.length}
                </span>
              </div>
            ))}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Server health</CardTitle>
          </CardHeader>
          <CardContent className="flex flex-col gap-2.5">
            <HealthRow
              label="CPU"
              value={health ? `${health.cpuPercent.toFixed(0)}%` : "—"}
              bar={health ? health.cpuPercent / 100 : undefined}
            />
            <HealthRow
              label="MEM"
              value={
                health && health.memTotalBytes > 0n
                  ? `${formatBytes(Number(health.memUsedBytes))} / ${formatBytes(Number(health.memTotalBytes))}`
                  : "—"
              }
              bar={
                health && health.memTotalBytes > 0n
                  ? Number(health.memUsedBytes) / Number(health.memTotalBytes)
                  : undefined
              }
            />
            <HealthRow
              label="DISK"
              value={
                health && health.diskTotalBytes > 0n
                  ? `${formatBytes(Number(health.diskUsedBytes))} / ${formatBytes(Number(health.diskTotalBytes))}`
                  : "—"
              }
              bar={
                health && health.diskTotalBytes > 0n
                  ? Number(health.diskUsedBytes) / Number(health.diskTotalBytes)
                  : undefined
              }
            />
            <HealthRow label="NET I/O" value={`${formatRate(nowRx)} · ${formatRate(nowTx)}`} />
          </CardContent>
        </Card>
      </div>

      {/* Recent activity (audit) */}
      <Card>
        <CardHeader>
          <CardTitle>Recent activity</CardTitle>
        </CardHeader>
        <CardContent className="flex flex-col">
          {recent.length === 0 && (
            <p className="text-sm text-muted-foreground">No audit events yet.</p>
          )}
          {recent.map((e) => (
            <div
              key={String(e.id)}
              className="flex items-center gap-3 border-b border-border/40 py-1.5 last:border-0"
            >
              <span className={cn("w-4 text-center font-mono text-sm", auditTone(e.kind))}>
                {auditIcon(e.kind)}
              </span>
              <span className="w-12 font-mono text-[11px] text-muted-foreground tabular">
                {formatClock(Number(e.tsUnix))}
              </span>
              <span className={cn("w-28 font-mono text-[11px] uppercase tracking-wider", auditTone(e.kind))}>
                {e.kind}
              </span>
              <span className="w-24 truncate font-mono text-[11px] font-semibold">{e.actor}</span>
              <span className="flex-1 truncate text-xs text-muted-foreground">{e.detail}</span>
            </div>
          ))}
        </CardContent>
      </Card>

      {info && (
        <p className="font-mono text-[11px] text-muted-foreground">
          bind {info.adminBind} · started{" "}
          {new Date(Number(info.startedAtUnix) * 1000).toISOString().slice(0, 19).replace("T", " ")}{" "}
          UTC
        </p>
      )}
    </div>
  );
}

function WindowPicker({
  value,
  onChange,
}: {
  value: Win;
  onChange: (w: Win) => void;
}) {
  return (
    <div className="flex gap-1 rounded-md border border-border p-0.5">
      {WINDOWS.map((w) => (
        <button
          key={w.id}
          onClick={() => onChange(w.id)}
          className={cn(
            "rounded px-2 py-0.5 font-mono text-[10px] tracking-wider transition-colors",
            value === w.id
              ? "bg-primary/15 text-primary"
              : "text-muted-foreground hover:text-foreground",
          )}
        >
          {w.label}
        </button>
      ))}
    </div>
  );
}

function Legend({ color, label }: { color: string; label: string }) {
  return (
    <span className="flex items-center gap-1.5 text-muted-foreground">
      <span className="size-2 rounded-full" style={{ background: color }} />
      {label}
    </span>
  );
}

function HealthRow({ label, value, bar }: { label: string; value: string; bar?: number }) {
  return (
    <div className="flex items-center gap-3">
      <span className="w-16 font-mono text-[10px] uppercase tracking-wider text-muted-foreground">
        {label}
      </span>
      {bar !== undefined ? (
        <div className="h-2 flex-1 overflow-hidden rounded bg-muted">
          <div
            className="h-full rounded bg-primary"
            style={{ width: `${Math.min(100, Math.max(0, bar * 100))}%` }}
          />
        </div>
      ) : (
        <span className="flex-1" />
      )}
      <span className="font-mono text-xs tabular">{value}</span>
    </div>
  );
}

function Kpi({
  label,
  value,
  sub,
  accent,
}: {
  label: string;
  value: string;
  sub?: string;
  accent?: boolean;
}) {
  return (
    <Card>
      <CardContent className="p-5">
        <p className="text-[10px] uppercase tracking-wider text-muted-foreground">{label}</p>
        <p
          className={cn(
            "mt-1 font-mono text-2xl font-semibold tabular",
            accent ? "text-warning" : "text-foreground",
          )}
        >
          {value}
        </p>
        {sub && <p className="text-xs text-muted-foreground">{sub}</p>}
      </CardContent>
    </Card>
  );
}
