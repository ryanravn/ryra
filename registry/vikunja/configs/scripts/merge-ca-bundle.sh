#!/bin/bash
set -euo pipefail
SERVICE_HOME="$HOME/.local/share/ryra/vikunja"
CADDY_CA="$HOME/.local/share/ryra/caddy-root-ca.crt"
MERGED="$SERVICE_HOME/ca-bundle.crt"
HOSTS="$SERVICE_HOME/hosts"

# Ensure placeholder files exist for Volume= bind mounts.
[ -d "$MERGED" ] && rm -rf "$MERGED"
[ -d "$HOSTS" ] && rm -rf "$HOSTS"
[ -f "$HOSTS" ] || printf "127.0.0.1 localhost\n::1 localhost\n" > "$HOSTS"

# --- CA bundle: trust caddy's self-signed CA ---
if [ -f "$CADDY_CA" ]; then
  # Use the host's system CA bundle as base (works for Debian/Fedora)
  if [ ! -s "$MERGED" ]; then
    for sys_ca in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt; do
      [ -f "$sys_ca" ] && cp "$sys_ca" "$MERGED" && break
    done
    [ -s "$MERGED" ] || touch "$MERGED"
  fi
  # Append Caddy's root CA if not already present
  if ! grep -q "ryra-caddy-ca" "$MERGED" 2>/dev/null; then
    echo "# ryra-caddy-ca" >> "$MERGED"
    cat "$CADDY_CA" >> "$MERGED"
  fi
else
  [ -f "$MERGED" ] || touch "$MERGED"
fi

# --- /etc/hosts: resolve .localhost auth domains to caddy ---
if podman inspect caddy >/dev/null 2>&1; then
  CADDY_IP=$(podman inspect caddy --format '{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}' 2>/dev/null | awk '{print $1}')
  if [ -n "$CADDY_IP" ]; then
    printf "127.0.0.1 localhost\n::1 localhost\n" > "$HOSTS"
    for env_file in "$HOME"/.local/share/ryra/*/.env; do
      [ -f "$env_file" ] || continue
      grep -oP '(?<=://)[^:/]+\.localhost' "$env_file" 2>/dev/null | sort -u | while read -r domain; do
        echo "$CADDY_IP $domain" >> "$HOSTS"
      done
    done
  fi
fi
