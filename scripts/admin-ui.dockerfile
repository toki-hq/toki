# Standalone Toki admin UI: a Vite/React SPA served by nginx, which
# reverse-proxies the gRPC-Web Admin service + the /api cookie endpoints
# to the toki-server admin port (${TOKI_SERVER_GRPC_ENDPOINT}). Decoupled
# from the server binary so the UI can be built, deployed, and scaled on
# its own. Build context = repo root (the UI's `buf` codegen reads the
# proto at ../crates/proto/proto, so that path must be present).

# ── Build stage ───────────────────────────────────────────────────
FROM node:24-slim AS build

WORKDIR /app
# Mirror the repo layout so admin-ui/buf.yaml's `../crates/proto/proto`
# resolves during `npm run gen`.
COPY crates/proto/proto ./crates/proto/proto
COPY admin-ui ./admin-ui

WORKDIR /app/admin-ui
RUN npm ci
RUN npm run gen
RUN npm run build

# ── Release stage ─────────────────────────────────────────────────
FROM nginx:alpine AS release

# Built SPA assets.
COPY --from=build /app/admin-ui/dist /usr/share/nginx/html

# nginx config template + entrypoint (envsubst's the backend endpoint in
# at container start).
COPY scripts/admin-ui.nginx.conf.template /etc/nginx/templates/admin-ui.conf.template
COPY scripts/admin-ui.entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

# Plain HTTP on :80 — TLS to the browser is terminated by the external
# proxy (Coolify / Traefik / etc.), same as the rest of the stack.
EXPOSE 80

ENTRYPOINT ["/entrypoint.sh"]
