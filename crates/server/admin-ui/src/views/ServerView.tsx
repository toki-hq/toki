import { useEffect, useState } from "react";
import { toast } from "sonner";
import { ConnectError } from "@connectrpc/connect";
import type { ServerInfo, ServerConfig } from "@/gen/admin_pb";
import { admin } from "@/lib/client";
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input, Label } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";

function err(e: unknown): string {
  return e instanceof ConnectError ? e.rawMessage : e instanceof Error ? e.message : String(e);
}

export function ServerView({ info }: { info: ServerInfo | null }) {
  return (
    <div className="flex flex-col gap-6">
      <h1 className="font-mono text-xs uppercase tracking-widest text-muted-foreground">
        03 · Server
      </h1>
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        <Bootstrap info={info} />
        <RuntimeConfig />
        <ServerPassword tomlOverride={info?.tomlPasswordOverride ?? false} />
        <ChangePassword />
      </div>
    </div>
  );
}

function Bootstrap({ info }: { info: ServerInfo | null }) {
  return (
    <Card>
      <CardHeader>
        <CardTitle>Bootstrap</CardTitle>
        <CardDescription>Set at startup (read-only).</CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-2 font-mono text-sm">
        <Row k="Version" v={info ? `toki-server v${info.version}` : "—"} />
        <Row k="Admin bind" v={info?.adminBind ?? "—"} />
        <Row
          k="Started"
          v={
            info
              ? new Date(Number(info.startedAtUnix) * 1000).toISOString().slice(0, 10)
              : "—"
          }
        />
      </CardContent>
    </Card>
  );
}

function Row({ k, v }: { k: string; v: string }) {
  return (
    <div className="flex justify-between">
      <span className="text-muted-foreground">{k}</span>
      <span className="tabular">{v}</span>
    </div>
  );
}

function RuntimeConfig() {
  const [cfg, setCfg] = useState<ServerConfig | null>(null);
  const [name, setName] = useState("");
  const [maxPeers, setMaxPeers] = useState("");
  const [idleKick, setIdleKick] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    admin
      .getServerConfig({})
      .then((c) => {
        setCfg(c);
        setName(c.serverName);
        setMaxPeers(String(c.maxPeers));
        setIdleKick(String(c.idleKickSecs));
      })
      .catch((e) => toast.error(`Load config failed: ${err(e)}`));
  }, []);

  const dirty =
    cfg !== null &&
    (name !== cfg.serverName ||
      maxPeers !== String(cfg.maxPeers) ||
      idleKick !== String(cfg.idleKickSecs));

  async function save() {
    setBusy(true);
    try {
      const updated = await admin.updateServerConfig({
        serverName: name,
        maxPeers: Number(maxPeers),
        idleKickSecs: Number(idleKick),
      });
      setCfg(updated);
      toast.success("Server config saved");
    } catch (e) {
      toast.error(`Save failed: ${err(e)}`);
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Runtime</CardTitle>
        <CardDescription>Hot-applied; persisted to admin.db.</CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-3">
        <Field label="Server name">
          <Input value={name} maxLength={64} onChange={(e) => setName(e.target.value)} />
        </Field>
        <Field label="Max peers">
          <Input
            type="number"
            min={1}
            max={100000}
            value={maxPeers}
            onChange={(e) => setMaxPeers(e.target.value)}
            className="font-mono tabular"
          />
        </Field>
        <Field label="Idle kick (sec)">
          <Input
            type="number"
            min={5}
            max={86400}
            value={idleKick}
            onChange={(e) => setIdleKick(e.target.value)}
            className="font-mono tabular"
          />
        </Field>
        <Button className="mt-1 self-start" disabled={!dirty || busy} onClick={() => void save()}>
          {busy ? "Saving…" : "Save"}
        </Button>
      </CardContent>
    </Card>
  );
}

function ServerPassword({ tomlOverride }: { tomlOverride: boolean }) {
  const [pw, setPw] = useState("");
  const [reveal, setReveal] = useState(false);
  const [armed, setArmed] = useState<boolean | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    admin
      .getServerConfig({})
      .then((c) => setArmed(c.grpcPasswordSet))
      .catch(() => setArmed(null));
  }, []);

  async function save() {
    setBusy(true);
    try {
      await admin.setServerPassword({ password: pw });
      setArmed(pw.trim().length > 0);
      setPw("");
      toast.success(pw.trim() ? "Server password armed" : "Server password disarmed (open mode)");
    } catch (e) {
      toast.error(`Failed: ${err(e)}`);
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Server password</CardTitle>
        <CardDescription>The shared secret Toki clients must supply to connect.</CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-3">
        {tomlOverride ? (
          <p className="text-sm text-muted-foreground">
            Managed by <code className="font-mono">config.toml</code>. Remove the{" "}
            <code className="font-mono">password = …</code> line and restart to manage it here.
          </p>
        ) : (
          <>
            <Field label="New password (empty = open mode)">
              <div className="flex gap-2">
                <Input
                  type={reveal ? "text" : "password"}
                  value={pw}
                  onChange={(e) => setPw(e.target.value)}
                  className="font-mono"
                />
                <Button variant="outline" size="sm" onClick={() => setReveal((r) => !r)}>
                  {reveal ? "hide" : "show"}
                </Button>
              </div>
            </Field>
            <div className="flex items-center justify-between">
              <span className="text-xs text-muted-foreground">
                Status:{" "}
                {armed === null ? (
                  "—"
                ) : armed ? (
                  <Badge variant="primary">ARMED</Badge>
                ) : (
                  <Badge variant="muted">OPEN MODE</Badge>
                )}
              </span>
              <Button disabled={busy} onClick={() => void save()}>
                {busy ? "Saving…" : "Apply"}
              </Button>
            </div>
          </>
        )}
      </CardContent>
    </Card>
  );
}

function ChangePassword() {
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [confirm, setConfirm] = useState("");
  const [busy, setBusy] = useState(false);

  async function save() {
    if (next.length < 8) return toast.error("New password must be ≥ 8 characters");
    if (next !== confirm) return toast.error("New passwords don't match");
    if (next === current) return toast.error("New password must differ from current");
    setBusy(true);
    try {
      await admin.changePassword({ current, newPassword: next });
      setCurrent("");
      setNext("");
      setConfirm("");
      toast.success("Admin password changed");
    } catch (e) {
      toast.error(`Failed: ${err(e)}`);
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Admin password</CardTitle>
        <CardDescription>Changing it signs out your other sessions.</CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-3">
        <Field label="Current">
          <Input type="password" value={current} onChange={(e) => setCurrent(e.target.value)} />
        </Field>
        <Field label="New">
          <Input type="password" value={next} onChange={(e) => setNext(e.target.value)} />
        </Field>
        <Field label="Confirm new">
          <Input type="password" value={confirm} onChange={(e) => setConfirm(e.target.value)} />
        </Field>
        <Button
          className="mt-1 self-start"
          disabled={busy || !current || !next}
          onClick={() => void save()}
        >
          {busy ? "Saving…" : "Change password"}
        </Button>
      </CardContent>
    </Card>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-1.5">
      <Label>{label}</Label>
      {children}
    </div>
  );
}
