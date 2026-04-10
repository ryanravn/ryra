#!/bin/bash
set -euo pipefail
CADDY_CONFIG="$RYRA_SERVICE_HOME/config"
mkdir -p "$CADDY_CONFIG"
CADDYFILE="$CADDY_CONFIG/Caddyfile"
[ -f "$CADDYFILE" ] && exit 0
cat > "$CADDYFILE" <<'EOF'
:80 {
	respond 404
}

:8443 {
	tls internal
	respond 404
}
EOF
