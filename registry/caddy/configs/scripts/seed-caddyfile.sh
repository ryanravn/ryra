#!/bin/bash
set -euo pipefail
CADDY_CONFIG="$SERVICE_HOME/config"
mkdir -p "$CADDY_CONFIG"
CADDYFILE="$CADDY_CONFIG/Caddyfile"
TLS_SNIPPET="$CADDY_CONFIG/tls.caddy"

# `PORT_*` come from caddy's .env (loaded as the quadlet's
# EnvironmentFile). They reflect the resolved host ports — and since the
# quadlet's PublishPort uses `host:host` mapping, those are also the
# ports Caddy itself binds to inside the container. Defaults match
# service.toml's high-port fallbacks so this script still works if the
# env var is somehow unset.
PORT_HTTP="${SERVICE_PORT_HTTP:-8080}"
PORT_HTTPS="${SERVICE_PORT_HTTPS:-8443}"

# Defensive fallback only — `ryra add caddy` writes tls.caddy with the
# right contents (LAN default or --acme email) before starting Caddy.
# This branch fires if the file got removed manually; without it Caddy's
# `import tls.caddy` would fail and the container wouldn't start.
if [ ! -f "$TLS_SNIPPET" ]; then
	cat > "$TLS_SNIPPET" <<'EOF'
(services_tls) {
	tls internal
}
EOF
fi

[ -f "$CADDYFILE" ] && exit 0
# No HTTPS catchall block — Caddy's auto-HTTPS doesn't recognize
# non-standard ports like 8080 the way it does port 80, so pairing a
# bare-port `:8080 { respond 404 }` with `:8443 { tls internal; respond 404 }`
# trips the parser ("automation policy from site block is also default
# /catch-all policy ... in conflict"). Per-service blocks below create
# their own HTTPS listener; the seed only needs the HTTP catchall.
cat > "$CADDYFILE" <<EOF
import tls.caddy

:${SERVICE_PORT_HTTP} {
	respond 404
}
EOF
