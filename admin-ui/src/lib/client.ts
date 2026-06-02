import { createClient, type Client } from "@connectrpc/connect";
import { createGrpcWebTransport } from "@connectrpc/connect-web";
import { Admin } from "@/gen/admin_pb";

// Same-origin gRPC-Web transport. In production the SPA is served by
// toki-server, so `baseUrl: "/"` hits the same listener; the browser
// auto-attaches the HttpOnly session cookie. In dev, Vite proxies
// `/toki.admin.v1.Admin/*` to the running server. `credentials:
// "same-origin"` is the default for fetch — cookies ride along.
const transport = createGrpcWebTransport({
  baseUrl: window.location.origin,
});

export const admin: Client<typeof Admin> = createClient(Admin, transport);
