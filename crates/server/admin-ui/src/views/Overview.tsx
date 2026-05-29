import type { Snapshot, ServerInfo } from "@/gen/admin_pb";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { ALL_FREQUENCIES, channelNumber, formatDuration, formatUptime } from "@/lib/utils";

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
            <CardTitle>Lobby</CardTitle>
          </CardHeader>
          <CardContent className="flex flex-col gap-2">
            {lobby.length === 0 && (
              <p className="text-sm text-muted-foreground">No clients between register and join.</p>
            )}
            {lobby.map((m) => (
              <div key={m.id} className="flex items-center justify-between text-sm">
                <span className="font-mono">{m.displayName}</span>
                <span className="font-mono text-xs text-muted-foreground tabular">
                  {formatDuration(Number(m.connectedSecs))}
                </span>
              </div>
            ))}
          </CardContent>
        </Card>
      </div>

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
          className={`mt-1 font-mono text-2xl font-semibold tabular ${
            accent ? "text-warning" : "text-foreground"
          }`}
        >
          {value}
        </p>
        {sub && <p className="text-xs text-muted-foreground">{sub}</p>}
      </CardContent>
    </Card>
  );
}
