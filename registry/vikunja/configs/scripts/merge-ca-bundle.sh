#!/bin/bash
# Prepare CA bundle and /etc/hosts for vikunja container.
# Runs as ExecStartPre — must not fail or the service won't start.
# All operations are best-effort with explicit error handling.
SERVICE_HOME="${RYRA_SERVICE_HOME:-$HOME/.local/share/ryra/vikunja}"
CADDY_CA="$HOME/.local/share/ryra/caddy-root-ca.crt"
MERGED="$SERVICE_HOME/ca-bundle.crt"
HOSTS="$SERVICE_HOME/hosts"

# Fix stale directories from failed Volume= mounts
[ -d "$MERGED" ] && rm -rf "$MERGED"
[ -d "$HOSTS" ] && rm -rf "$HOSTS"

# Create default hosts file
printf "127.0.0.1 localhost\n::1 localhost\n" > "$HOSTS"

# Create default empty CA bundle
[ -f "$MERGED" ] || touch "$MERGED"

# If caddy CA exists, build a proper CA bundle
if [ -f "$CADDY_CA" ]; then
  # Copy system CAs as base
  for f in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt; do
    if [ -f "$f" ]; then cp "$f" "$MERGED"; break; fi
  done
  # Append caddy CA
  cat "$CADDY_CA" >> "$MERGED" 2>/dev/null || true
fi

# If caddy container is running, add auth domain to hosts
CADDY_IP=$(podman inspect caddy --format '{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}' 2>/dev/null | awk '{print $1}')
if [ -n "$CADDY_IP" ]; then
  # Find .localhost domains in service .env files and map them to caddy
  for f in "$HOME"/.local/share/ryra/*/.env; do
    [ -f "$f" ] || continue
    sed -n 's|.*://\([^:/]*\.localhost\).*|\1|p' "$f" 2>/dev/null
  done | sort -u | while read -r domain; do
    [ -n "$domain" ] && echo "$CADDY_IP $domain" >> "$HOSTS"
  done
fi

exit 0
