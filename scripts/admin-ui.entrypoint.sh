#!/bin/sh
set -e

# The UI talks to the toki-server admin port through this reverse proxy.
# Default to the conventional docker-compose service name so a bare
# `docker run` against the bundled stack works out of the box.
: "${TOKI_SERVER_GRPC_ENDPOINT:=https://toki-server:8000}"
export TOKI_SERVER_GRPC_ENDPOINT

# Render the nginx config, substituting ONLY our variable so nginx's own
# `$host`, `$uri`, `$remote_addr`, … survive (envsubst would otherwise
# blank them out).
envsubst '${TOKI_SERVER_GRPC_ENDPOINT}' \
    < /etc/nginx/templates/admin-ui.conf.template \
    > /etc/nginx/conf.d/default.conf

echo "toki-admin-ui → proxying /api + /toki.admin.v1.Admin to ${TOKI_SERVER_GRPC_ENDPOINT}"

exec nginx -g 'daemon off;'
