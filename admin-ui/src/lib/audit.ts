// Presentation helpers for audit-log entries: map the server's `kind`
// vocabulary to an icon glyph + a Tailwind text-color class. Kept in one
// place so the Overview "recent activity" list and the full Audit table
// render events identically.

const META: Record<string, { icon: string; tone: string }> = {
  // Admin / operator actions
  kick: { icon: "↯", tone: "text-warning" },
  move: { icon: "⇄", tone: "text-primary" },
  rename: { icon: "✎", tone: "text-primary" },
  priority: { icon: "★", tone: "text-primary" },
  "channel-name": { icon: "✎", tone: "text-primary" },
  "channel-clear": { icon: "⊘", tone: "text-primary" },
  "server-config": { icon: "⚙", tone: "text-primary" },
  "admin-password": { icon: "⚙", tone: "text-primary" },
  // Connections
  connect: { icon: "+", tone: "text-primary" },
  disconnect: { icon: "−", tone: "text-muted-foreground" },
  // Security
  "auth-ok": { icon: "✓", tone: "text-primary" },
  "auth-fail": { icon: "⚠", tone: "text-warning" },
};

export function auditIcon(kind: string): string {
  return META[kind]?.icon ?? "·";
}

export function auditTone(kind: string): string {
  return META[kind]?.tone ?? "text-muted-foreground";
}
