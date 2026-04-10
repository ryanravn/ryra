#!/bin/bash
[ -z "${OAUTH_CLIENT_ID:-}" ] && exit 0

IMMICH_URL="http://127.0.0.1:$RYRA_PORT_HTTP"
ADMIN_EMAIL="$INIT_IMMICH_ADMIN_EMAIL"
ADMIN_PASSWORD="$INIT_IMMICH_ADMIN_PASSWORD"

cat > /tmp/immich-oidc-setup.py << 'PYEOF'
import sys, json, urllib.request

immich_url = sys.argv[1]
admin_email = sys.argv[2]
admin_password = sys.argv[3]
client_id = sys.argv[4]
client_secret = sys.argv[5]
issuer_url = sys.argv[6]

def api(method, path, data=None, token=None):
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    body = json.dumps(data).encode() if data else None
    req = urllib.request.Request(f"{immich_url}/api{path}", data=body, headers=headers, method=method)
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.loads(resp.read())

resp = api("POST", "/auth/login", {"email": admin_email, "password": admin_password})
token = resp["accessToken"]

config = api("GET", "/system-config", token=token)
config["oauth"]["enabled"] = True
config["oauth"]["autoRegister"] = True
config["oauth"]["autoLaunch"] = False
config["oauth"]["clientId"] = client_id
config["oauth"]["clientSecret"] = client_secret
config["oauth"]["issuerUrl"] = issuer_url
config["oauth"]["scope"] = "openid email profile"
config["oauth"]["buttonText"] = "Login with SSO"
api("PUT", "/system-config", data=config, token=token)
print("Immich OIDC configured successfully")
PYEOF

python3 /tmp/immich-oidc-setup.py \
  "$IMMICH_URL" "$ADMIN_EMAIL" "$ADMIN_PASSWORD" \
  "$OAUTH_CLIENT_ID" "$OAUTH_CLIENT_SECRET" "$OAUTH_ISSUER_URL"
