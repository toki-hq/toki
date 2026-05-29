import { useCallback, useEffect, useState } from "react";
import { toast } from "sonner";
import { Download, RefreshCw } from "lucide-react";
import type { AuditEntry } from "@/gen/admin_pb";
import { AuditFilter } from "@/gen/admin_pb";
import { admin } from "@/lib/client";
import { Card } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { auditIcon, auditTone } from "@/lib/audit";

const PAGE = 50;

const TABS: { id: AuditFilter; label: string }[] = [
  { id: AuditFilter.ALL, label: "All" },
  { id: AuditFilter.ADMIN, label: "Admin" },
  { id: AuditFilter.CONNECTIONS, label: "Connections" },
  { id: AuditFilter.SECURITY, label: "Security" },
];

function fmtWhen(tsUnix: number): string {
  const d = new Date(tsUnix * 1000);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

export function Audit() {
  const [filter, setFilter] = useState<AuditFilter>(AuditFilter.ALL);
  const [entries, setEntries] = useState<AuditEntry[]>([]);
  const [total, setTotal] = useState(0);
  const [counts, setCounts] = useState<Record<number, number>>({});
  const [busy, setBusy] = useState(false);

  // (Re)load the first page for the active filter, and refresh the
  // per-tab counts.
  const reload = useCallback(async () => {
    setBusy(true);
    try {
      const res = await admin.getAuditLog({ filter, limit: PAGE, beforeId: 0n });
      setEntries(res.entries);
      setTotal(Number(res.total));
      // Cheap per-tab totals (limit 1; `total` is filter-wide).
      const totals = await Promise.all(
        TABS.map((t) =>
          admin
            .getAuditLog({ filter: t.id, limit: 1, beforeId: 0n })
            .then((r) => [t.id, Number(r.total)] as const)
            .catch(() => [t.id, 0] as const),
        ),
      );
      setCounts(Object.fromEntries(totals));
    } catch (e) {
      toast.error(`Load audit log failed: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setBusy(false);
    }
  }, [filter]);

  useEffect(() => {
    void reload();
  }, [reload]);

  async function loadMore() {
    const oldest = entries.at(-1);
    if (!oldest) return;
    setBusy(true);
    try {
      const res = await admin.getAuditLog({ filter, limit: PAGE, beforeId: oldest.id });
      setEntries((prev) => [...prev, ...res.entries]);
    } catch (e) {
      toast.error(`Load more failed: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setBusy(false);
    }
  }

  // Page through everything for the active filter and download as JSONL.
  async function exportJsonl() {
    setBusy(true);
    try {
      const all: AuditEntry[] = [];
      let before = 0n;
      for (;;) {
        const res = await admin.getAuditLog({ filter, limit: 500, beforeId: before });
        all.push(...res.entries);
        if (res.entries.length < 500) break;
        before = res.entries[res.entries.length - 1].id;
      }
      const lines = all
        .map((e) =>
          JSON.stringify({
            id: Number(e.id),
            ts: Number(e.tsUnix),
            kind: e.kind,
            actor: e.actor,
            frequency: e.frequency,
            detail: e.detail,
          }),
        )
        .join("\n");
      const blob = new Blob([lines], { type: "application/x-ndjson" });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `toki-audit-${new Date().toISOString().slice(0, 10)}.jsonl`;
      a.click();
      URL.revokeObjectURL(url);
      toast.success(`Exported ${all.length} entries`);
    } catch (e) {
      toast.error(`Export failed: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center justify-between gap-3">
        <h1 className="font-mono text-xs uppercase tracking-widest text-muted-foreground">
          04 · Audit log
        </h1>
        <div className="flex gap-2">
          <Button variant="outline" size="sm" disabled={busy} onClick={() => void reload()}>
            <RefreshCw /> Refresh
          </Button>
          <Button variant="outline" size="sm" disabled={busy} onClick={() => void exportJsonl()}>
            <Download /> Export JSONL
          </Button>
        </div>
      </div>

      <div className="flex flex-wrap items-center gap-2">
        {TABS.map((t) => (
          <button
            key={t.id}
            onClick={() => setFilter(t.id)}
            className={cn(
              "flex items-center gap-2 rounded px-3 py-1 font-mono text-[11px] uppercase tracking-wider transition-colors",
              filter === t.id
                ? "bg-primary/15 text-primary"
                : "border border-border text-muted-foreground hover:text-foreground",
            )}
          >
            {t.label}
            <span className="tabular">{counts[t.id] ?? 0}</span>
          </button>
        ))}
        <span className="ml-auto font-mono text-[11px] text-muted-foreground">
          showing {entries.length} of {total}
        </span>
      </div>

      <Card className="overflow-hidden p-0">
        <div className="flex items-center gap-3 border-b border-border px-4 py-2 font-mono text-[10px] uppercase tracking-wider text-muted-foreground">
          <span className="w-4" />
          <span className="w-32">When</span>
          <span className="w-28">Event</span>
          <span className="w-24">Actor</span>
          <span className="w-16">Freq</span>
          <span className="flex-1">Detail</span>
        </div>
        <div className="max-h-[calc(100vh-16rem)] overflow-y-auto">
          {entries.length === 0 && !busy && (
            <p className="p-4 text-sm text-muted-foreground">No matching audit events.</p>
          )}
          {entries.map((e) => (
            <div
              key={String(e.id)}
              className="flex items-center gap-3 border-b border-border/40 px-4 py-1.5 last:border-0"
            >
              <span className={cn("w-4 text-center font-mono text-sm", auditTone(e.kind))}>
                {auditIcon(e.kind)}
              </span>
              <span className="w-32 font-mono text-[11px] text-muted-foreground tabular">
                {fmtWhen(Number(e.tsUnix))}
              </span>
              <span className={cn("w-28 font-mono text-[11px] uppercase tracking-wider", auditTone(e.kind))}>
                {e.kind}
              </span>
              <span
                className={cn(
                  "w-24 truncate font-mono text-[11px] font-semibold",
                  e.actor === "SYSTEM" && "text-muted-foreground",
                )}
              >
                {e.actor}
              </span>
              <span className="w-16 font-mono text-[11px] text-muted-foreground tabular">
                {e.frequency || "—"}
              </span>
              <span className="flex-1 truncate text-xs text-muted-foreground">{e.detail}</span>
            </div>
          ))}
        </div>
      </Card>

      {entries.length < total && (
        <Button variant="outline" size="sm" className="self-center" disabled={busy} onClick={() => void loadMore()}>
          Load more
        </Button>
      )}
    </div>
  );
}
