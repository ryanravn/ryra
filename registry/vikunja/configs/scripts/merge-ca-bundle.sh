#!/bin/bash
set -euo pipefail
SERVICE_HOME="$HOME/.local/share/ryra/vikunja"
CADDY_CA="$HOME/.local/share/ryra/caddy-root-ca.crt"
MERGED="$SERVICE_HOME/ca-bundle.crt"
HOSTS="$SERVICE_HOME/hosts"

# Ensure placeholder files exist for Volume= bind mounts.
# If podman previously created these as directories (source didn't exist),
# remove them first.
[ -d "$MERGED" ] && rm -rf "$MERGED"
[ -d "$HOSTS" ] && rm -rf "$HOSTS"
[ -f "$MERGED" ] || touch "$MERGED"
[ -f "$HOSTS" ] || printf "127.0.0.1 localhost\n::1 localhost\n" > "$HOSTS"

# --- CA bundle: trust caddy's self-signed CA ---
# Only needed when --auth is used
if [ -f "$CADDY_CA" ]; then
  if [ ! -s "$MERGED" ]; then
    # Extract the system CA bundle from the vikunja image
    podman run --rm --entrypoint sh vikunja/vikunja:2.3.0 \
      -c "cat /etc/ssl/certs/ca-certificates.crt" > "$MERGED" 2>/dev/null || true
  fi
  # Append Caddy's root CA if not already present
  if ! grep -q "ryra-caddy-ca" "$MERGED" 2>/dev/null; then
    echo "# ryra-caddy-ca" >> "$MERGED"
    cat "$CADDY_CA" >> "$MERGED"
  fi
fi

# --- /etc/hosts: resolve .localhost auth domains to caddy ---
# .localhost always resolves to 127.0.0.1 (RFC 6761/glibc), which is the
# container's loopback, not caddy. Create a custom hosts file with caddy's
# IP for the auth domain.
if podman inspect caddy >/dev/null 2>&1; then
  CADDY_IP=$(podman inspect caddy --format '{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}' 2>/dev/null | awk '{print $1}')
  if [ -n "$CADDY_IP" ]; then
    printf "127.0.0.1 localhost\n::1 localhost\n" > "$HOSTS"
    # Add all .localhost domains that point to caddy
    for env_file in "$HOME"/.local/share/ryra/*/.env; do
      [ -f "$env_file" ] || continue
      grep -oP '(?<=://)[^:/]+\.localhost' "$env_file" 2>/dev/null | sort -u | while read -r domain; do
        echo "$CADDY_IP $domain" >> "$HOSTS"
      done
    done
  fi
fi
