import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import basicSsl from "@vitejs/plugin-basic-ssl";
import path from "node:path";

// Dev runs over HTTPS (basic-ssl self-signed) so the `Secure` admin
// session cookie is actually stored by the browser, and proxies the
// gRPC-Web + cookie endpoints to the running toki-server (also HTTPS,
// self-signed → `secure: false`). Same-origin from the browser's POV, so
// SameSite=Strict cookies round-trip.
export default defineConfig({
  plugins: [react(), basicSsl()],
  resolve: {
    alias: { "@": path.resolve(__dirname, "src") },
  },
  server: {
    port: 5173,
    proxy: {
      "/toki.admin.v1.Admin": {
        target: "https://localhost:8000",
        secure: false,
        changeOrigin: false,
      },
      "/api": {
        target: "https://localhost:8000",
        secure: false,
        changeOrigin: false,
      },
    },
  },
  build: {
    // Embedded by the server via rust-embed; keep it lean and predictable.
    outDir: "dist",
    emptyOutDir: true,
  },
});
