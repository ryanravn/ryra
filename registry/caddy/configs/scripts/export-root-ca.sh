#!/bin/bash
CA_CERT="$(dirname "$RYRA_SERVICE_HOME")/caddy-root-ca.crt"
for i in $(seq 1 10); do
  # Export both root and intermediate CA certs — some TLS clients need the
  # full chain in the trust store to verify Caddy's self-signed certificates.
  ROOT=$(podman exec caddy cat /data/caddy/pki/authorities/local/root.crt 2>/dev/null)
  INTERMEDIATE=$(podman exec caddy cat /data/caddy/pki/authorities/local/intermediate.crt 2>/dev/null)
  if [ -n "$ROOT" ]; then
    printf '%s\n' "$ROOT" > "$CA_CERT"
    [ -n "$INTERMEDIATE" ] && printf '%s\n' "$INTERMEDIATE" >> "$CA_CERT"
    exit 0
  fi
  sleep 1
done
echo "Note: CA cert not yet available (will be exported on next caddy restart)"
exit 0
