import { useMemo, useState } from "react";
import { toast } from "sonner";
import { MoreHorizontal, Pencil, ArrowLeftRight, Zap, Power } from "lucide-react";
import { ConnectError } from "@connectrpc/connect";
import type { Snapshot, Member, Room } from "@/gen/admin_pb";
import { admin } from "@/lib/client";
import { Card } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input, Label } from "@/components/ui/input";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogClose,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubTrigger,
  DropdownMenuSubContent,
} from "@/components/ui/dropdown-menu";
import { ALL_FREQUENCIES, channelNumber, cn, formatDuration } from "@/lib/utils";

function err(e: unknown): string {
  return e instanceof ConnectError ? e.rawMessage : e instanceof Error ? e.message : String(e);
}

export function Rooms({ snapshot }: { snapshot: Snapshot | null }) {
  const rooms = useMemo(() => snapshot?.rooms ?? [], [snapshot]);
  const activeCount = rooms.filter((r) => r.members.length > 0).length;
  const [filter, setFilter] = useState("");
  const [activeOnly, setActiveOnly] = useState(true);
  const [selected, setSelected] = useState<string | null>(null);
  const [renaming, setRenaming] = useState<Member | null>(null);

  // The Watch snapshot only carries rooms the server is tracking (i.e. with
  // members). When "Active only" is off, fill in the rest of the band with
  // synthetic empty rooms so every channel is reachable.
  const allRooms = useMemo<Room[]>(() => {
    if (activeOnly) return rooms;
    const byFreq = new Map(rooms.map((r) => [r.frequency, r]));
    return ALL_FREQUENCIES.map(
      (f) =>
        byFreq.get(f) ??
        ({ $typeName: "toki.admin.v1.Room", frequency: f, members: [] } as Room),
    );
  }, [rooms, activeOnly]);

  const visible = allRooms.filter((r) => {
    if (activeOnly && r.members.length === 0) return false;
    if (filter && !r.frequency.includes(filter)) return false;
    return true;
  });

  const current =
    allRooms.find((r) => r.frequency === selected) ?? visible[0] ?? null;

  return (
    <div className="flex h-[calc(100vh-7.5rem)] flex-col gap-4">
      <h1 className="font-mono text-xs uppercase tracking-widest text-muted-foreground">
        02 · Channels — {visible.length} shown · {activeCount} active
      </h1>
      <div className="grid flex-1 grid-cols-[20rem_1fr] gap-4 overflow-hidden">
        {/* Channel list */}
        <Card className="flex flex-col overflow-hidden">
          <div className="flex flex-col gap-2 border-b border-border p-3">
            <Input
              placeholder="filter frequency…"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              className="font-mono"
            />
            <label className="flex items-center gap-2 text-xs text-muted-foreground">
              <input
                type="checkbox"
                checked={activeOnly}
                onChange={(e) => setActiveOnly(e.target.checked)}
                className="accent-primary"
              />
              Active only
            </label>
          </div>
          <div className="flex-1 overflow-y-auto">
            {visible.map((r) => (
              <ChannelRow
                key={r.frequency}
                room={r}
                selected={current?.frequency === r.frequency}
                onSelect={() => setSelected(r.frequency)}
              />
            ))}
            {visible.length === 0 && (
              <p className="p-4 text-sm text-muted-foreground">No channels match.</p>
            )}
          </div>
        </Card>

        {/* Detail */}
        <Card className="flex flex-col overflow-hidden">
          {current ? (
            <ChannelDetail room={current} onRename={setRenaming} />
          ) : (
            <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
              Select a channel.
            </div>
          )}
        </Card>
      </div>

      {renaming && (
        <RenameDialog member={renaming} onClose={() => setRenaming(null)} />
      )}
    </div>
  );
}

function ChannelRow({
  room,
  selected,
  onSelect,
}: {
  room: Room;
  selected: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      onClick={onSelect}
      className={cn(
        "flex w-full items-center gap-3 border-b border-border/50 px-3 py-2 text-left transition-colors",
        selected ? "bg-primary/10" : "hover:bg-accent/50",
      )}
    >
      <span className="w-6 font-mono text-xs text-muted-foreground tabular">
        {String(channelNumber(room.frequency)).padStart(2, "0")}
      </span>
      <span className="flex-1 font-mono text-sm tabular">{room.frequency}</span>
      {room.holder && (
        <span className="size-2 rounded-full bg-warning shadow-[0_0_6px] shadow-warning" />
      )}
      <span className="w-6 text-right font-mono text-sm tabular">{room.members.length}</span>
    </button>
  );
}

function ChannelDetail({
  room,
  onRename,
}: {
  room: Room;
  onRename: (m: Member) => void;
}) {
  return (
    <>
      <div className="flex items-baseline gap-3 border-b border-border p-4">
        <span className="font-mono text-3xl font-semibold text-primary tabular">
          {room.frequency}
        </span>
        <span className="text-xs text-muted-foreground">MHz · CH {channelNumber(room.frequency)}</span>
        <span className="ml-auto font-mono text-sm text-muted-foreground tabular">
          {room.members.length} members
        </span>
      </div>
      <div className="flex-1 overflow-y-auto">
        {room.members.length === 0 && (
          <p className="p-4 text-sm text-muted-foreground">No members on this frequency.</p>
        )}
        {room.members.map((m) => {
          const isHolder = room.holder === m.id;
          return (
            <div
              key={m.id}
              className={cn(
                "flex items-center gap-3 border-b border-border/50 px-4 py-2.5",
                m.priority && "shadow-[inset_2px_0_0] shadow-warning",
              )}
            >
              <span
                className={cn(
                  "size-2 rounded-full",
                  isHolder
                    ? "bg-warning shadow-[0_0_6px] shadow-warning"
                    : "bg-primary/70 shadow-[0_0_6px] shadow-primary/50",
                )}
              />
              <span className="flex items-center gap-2 font-mono text-sm font-semibold">
                {m.displayName}
                {m.priority && (
                  <Badge variant="warning">
                    <Zap className="size-2.5" /> PRIO
                  </Badge>
                )}
              </span>
              <span
                className={cn(
                  "font-mono text-xs",
                  isHolder ? "text-warning" : "text-primary/80",
                )}
              >
                {isHolder ? "● TX" : "◐ RX"}
              </span>
              <span className="font-mono text-xs text-muted-foreground tabular">
                {formatDuration(Number(m.connectedSecs))}
              </span>
              <span className="ml-auto truncate font-mono text-xs text-muted-foreground/60">
                {m.id.slice(0, 8)}
              </span>
              <MemberMenu member={m} onRename={() => onRename(m)} />
            </div>
          );
        })}
      </div>
    </>
  );
}

function MemberMenu({ member, onRename }: { member: Member; onRename: () => void }) {
  async function move(frequency: string) {
    try {
      await admin.moveClient({ id: member.id, frequency });
      toast.success(`Moved ${member.displayName} → ${frequency}`);
    } catch (e) {
      toast.error(`Move failed: ${err(e)}`);
    }
  }
  async function togglePriority() {
    try {
      await admin.setPriority({ id: member.id, grant: !member.priority });
      toast.success(
        member.priority ? `Priority revoked from ${member.displayName}` : `${member.displayName} promoted to priority`,
      );
    } catch (e) {
      toast.error(`Priority change failed: ${err(e)}`);
    }
  }
  async function kick() {
    if (!confirm(`Kick ${member.displayName}? They'll be disconnected immediately.`)) return;
    try {
      await admin.kickClient({ id: member.id });
      toast.warning(`Kicked ${member.displayName}`);
    } catch (e) {
      toast.error(`Kick failed: ${err(e)}`);
    }
  }

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" className="size-7">
          <MoreHorizontal />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <DropdownMenuLabel>{member.displayName}</DropdownMenuLabel>
        <DropdownMenuSeparator />
        <DropdownMenuItem onSelect={onRename}>
          <Pencil /> Rename callsign
        </DropdownMenuItem>
        <DropdownMenuSub>
          <DropdownMenuSubTrigger>
            <ArrowLeftRight /> Move to channel…
          </DropdownMenuSubTrigger>
          <DropdownMenuSubContent>
            {ALL_FREQUENCIES.map((f) => (
              <DropdownMenuItem key={f} onSelect={() => void move(f)} className="font-mono">
                {String(channelNumber(f)).padStart(2, "0")} · {f}
              </DropdownMenuItem>
            ))}
          </DropdownMenuSubContent>
        </DropdownMenuSub>
        <DropdownMenuItem onSelect={() => void togglePriority()}>
          <Zap /> {member.priority ? "Revoke priority" : "Promote to priority"}
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem onSelect={() => void kick()} className="text-warning">
          <Power /> Kick (disconnect)
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function RenameDialog({ member, onClose }: { member: Member; onClose: () => void }) {
  const [name, setName] = useState(member.displayName);
  const [busy, setBusy] = useState(false);
  async function save() {
    setBusy(true);
    try {
      await admin.renameClient({ id: member.id, displayName: name });
      toast.success(`Renamed to ${name}`);
      onClose();
    } catch (e) {
      toast.error(`Rename failed: ${err(e)}`);
    } finally {
      setBusy(false);
    }
  }
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Rename {member.displayName}</DialogTitle>
        </DialogHeader>
        <div className="flex flex-col gap-3">
          <Label htmlFor="callsign">New callsign</Label>
          <Input
            id="callsign"
            value={name}
            autoFocus
            onChange={(e) => setName(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && void save()}
          />
          <div className="mt-2 flex justify-end gap-2">
            <DialogClose asChild>
              <Button variant="ghost">Cancel</Button>
            </DialogClose>
            <Button onClick={() => void save()} disabled={busy}>
              {busy ? "Saving…" : "Rename"}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
