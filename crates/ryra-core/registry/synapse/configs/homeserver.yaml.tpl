## Synapse base config — rendered from env at service start time by
## configs/scripts/render-config.sh (envsubst). This template intentionally
## contains only the always-on bits; OIDC is added as an overlay file in
## /data/conf.d/oidc.yaml when --auth is used.

server_name: "${SYNAPSE_SERVER_NAME}"
public_baseurl: "${SYNAPSE_PUBLIC_BASEURL}"
pid_file: /data/homeserver.pid
report_stats: false

listeners:
  - port: 8008
    tls: false
    type: http
    x_forwarded: true
    bind_addresses: ["0.0.0.0"]
    resources:
      - names: [client, federation]
        compress: false

database:
  name: psycopg2
  args:
    user: synapse
    password: "${POSTGRES_PASSWORD}"
    database: synapse
    host: db
    port: 5432
    cp_min: 5
    cp_max: 10

log_config: "/etc/synapse-config/log.config"
media_store_path: /data/media_store
signing_key_path: /data/signing.key

registration_shared_secret: "${SYNAPSE_REGISTRATION_SHARED_SECRET}"
macaroon_secret_key: "${SYNAPSE_MACAROON_SECRET}"
form_secret: "${SYNAPSE_FORM_SECRET}"

# Open registration is disabled. To create users: run
#   register_new_matrix_user -c /data/homeserver.yaml http://localhost:8008
# inside the synapse container, or POST to /_synapse/admin/v1/register with
# the shared secret above.
enable_registration: false
enable_registration_without_verification: false

# Federation off by default — this is a closed homeserver. Remove these two
# lines (and restart) to federate with the wider Matrix network.
federation_domain_whitelist: []
allow_public_rooms_over_federation: false

# Trusted key servers are only consulted during federation. Keep the default
# matrix.org trust so a future federation-on switch works out of the box.
trusted_key_servers:
  - server_name: "matrix.org"

suppress_key_server_warning: true
