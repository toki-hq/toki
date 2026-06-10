import { useCallback, useEffect, useState } from "react";
import { toast } from "sonner";
import { RefreshCw, Fingerprint, Laptop, ShieldOff } from "lucide-react";
import { ConnectError } from "@connectrpc/connect";
import type { BanRecord } from "@/gen/admin_pb";
import { admin } from "@/lib/client";
import { Card } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";

function err(e: unknown): string {
  return e instanceof ConnectError ? e.rawMessage : e instanceof Error ? e.message : String(e);
}

function fmtWhen(tsUnix: number): string {
  const d = new Date(tsUnix * 1000);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/// Active identity bans: who's banned, why, by whom, and the lift
/// action. Bans are issued from a member's row menu in Channels; this
/// view is the review/lift side.
export function Bans() {
  const [bans, setBans] = useState<BanRecord[]>([]);
  const [busy, setBusy] = useState(false);

  const reload = useCallback(async () => {
    setBusy(true);
    try {
      const res = await admin.listBans({});
      setBans(res.bans);
    } catch (e) {
      toast.error(`Load bans failed: ${err(e)}`);
    } finally {
      setBusy(false);
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  async function lift(ban: BanRecord) {
    if (!confirm(`Lift the ban on ${ban.lastCallsign} (${ban.displayId})?`)) return;
    try {
      await admin.liftBan({ pubkey: ban.pubkey });
      toast.success(`Ban lifted for ${ban.displayId}`);
      void reload();
    } catch (e) {
      toast.error(`Lift failed: ${err(e)}`);
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-lg font-semibold">Bans</h1>
          <p className="text-sm text-muted-foreground">
            {bans.length === 0
              ? "No active bans."
              : `${bans.length} banned ${bans.length === 1 ? "identity" : "identities"}.`}{" "}
            Ban a member from its row menu in Channels.
          </p>
        </div>
        <Button variant="ghost" size="icon" disabled={busy} onClick={() => void reload()}>
          <RefreshCw className={busy ? "animate-spin" : undefined} />
        </Button>
      </div>

      <Card className="overflow-hidden">
        {bans.length === 0 && (
          <p className="p-6 text-center text-sm text-muted-foreground">
            Nothing here — the airwaves are civil.
          </p>
        )}
        {bans.map((ban) => (
          <div
            key={ban.pubkey}
            className="flex items-center gap-3 border-b border-border/50 px-4 py-3 last:border-b-0"
          >
            <Badge variant="primary" className="cursor-default" title={ban.pubkey}>
              <Fingerprint className="size-2.5" />
              {ban.displayId}
            </Badge>
            <span className="font-mono text-sm font-semibold">{ban.lastCallsign}</span>
            {ban.machineHash && (
              <Badge
                variant="warning"
                title={`Machine banned: ${ban.machineHash.slice(0, 16)}…`}
              >
                <Laptop className="size-2.5" /> machine
              </Badge>
            )}
            <span className="min-w-0 flex-1 truncate text-sm text-muted-foreground">
              {ban.reason || "—"}
            </span>
            <span className="font-mono text-xs text-muted-foreground/70">
              by {ban.bannedBy} · {fmtWhen(Number(ban.bannedAtUnix))}
            </span>
            <Button variant="ghost" size="sm" onClick={() => void lift(ban)}>
              <ShieldOff /> Lift
            </Button>
          </div>
        ))}
      </Card>
    </div>
  );
}
