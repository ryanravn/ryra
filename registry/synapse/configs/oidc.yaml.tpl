## OIDC overlay — rendered to /data/conf.d/oidc.yaml only when --auth is used.
## Merged on top of homeserver.yaml by Synapse's multi-config-path loader.

oidc_providers:
  - idp_id: authelia
    idp_name: "SSO"
    issuer: "${AUTH_EXTERNAL_URL}"
    client_id: "${OAUTH_CLIENT_ID}"
    client_secret: "${OAUTH_CLIENT_SECRET}"
    # Authelia registers ryra-managed OIDC clients with token_endpoint_auth_
    # method=client_secret_post; Synapse defaults to client_secret_basic and
    # Authelia rejects that with invalid_client.
    client_auth_method: client_secret_post
    scopes: ["openid", "profile", "email"]
    discover: true
    # Authelia puts `preferred_username` on the userinfo endpoint (the id_token
    # only carries the opaque `sub` UUID by default). Force Synapse to call
    # userinfo so user_mapping_provider below sees the real username.
    user_profile_method: "userinfo_endpoint"
    user_mapping_provider:
      config:
        subject_claim: sub
        localpart_template: "{{ user.preferred_username }}"
        display_name_template: "{{ user.name }}"
        email_template: "{{ user.email }}"

# Password login is left enabled so the shared-secret admin-create path keeps
# working. Flip this to true if you want SSO-only.
password_config:
  enabled: true
