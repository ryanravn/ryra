#!/bin/bash
set -euo pipefail

# Skip if no OIDC credentials or no enterprise key
[ -z "${OIDC_CLIENT_ID:-}" ] && exit 0
[ -z "${ENTERPRISE_KEY:-}" ] && { echo "No ENTERPRISE_KEY set — SSO registration skipped"; exit 0; }

TWENTY_URL="http://127.0.0.1:${RYRA_PORT_HTTP}"
GRAPHQL_URL="${TWENTY_URL}/metadata"
ADMIN_EMAIL="${RYRA_ADMIN_EMAIL:-admin@example.com}"
ADMIN_PASSWORD="${RYRA_ADMIN_PASSWORD}"

# Helper: extract a value from JSON using a dotted path (e.g. "data.signIn.tokens")
jq_path() {
  python3 -c "
import sys, json, functools
try:
    d = json.load(sys.stdin)
    print(functools.reduce(lambda o, k: o[int(k)] if k.isdigit() else o[k], '$1'.split('.'), d))
except Exception:
    pass
" 2>/dev/null
}

echo "Waiting for Twenty to be ready..."
for i in $(seq 1 120); do
  curl -sf "${TWENTY_URL}/healthz" >/dev/null 2>&1 && break
  sleep 5
done

# Step 1: Get a workspace-scoped access token.
# Try signIn first (account already exists), fall back to signUp + workspace creation.
echo "Authenticating with Twenty..."
LOGIN_TOKEN=""

# Try signIn — returns a loginToken if the user has a workspace
SIGNIN_RESULT=$(curl -sf -X POST "${GRAPHQL_URL}" \
  -H "Content-Type: application/json" \
  -d "{
    \"query\": \"mutation { signIn(email: \\\"${ADMIN_EMAIL}\\\", password: \\\"${ADMIN_PASSWORD}\\\") { tokens { accessOrWorkspaceAgnosticToken { token } } availableWorkspaces { availableWorkspacesForSignIn { loginToken } } } }\"
  }" 2>/dev/null || true)

LOGIN_TOKEN=$(echo "$SIGNIN_RESULT" | jq_path "data.signIn.availableWorkspaces.availableWorkspacesForSignIn.0.loginToken")
AGNOSTIC_TOKEN=$(echo "$SIGNIN_RESULT" | jq_path "data.signIn.tokens.accessOrWorkspaceAgnosticToken.token")

if [ -z "$LOGIN_TOKEN" ] && [ -n "$AGNOSTIC_TOKEN" ]; then
  # User exists but has no workspace — create one
  echo "Creating workspace..."
  WS_RESULT=$(curl -sf -X POST "${GRAPHQL_URL}" \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer ${AGNOSTIC_TOKEN}" \
    -d '{"query": "mutation { signUpInNewWorkspace { loginToken { token } } }"}' 2>/dev/null || true)
  LOGIN_TOKEN=$(echo "$WS_RESULT" | jq_path "data.signUpInNewWorkspace.loginToken.token")
fi

if [ -z "$LOGIN_TOKEN" ]; then
  # User doesn't exist — sign up
  echo "Creating admin account..."
  SIGNUP_RESULT=$(curl -sf -X POST "${GRAPHQL_URL}" \
    -H "Content-Type: application/json" \
    -d "{
      \"query\": \"mutation { signUp(email: \\\"${ADMIN_EMAIL}\\\", password: \\\"${ADMIN_PASSWORD}\\\") { tokens { accessOrWorkspaceAgnosticToken { token } } availableWorkspaces { availableWorkspacesForSignIn { loginToken } } } }\"
    }" 2>/dev/null || true)

  LOGIN_TOKEN=$(echo "$SIGNUP_RESULT" | jq_path "data.signUp.availableWorkspaces.availableWorkspacesForSignIn.0.loginToken")
  AGNOSTIC_TOKEN=$(echo "$SIGNUP_RESULT" | jq_path "data.signUp.tokens.accessOrWorkspaceAgnosticToken.token")

  if [ -z "$LOGIN_TOKEN" ] && [ -n "$AGNOSTIC_TOKEN" ]; then
    echo "Creating workspace..."
    WS_RESULT=$(curl -sf -X POST "${GRAPHQL_URL}" \
      -H "Content-Type: application/json" \
      -H "Authorization: Bearer ${AGNOSTIC_TOKEN}" \
      -d '{"query": "mutation { signUpInNewWorkspace { loginToken { token } } }"}' 2>/dev/null || true)
    LOGIN_TOKEN=$(echo "$WS_RESULT" | jq_path "data.signUpInNewWorkspace.loginToken.token")
  fi
fi

if [ -z "$LOGIN_TOKEN" ]; then
  # Exit 0 intentionally: ExecStartPost failure would cause systemd to
  # restart-loop the service. SSO registration can be retried manually.
  echo "Warning: Could not authenticate with Twenty — SSO registration skipped"
  exit 0
fi

# Step 2: Exchange login token for workspace-scoped access token
echo "Getting workspace access token..."
ACCESS_TOKEN=$(curl -sf -X POST "${GRAPHQL_URL}" \
  -H "Content-Type: application/json" \
  -d "{
    \"query\": \"mutation { getAuthTokensFromLoginToken(loginToken: \\\"${LOGIN_TOKEN}\\\", origin: \\\"${TWENTY_URL}\\\") { tokens { accessOrWorkspaceAgnosticToken { token } } } }\"
  }" | jq_path "data.getAuthTokensFromLoginToken.tokens.accessOrWorkspaceAgnosticToken.token")

if [ -z "$ACCESS_TOKEN" ]; then
  echo "Warning: Could not get workspace access token — SSO registration skipped"
  exit 0
fi

# Step 3: Register OIDC identity provider
echo "Registering Authelia as OIDC identity provider..."
REGISTER_RESULT=$(curl -sf -X POST "${GRAPHQL_URL}" \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer ${ACCESS_TOKEN}" \
  -d "{
    \"query\": \"mutation { createOIDCIdentityProvider(input: { name: \\\"Authelia\\\", issuer: \\\"${OIDC_ISSUER_URL}\\\", clientID: \\\"${OIDC_CLIENT_ID}\\\", clientSecret: \\\"${OIDC_CLIENT_SECRET}\\\" }) { id type issuer name status } }\"
  }" 2>/dev/null || true)

if echo "$REGISTER_RESULT" | grep -q '"id"'; then
  echo "OIDC provider registered successfully"
elif echo "$REGISTER_RESULT" | grep -qi 'already\|exist\|duplicate'; then
  echo "OIDC provider already registered"
else
  echo "Warning: OIDC registration result: $REGISTER_RESULT"
fi
