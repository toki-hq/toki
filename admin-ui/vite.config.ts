import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import basicSsl from "@vitejs/plugin-basic-ssl";
import path from "node:path";

// Dev runs over HTTPS (basic-ssl self-signed) so the `Secure` admin
// session cookie is actually stored by the browser, and proxies the
// gRPC-Web + cookie endpoints to the toki-server admin port. Same-origin
// from the browser's POV, so SameSite=Strict cookies round-trip.
//
// The upstream is `TOKI_SERVER_GRPC_ENDPOINT` (the admin service — it
// co-serves the gRPC-Web `Admin` RPCs and the `/api/*` cookie endpoints),
// falling back to a local server. `secure: false` tolerates the server's
// self-signed cert; `changeOrigin: true` rewrites the upstream Host/SNI so
// a remote or containerised target (e.g. `https://toki-server:8000`) is
// reached correctly. This mirrors the production nginx reverse-proxy
// (scripts/admin-ui.nginx.conf.template) so dev and prod behave alike.
const grpcEndpoint =
  process.env.TOKI_SERVER_GRPC_ENDPOINT ?? "https://localhost:8000";

const proxyOpts = {
  target: grpcEndpoint,
  secure: false,
  changeOrigin: true,
};

export default defineConfig({
  plugins: [react(), basicSsl()],
  resolve: {
    alias: { "@": path.resolve(__dirname, "src") },
  },
  server: {
    port: 5173,
    proxy: {
      "/toki.admin.v1.Admin": proxyOpts,
      "/api": proxyOpts,
    },
  },
  build: {
    // Served by the standalone UI image (nginx); keep it lean + predictable.
    outDir: "dist",
    emptyOutDir: true,
  },
});
