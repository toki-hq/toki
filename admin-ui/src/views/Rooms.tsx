import { useEffect, useMemo, useState } from "react";
import { toast } from "sonner";
import {
  MoreHorizontal,
  Pencil,
  ArrowLeftRight,
  Zap,
  Power,
  Tag,
  Eraser,
  Check,
  Fingerprint,
  Ban,
  MicOff,
  Mic,
} from "lucide-react";
import { ConnectError } from "@connectrpc/connect";
import type { Snapshot, Member, Room } from "@/gen/admin_pb";
import { admin } from "@/lib/client";
import { Card } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input, Label } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
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
  // Admin-assigned names live in the snapshot's channelNames map (all
  // named frequencies, occupied or not). Names persist even while the
  // feature is off, so the toggle is fetched separately to gate editing.
  const names = snapshot?.channelNames ?? {};
  // Channel-wide mutes (all muted frequencies, occupied or not). Used to
  // flag synthetic empty rooms as muted too — you can mute an empty channel.
  const mutedChannels = useMemo(
    () => new Set(snapshot?.mutedChannels ?? []),
    [snapshot],
  );
  const [namedEnabled, setNamedEnabled] = useState(false);
  const [filter, setFilter] = useState("");
  const [activeOnly, setActiveOnly] = useState(true);
  const [selected, setSelected] = useState<string | null>(null);
  const [renaming, setRenaming] = useState<Member | null>(null);
  const [banning, setBanning] = useState<Member | null>(null);

  useEffect(() => {
    // One-shot on mount (the section remounts on each visit, so a
    // toggle flipped in Settings is reflected when you return here).
    admin
      .getServerConfig({})
      .then((c) => setNamedEnabled(c.namedChannelsEnabled))
      .catch(() => setNamedEnabled(false));
  }, []);

  async function clearAll() {
    if (!confirm("Clear the names of ALL channels? This can't be undone.")) return;
    try {
      await admin.clearAllChannelNames({});
      toast.success("All channel names cleared");
    } catch (e) {
      toast.error(`Clear all failed: ${err(e)}`);
    }
  }

  // The Watch snapshot only carries rooms the server is tracking (i.e. with
  // members). When "Active only" is off, fill in the rest of the band with
  // synthetic empty rooms so every channel is reachable.
  const allRooms = useMemo<Room[]>(() => {
    if (activeOnly) return rooms;
    const byFreq = new Map(rooms.map((r) => [r.frequency, r]));
    return ALL_FREQUENCIES.map(
      (f) =>
        byFreq.get(f) ??
        ({
          $typeName: "toki.admin.v1.Room",
          frequency: f,
          members: [],
          muted: mutedChannels.has(f),
        } as Room),
    );
  }, [rooms, activeOnly, mutedChannels]);

  const visible = allRooms.filter((r) => {
    if (activeOnly && r.members.length === 0) return false;
    if (filter && !r.frequency.includes(filter)) return false;
    return true;
  });

  const current =
    allRooms.find((r) => r.frequency === selected) ?? visible[0] ?? null;

  return (
    <div className="flex h-[calc(100vh-7.5rem)] flex-col gap-4">
      <div className="flex items-center justify-between gap-3">
        <h1 className="font-mono text-xs uppercase tracking-widest text-muted-foreground">
          02 · Channels — {visible.length} shown · {activeCount} active
        </h1>
        {namedEnabled && Object.keys(names).length > 0 && (
          <Button variant="outline" size="sm" onClick={() => void clearAll()}>
            <Eraser /> Clear all names
          </Button>
        )}
      </div>
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
                name={names[r.frequency]}
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
            <ChannelDetail
              room={current}
              name={names[current.frequency]}
              namedEnabled={namedEnabled}
              onRename={setRenaming}
              onBan={setBanning}
            />
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
      {banning && <BanDialog member={banning} onClose={() => setBanning(null)} />}
    </div>
  );
}

function ChannelRow({
  room,
  name,
  selected,
  onSelect,
}: {
  room: Room;
  name?: string;
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
      <span className="flex min-w-0 flex-1 flex-col">
        <span className="font-mono text-sm tabular">{room.frequency}</span>
        <span
          className={cn(
            "truncate text-xs",
            name ? "text-primary/80" : "font-mono text-muted-foreground/50",
          )}
        >
          {name || "--"}
        </span>
      </span>
      {room.holder && (
        <span className="size-2 rounded-full bg-warning shadow-[0_0_6px] shadow-warning" />
      )}
      <span className="w-6 text-right font-mono text-sm tabular">{room.members.length}</span>
    </button>
  );
}

function ChannelDetail({
  room,
  name,
  namedEnabled,
  onRename,
  onBan,
}: {
  room: Room;
  name?: string;
  namedEnabled: boolean;
  onRename: (m: Member) => void;
  onBan: (m: Member) => void;
}) {
  return (
    <>
      <div className="flex items-baseline gap-3 border-b border-border p-4">
        <span className="font-mono text-3xl font-semibold text-primary tabular">
          {room.frequency}
        </span>
        <span className="text-xs text-muted-foreground">MHz · CH {channelNumber(room.frequency)}</span>
        {name && <span className="font-mono text-sm text-primary/90">“{name}”</span>}
        {room.muted && (
          <Badge variant="destructive">
            <MicOff className="size-2.5" /> MUTED
          </Badge>
        )}
        <span className="ml-auto flex items-center gap-3">
          <ChannelMuteToggle frequency={room.frequency} muted={room.muted} />
          <span className="font-mono text-sm text-muted-foreground tabular">
            {room.members.length} members
          </span>
        </span>
      </div>
      <NameEditor frequency={room.frequency} name={name} enabled={namedEnabled} />
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
                {m.muted && (
                  <Badge variant="destructive">
                    <MicOff className="size-2.5" /> MUTED
                  </Badge>
                )}
                <IdentityBadge member={m} />
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
              <QualityReadout member={m} />
              <span className="ml-auto truncate font-mono text-xs text-muted-foreground/60">
                {m.id.slice(0, 8)}
              </span>
              <MemberMenu member={m} onRename={() => onRename(m)} onBan={() => onBan(m)} />
            </div>
          );
        })}
      </div>
    </>
  );
}

function ChannelMuteToggle({ frequency, muted }: { frequency: string; muted: boolean }) {
  const [busy, setBusy] = useState(false);
  async function toggle() {
    setBusy(true);
    try {
      await admin.setChannelMute({ frequency, muted: !muted });
      toast.success(muted ? `Unmuted ${frequency}` : `Muted ${frequency}`);
    } catch (e) {
      toast.error(`Channel mute failed: ${err(e)}`);
    } finally {
      setBusy(false);
    }
  }
  return (
    <Button
      variant={muted ? "default" : "ghost"}
      size="sm"
      disabled={busy}
      onClick={() => void toggle()}
      title={muted ? "Allow transmission on this channel" : "Silence all transmission on this channel"}
    >
      {muted ? <Mic /> : <MicOff />} {muted ? "Unmute channel" : "Mute channel"}
    </Button>
  );
}

function NameEditor({
  frequency,
  name,
  enabled,
}: {
  frequency: string;
  name?: string;
  enabled: boolean;
}) {
  const [value, setValue] = useState(name ?? "");
  const [busy, setBusy] = useState(false);

  // Re-sync when the selected channel changes or a live rename arrives
  // via the Watch stream (so the field tracks the authoritative name).
  useEffect(() => {
    setValue(name ?? "");
  }, [frequency, name]);

  const dirty = value.trim() !== (name ?? "");

  async function save() {
    setBusy(true);
    try {
      await admin.setChannelName({ frequency, name: value.trim() });
      toast.success(
        value.trim() ? `Named ${frequency} “${value.trim()}”` : `Cleared name on ${frequency}`,
      );
    } catch (e) {
      toast.error(`Save failed: ${err(e)}`);
    } finally {
      setBusy(false);
    }
  }
  async function clear() {
    setBusy(true);
    try {
      await admin.setChannelName({ frequency, name: "" });
      setValue("");
      toast.success(`Cleared name on ${frequency}`);
    } catch (e) {
      toast.error(`Clear failed: ${err(e)}`);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex flex-col gap-2 border-b border-border bg-card/40 p-3">
      <Label className="flex items-center gap-1.5 text-xs text-muted-foreground">
        <Tag className="size-3" /> Channel name
      </Label>
      {enabled ? (
        <div className="flex items-center gap-2">
          <Input
            value={value}
            maxLength={16}
            placeholder="unnamed"
            disabled={busy}
            onChange={(e) => setValue(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && dirty && void save()}
            className="font-mono"
          />
          <span className="w-10 text-right font-mono text-xs text-muted-foreground tabular">
            {value.length}/16
          </span>
          <Button size="sm" disabled={!dirty || busy} onClick={() => void save()}>
            <Check /> Save
          </Button>
          <Button
            variant="outline"
            size="sm"
            disabled={busy || !name}
            onClick={() => void clear()}
          >
            <Eraser /> Clear
          </Button>
        </div>
      ) : (
        <p className="text-xs text-muted-foreground">
          {name ? (
            <>
              Currently <span className="font-mono text-primary/80">“{name}”</span> (dormant).{" "}
            </>
          ) : null}
          Enable <span className="font-medium">Named channels</span> in Server settings to edit.
        </p>
      )}
    </div>
  );
}

/// Keypair-identity badge for a member row. Shows the durable identity
/// string (e.g. 7Q4XF9KB) next to the freely-chosen display name;
/// hovering reveals the full public key, the machine-hash prefix, and
/// when this identity was first seen by the server. Members that
/// registered without an identity (pre-identity clients) render nothing.
/// Compact connection-quality readout: RTT / jitter / loss as the client
/// last reported them, colour-coded by a worst-of-three health score.
/// Shows a dash until the client's first report lands (a just-connected
/// member, or a client too old to report). Mirrors the client strip's
/// signal score so operator and user see the same verdict.
function QualityReadout({ member }: { member: Member }) {
  if (!member.qualityFresh) {
    return <span className="font-mono text-xs text-muted-foreground/40 tabular">—</span>;
  }
  const rtt = member.rttMs;
  const jit = member.jitterMs;
  const lossPct = member.lossPctCenti / 100;
  // Worst-of-three → colour, matching the client's bar thresholds.
  const bad = rtt >= 250 || jit >= 50 || member.lossPctCenti >= 500;
  const marginal = rtt >= 150 || jit >= 25 || member.lossPctCenti >= 200;
  const tone = bad ? "text-destructive" : marginal ? "text-warning" : "text-primary/80";
  return (
    <span
      className={cn("font-mono text-xs tabular", tone)}
      title={`Round-trip ${rtt} ms · jitter ${jit} ms · packet loss ${lossPct.toFixed(2)}%`}
    >
      {rtt}ms · {lossPct.toFixed(1)}%
    </span>
  );
}

function IdentityBadge({ member }: { member: Member }) {
  if (!member.identity) return null;
  const firstSeen =
    member.identityFirstSeenUnix > 0n
      ? new Date(Number(member.identityFirstSeenUnix) * 1000).toLocaleString()
      : "—";
  return (
    <span className="group relative inline-flex">
      <Badge variant="primary" className="cursor-default">
        <Fingerprint className="size-2.5" />
        {member.identity}
      </Badge>
      <div className="absolute left-0 top-full z-50 mt-1 hidden w-80 rounded-md border border-border bg-popover p-3 text-popover-foreground shadow-xl group-hover:block">
        <p className="mb-2 font-mono text-xs font-semibold">{member.identity}</p>
        <dl className="space-y-1.5 text-[10px]">
          <div>
            <dt className="uppercase tracking-wider text-muted-foreground">Public key</dt>
            <dd className="break-all font-mono text-foreground/80">{member.identityPubkey}</dd>
          </div>
          <div>
            <dt className="uppercase tracking-wider text-muted-foreground">Machine</dt>
            <dd className="font-mono text-foreground/80">
              {member.identityMachineHash
                ? `${member.identityMachineHash.slice(0, 16)}…`
                : "not reported"}
            </dd>
          </div>
          <div>
            <dt className="uppercase tracking-wider text-muted-foreground">First seen</dt>
            <dd className="font-mono text-foreground/80">{firstSeen}</dd>
          </div>
        </dl>
      </div>
    </span>
  );
}

function MemberMenu({
  member,
  onRename,
  onBan,
}: {
  member: Member;
  onRename: () => void;
  onBan: () => void;
}) {
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
  async function toggleMute() {
    try {
      await admin.setMute({ id: member.id, muted: !member.muted });
      toast.success(
        member.muted ? `${member.displayName} unmuted` : `${member.displayName} muted`,
      );
    } catch (e) {
      toast.error(`Mute change failed: ${err(e)}`);
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
        <DropdownMenuItem onSelect={() => void toggleMute()}>
          {member.muted ? <Mic /> : <MicOff />} {member.muted ? "Unmute" : "Mute (silence)"}
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem onSelect={() => void kick()} className="text-warning">
          <Power /> Kick (disconnect)
        </DropdownMenuItem>
        <DropdownMenuItem
          onSelect={onBan}
          disabled={!member.identity}
          className="text-destructive"
        >
          <Ban /> {member.identity ? "Ban identity\u2026" : "Ban (no identity)"}
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

/// Confirm-and-reason dialog for banning a member's identity. Only
/// reachable for members with a verified identity (the menu item is
/// disabled otherwise). The machine toggle appears only when the
/// session reported a machine hash.
function BanDialog({ member, onClose }: { member: Member; onClose: () => void }) {
  const [reason, setReason] = useState("");
  const [banMachine, setBanMachine] = useState(false);
  const [busy, setBusy] = useState(false);

  async function ban() {
    setBusy(true);
    try {
      await admin.banClient({ id: member.id, reason, banMachine });
      toast.warning(`Banned ${member.displayName} (${member.identity})`);
      onClose();
    } catch (e) {
      toast.error(`Ban failed: ${err(e)}`);
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            Ban {member.displayName}{" "}
            <span className="font-mono text-sm text-muted-foreground">({member.identity})</span>
          </DialogTitle>
        </DialogHeader>
        <p className="text-sm text-muted-foreground">
          The session is kicked immediately and this identity can no longer register. Display
          names don&apos;t matter — the ban follows the key.
        </p>
        <div className="space-y-1.5">
          <Label htmlFor="ban-reason">Reason (shown to the banned client)</Label>
          <Input
            id="ban-reason"
            value={reason}
            maxLength={256}
            placeholder="optional"
            onChange={(e) => setReason(e.target.value)}
          />
        </div>
        {member.identityMachineHash && (
          <label className="flex items-center justify-between gap-3 text-sm">
            <span>
              Also ban this machine
              <span className="block text-xs text-muted-foreground">
                A wiped config gets a fresh identity but keeps the machine hash — this closes
                that path.
              </span>
            </span>
            <Switch checked={banMachine} onCheckedChange={setBanMachine} />
          </label>
        )}
        <div className="flex justify-end gap-2">
          <DialogClose asChild>
            <Button variant="ghost">Cancel</Button>
          </DialogClose>
          <Button variant="destructive" disabled={busy} onClick={() => void ban()}>
            <Ban /> Ban
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
