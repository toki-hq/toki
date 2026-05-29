import { useEffect, useState } from "react";
import { Code, ConnectError } from "@connectrpc/connect";
import {
  Radio,
  LayoutDashboard,
  RadioTower,
  Server as ServerIcon,
  ScrollText,
  LogOut,
  Loader2,
} from "lucide-react";
import { admin } from "@/lib/client";
import { logout } from "@/lib/auth";
import type { ServerInfo } from "@/gen/admin_pb";
import { useWatch } from "@/hooks/useWatch";
import { useTheme } from "@/components/ThemeProvider";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { cn, formatUptime } from "@/lib/utils";
import { Login } from "@/views/Login";
import { Overview } from "@/views/Overview";
import { Rooms } from "@/views/Rooms";
import { ServerView } from "@/views/ServerView";
import { Audit } from "@/views/Audit";

type Section = "overview" | "rooms" | "server" | "audit";

const NAV: { id: Section; label: string; icon: typeof Radio }[] = [
  { id: "overview", label: "Overview", icon: LayoutDashboard },
  { id: "rooms", label: "Channels", icon: RadioTower },
  { id: "server", label: "Server", icon: ServerIcon },
  { id: "audit", label: "Audit", icon: ScrollText },
];

export function App() {
  const [authed, setAuthed] = useState<boolean | null>(null);
  const [info, setInfo] = useState<ServerInfo | null>(null);

  async function check() {
    try {
      const i = await admin.getServerInfo({});
      setInfo(i);
      setAuthed(true);
    } catch (err) {
      if (err instanceof ConnectError && err.code === Code.Unauthenticated) {
        setAuthed(false);
      } else {
        // Network/other error — still show login so the operator can retry.
        setAuthed(false);
      }
    }
  }

  useEffect(() => {
    void check();
  }, []);

  if (authed === null) {
    return (
      <div className="flex min-h-screen items-center justify-center bg-background">
        <Loader2 className="size-6 animate-spin text-muted-foreground" />
      </div>
    );
  }
  if (!authed) return <Login onSuccess={() => void check()} />;
  return <Shell info={info} onLoggedOut={() => setAuthed(false)} />;
}

function Shell({ info, onLoggedOut }: { info: ServerInfo | null; onLoggedOut: () => void }) {
  const [section, setSection] = useState<Section>("overview");
  const { snapshot, connected } = useWatch(onLoggedOut);
  const { theme, setTheme } = useTheme();

  const peers =
    (snapshot?.rooms.reduce((n, r) => n + r.members.length, 0) ?? 0) +
    (snapshot?.lobby.length ?? 0);
  const transmitting = snapshot?.rooms.filter((r) => r.holder).length ?? 0;

  async function doLogout() {
    await logout();
    onLoggedOut();
  }

  return (
    <div className="flex min-h-screen flex-col bg-background text-foreground">
      {/* Topbar */}
      <header className="flex h-14 items-center gap-6 border-b border-border bg-card/60 px-5 backdrop-blur">
        <div className="flex items-center gap-2">
          <Radio className="size-5 text-primary" />
          <span className="font-mono text-sm font-bold tracking-tight">
            <span className="text-primary">TOKI</span>
            <span className="text-muted-foreground"> · ADMIN</span>
          </span>
          {info && (
            <span className="ml-1 font-mono text-[10px] text-muted-foreground">
              v{info.version}
            </span>
          )}
        </div>
        <div className="flex items-center gap-5 font-mono text-xs text-muted-foreground tabular">
          <Stat label="UPTIME" value={formatUptime(Number(snapshot?.serverUptimeSecs ?? 0n))} />
          <Stat label="PEERS" value={String(peers)} />
          <Stat
            label="TX"
            value={transmitting > 0 ? String(transmitting) : "—"}
            accent={transmitting > 0 ? "warning" : undefined}
          />
        </div>
        <div className="ml-auto flex items-center gap-3">
          <span
            className={cn(
              "rounded px-2 py-0.5 font-mono text-[10px] uppercase tracking-wider",
              connected
                ? "bg-primary/15 text-primary"
                : "animate-pulse-glow bg-warning/15 text-warning",
            )}
          >
            {connected ? "Connected" : "Reconnecting"}
          </span>
          <Button variant="ghost" size="sm" onClick={doLogout}>
            <LogOut /> Logout
          </Button>
        </div>
      </header>

      <div className="flex flex-1">
        {/* Sidebar */}
        <nav className="flex w-52 flex-col gap-1 border-r border-border bg-card/30 p-3">
          {NAV.map(({ id, label, icon: Icon }) => (
            <button
              key={id}
              onClick={() => setSection(id)}
              className={cn(
                "flex items-center gap-2.5 rounded-md px-3 py-2 text-sm transition-colors",
                section === id
                  ? "bg-primary/15 text-primary"
                  : "text-muted-foreground hover:bg-accent hover:text-foreground",
              )}
            >
              <Icon className="size-4" />
              {label}
            </button>
          ))}
          <div className="mt-auto flex items-center justify-between rounded-md px-3 py-2">
            <span className="text-xs text-muted-foreground">Phosphor theme</span>
            <Switch
              checked={theme === "phosphor"}
              onCheckedChange={(on) => setTheme(on ? "phosphor" : "dashboard")}
              aria-label="Toggle phosphor theme"
            />
          </div>
        </nav>

        {/* Content */}
        <main className="flex-1 overflow-auto p-6">
          {section === "overview" && <Overview snapshot={snapshot} info={info} />}
          {section === "rooms" && <Rooms snapshot={snapshot} />}
          {section === "server" && <ServerView info={info} />}
          {section === "audit" && <Audit />}
        </main>
      </div>
    </div>
  );
}

function Stat({
  label,
  value,
  accent,
}: {
  label: string;
  value: string;
  accent?: "warning";
}) {
  return (
    <span className="flex items-baseline gap-1.5">
      <span className="text-[10px] uppercase tracking-wider text-muted-foreground/70">
        {label}
      </span>
      <span className={cn("text-foreground", accent === "warning" && "text-warning")}>
        {value}
      </span>
    </span>
  );
}
