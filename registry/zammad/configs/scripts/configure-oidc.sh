#!/bin/bash
# Configure Zammad's native OpenID Connect integration by writing to the
# Setting table via rails runner. Zammad has no env-var surface for OIDC;
# this is the supported path.
#
# Zammad 7.x OIDC is PKCE-only (public client) — the strategy at
# /opt/zammad/lib/omni_auth/strategies/oidc_database.rb never passes
# client_secret into client_options. We configure as a public client and
# set the fqdn/http_type Settings so Zammad's redirect_uri matches what
# authelia has registered.
#
# Per docs/registry/conventions.md: ExecStartPost scripts must never return
# non-zero — it kills the service. We log failures and exit 0.
set -u
[ -z "${OAUTH_CLIENT_ID:-}" ] && exit 0

echo "oidc: waiting for railsserver on :3000 (up to 120 × 2s)"
for i in {1..120}; do
  if podman exec zammad-railsserver curl -sS -o /dev/null -m 3 "http://localhost:3000/" 2>/dev/null; then
    echo "oidc: railsserver ready after ${i} attempt(s)"
    break
  fi
  if [ $((i % 5)) -eq 0 ]; then
    echo "oidc: still waiting for railsserver (attempt ${i}/120)"
  fi
  sleep 2
done

if ! podman exec \
  -e OAUTH_CLIENT_ID="$OAUTH_CLIENT_ID" \
  -e OAUTH_ISSUER_URL="$OAUTH_ISSUER_URL" \
  -e ZAMMAD_FQDN="$ZAMMAD_FQDN" \
  -e ZAMMAD_HTTP_TYPE="$ZAMMAD_HTTP_TYPE" \
  zammad-railsserver bundle exec rails r '
    # Zammad builds redirect_uri as "#{http_type}://#{fqdn}/auth/openid_connect/callback".
    # Seed both from env so it matches what authelia has registered.
    Setting.set("fqdn", ENV["ZAMMAD_FQDN"]) if ENV["ZAMMAD_FQDN"].to_s != ""
    Setting.set("http_type", ENV["ZAMMAD_HTTP_TYPE"]) if ENV["ZAMMAD_HTTP_TYPE"].to_s != ""

    Setting.set("auth_openid_connect", true)
    Setting.set("auth_openid_connect_credentials", {
      "display_name" => "SSO",
      "identifier"   => ENV["OAUTH_CLIENT_ID"],
      "issuer"       => ENV["OAUTH_ISSUER_URL"],
      "uid_field"    => "sub",
      "scope"        => "openid email profile",
      "pkce"         => true,
    })
    # Auto-link OIDC users to existing local users by email on first login.
    Setting.set("auth_third_party_auto_link_at_inital_login", true)
    puts "Zammad OIDC configured for #{ENV["ZAMMAD_HTTP_TYPE"]}://#{ENV["ZAMMAD_FQDN"]}"
  ' 2>&1
then
  echo "configure-oidc.sh: failed to configure OIDC — re-run manually after first admin is created" >&2
fi
exit 0
