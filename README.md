# Ryra

[ryra.dev](https://ryra.dev) | [Docs](https://ryra.dev/docs)

A tool to deploy self-hosted services on Linux using rootless Podman and systemd. Built-in VM testing gives AI agents fast feedback loops for building infrastructure and deploying apps.

Each service gets container isolation via rootless Podman and systemd lifecycle management. Caddy handles reverse proxying with automatic HTTPS and optional SSO authentication via Authelia.

## Quick start

```
ryra init
ryra add whoami
```

### With a domain and reverse proxy

```
ryra add caddy
ryra add whoami --domain whoami.example.com
```

### With SSO authentication

```
ryra add caddy
AUTHELIA_ADMIN_PASSWORD=secret ryra add authelia --domain auth.example.com
ryra add whoami --domain whoami.example.com --auth
```

The `--auth` flag enables authentication for the service:
- Services with native OIDC support (immich, seafile) get OIDC configured automatically via post-start hooks
- Other services get Caddy forward auth — Authelia handles login before requests reach the service

## How it works

1. **`ryra init`** — creates `~/.config/ryra/ryra.toml`
2. **`ryra add <service>`** — generates Podman quadlet files, .env, and starts the service via systemd
3. **`ryra add caddy`** — installs Caddy as a reverse proxy (ports 8080/8443)
4. **`--domain`** — adds a Caddy site block routing the domain to the service
5. **`--auth`** — registers an OIDC client with the auth provider and configures the service

### Service layout

- Quadlet files: `~/.config/containers/systemd/`
- Service data: `~/.local/share/ryra/<name>/`
- Caddy config: `~/.local/share/ryra/caddy/config/Caddyfile`
- Service config: `~/.local/share/ryra/<name>/.env`

### Template variables

Service definitions in `registry/<name>/service.toml` use template variables in env values:

- `{{service.name}}`, `{{service.port}}`, `{{service.url}}`, `{{service.domain}}`
- `{{smtp.host}}`, `{{smtp.port}}`, `{{smtp.username}}`, `{{smtp.password}}`, `{{smtp.from}}`
- `{{auth.url}}`, `{{auth.internal_url}}`, `{{auth.issuer}}`, `{{auth.client_id}}`, `{{auth.client_secret}}`
- `{{services.<name>.port.<port_name>}}`, `{{services.<name>.env.<VAR>}}`
- `{{secret.<name>}}` — auto-generated secrets

## Managing data

`ryra list` shows every service ryra knows about — installed services
plus "orphans" (removed services whose data still lives on disk).
Each row includes the size, home-dir path, and podman volumes.

Removing a service keeps its data by default:

    ryra remove seafile           # stops + deregisters; keeps data + volumes
    ryra remove seafile --purge   # stops + deregisters + wipes everything

`--purge` also works on orphans to clean up leftover data:

    ryra list                     # shows seafile as `orphan` after a soft remove
    ryra remove seafile --purge   # wipes the leftover home dir + volumes

Bulk:

    ryra remove -a                # preserve data for every installed service
    ryra remove -a --purge        # wipe every service + every orphan's data

`ryra reset` additionally wipes ryra's own config, the Caddy CA from the
system trust store, snapshots, and registry caches.

## Development

Requires Rust (stable).

```
cargo build
cargo run -- init
cargo run -- add whoami
```

### E2E tests

Tests run in ephemeral QEMU VMs. Requires KVM and QEMU packages (see `CLAUDE.md`).

```
cargo run -- test                    # run all tests
cargo run -- test whoami             # run tests matching "whoami"
cargo run -- test list               # list available tests (add -v for full step details)
cargo run -- test --parallel=3       # run 3 VMs concurrently
cargo run -- test --keep-alive       # boot a VM for interactive debugging
```

## License

AGPL-3.0
