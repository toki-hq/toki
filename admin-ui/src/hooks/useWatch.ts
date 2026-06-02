import { useEffect, useRef, useState } from "react";
import { Code, ConnectError } from "@connectrpc/connect";
import { admin } from "@/lib/client";
import type { Snapshot } from "@/gen/admin_pb";

interface WatchState {
  snapshot: Snapshot | null;
  connected: boolean;
}

/**
 * Subscribe to the server-streaming `Watch` RPC and expose the latest
 * snapshot. Re-opens on transient stream errors; calls `onUnauth` if the
 * session is rejected (e.g. after logout / expiry) so the app can bounce
 * to the login screen.
 */
export function useWatch(onUnauth: () => void): WatchState {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [connected, setConnected] = useState(false);
  const onUnauthRef = useRef(onUnauth);
  onUnauthRef.current = onUnauth;

  useEffect(() => {
    const abort = new AbortController();
    let stopped = false;

    async function run() {
      while (!stopped) {
        try {
          setConnected(true);
          for await (const snap of admin.watch({}, { signal: abort.signal })) {
            setSnapshot(snap);
          }
        } catch (err) {
          if (stopped || abort.signal.aborted) return;
          setConnected(false);
          if (err instanceof ConnectError && err.code === Code.Unauthenticated) {
            onUnauthRef.current();
            return;
          }
          // Transient: back off briefly and reconnect.
          await new Promise((r) => setTimeout(r, 1500));
        }
      }
    }
    void run();
    return () => {
      stopped = true;
      abort.abort();
    };
  }, []);

  return { snapshot, connected };
}
