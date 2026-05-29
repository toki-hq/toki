import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";

export function Audit() {
  return (
    <div className="flex flex-col gap-6">
      <h1 className="font-mono text-xs uppercase tracking-widest text-muted-foreground">
        04 · Audit
      </h1>
      <Card>
        <CardHeader>
          <CardTitle>Coming soon</CardTitle>
        </CardHeader>
        <CardContent>
          <p className="text-sm text-muted-foreground">
            A persistent admin action log isn't implemented server-side yet. For now, every
            operator action is recorded via structured <code className="font-mono">tracing</code>{" "}
            logs on the server (grep for <code className="font-mono">admin_user</code>).
          </p>
        </CardContent>
      </Card>
    </div>
  );
}
