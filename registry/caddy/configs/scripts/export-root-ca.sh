#!/bin/bash
CA_CERT="$(dirname "$RYRA_SERVICE_HOME")/caddy-root-ca.crt"
for i in $(seq 1 10); do
  podman exec caddy cat /data/caddy/pki/authorities/local/root.crt > "$CA_CERT" 2>/dev/null && [ -s "$CA_CERT" ] && exit 0
  sleep 1
done
echo "Note: CA cert not yet available (will be exported on next caddy restart)"
exit 0
