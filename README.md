# Ryra

[ryra.dev](https://ryra.dev) · [Docs](https://ryra.dev/docs) · [Install](#install)

**Self-host Supabase, Immich, Forgejo, Jellyfin, Vaultwarden and a dozen more — one command per service.**

Ryra is a CLI for running self-hosted services on a single Linux box. Each `ryra add <service>` pulls a container image, writes a systemd quadlet, starts the service under rootless Podman, and — if you want it — wires up HTTPS via Caddy and SSO via Authelia. No daemon.

Bring your own reverse proxy if you prefer: Cloudflare Tunnel, Tailscale Funnel, nginx — pass `--url https://…` and Ryra leaves routing alone.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/ryanravn/ryra/main/install.sh | sh
```

Works on Debian, Ubuntu, Fedora, and Arch and any other Linux system with Systemd and Podman.

## Quick start

```sh
ryra add openclaw
```

Ryra pulls the image, prompts for any API keys or passwords the service needs, generates a systemd quadlet, and starts the container under rootless Podman. `ryra list` shows what's running.

## What you can run

| | | |
|-|-|-|
| **Supabase** — backend-as-a-service | **OpenClaw** — AI assistant gateway | **Immich** — photos |
| **Forgejo** — git forge | **Jellyfin** — media server | **Vaultwarden** — password vault |
| **Open WebUI** — LLM frontend | **Synapse** — Matrix chat | **Paperless-ngx** — docs |
| **Seafile** — file sync | **Vikunja** — tasks | **Uptime Kuma** — monitoring |
| **DocuSeal** — e-signatures | **Ente** — E2E photos | **Twenty** — CRM |
| **Postgres** — database | **Caddy** — reverse proxy | **Authelia** — SSO/OIDC |
| **Inbucket** — dev SMTP | | |

Run `ryra search` to browse the full list with install status.

## Design

- Containers run under your user with rootless Podman. Ryra is a stateless CLI — no background process.
- systemd owns the lifecycle via Podman quadlets. `systemctl --user` and `journalctl --user` work as normal.
- Service data lives in `~/.local/share/ryra/<name>/`. `ryra remove` preserves it by default; `--purge` wipes it.
- The registry is plain TOML in `registry/`, one directory per service. Fork, edit, contribute back — no plugin system.
- Caddy and Authelia are services themselves, added the same way as anything else, only needed if you want HTTPS / SSO.
- E2E tests run in ephemeral QEMU VMs — fresh Debian or Fedora install, SSH in, run `ryra add`, assert.

## How it works

1. **`ryra init`** writes `~/.config/ryra/ryra.toml` and checks that `loginctl linger` is on for your user (so services survive logout).
2. **`ryra add <service>`** reads `registry/<service>/service.toml`, allocates a port, generates a quadlet at `~/.config/containers/systemd/<service>.container`, writes a `.env` with any secrets, and asks systemd to start it.
3. **`--url <public-url>`** records where the service will be reachable. If Caddy is installed, Ryra also adds a site block routing that hostname to the container. If you run your own reverse proxy (nginx, Cloudflare Tunnel, Tailscale Funnel, …), Ryra leaves the routing alone and just uses the URL to populate OIDC callbacks and email links.
4. **`--auth`** registers an OIDC client with the auth provider and either (a) injects credentials into the service's native OIDC config, or (b) puts Authelia's forward-auth in front of the service via Caddy.

### Where things live

| Path | What |
|---|---|
| `~/.config/ryra/ryra.toml` | Ryra's own config |
| `~/.config/containers/systemd/<svc>.container` | Generated quadlet |
| `~/.local/share/ryra/<svc>/` | Service data + `.env` |
| `~/.local/share/ryra/caddy/config/Caddyfile` | Routing config |

## Managing services

```sh
ryra list                        # installed services + orphans
ryra remove seafile              # stop + deregister, keep data
ryra remove seafile --purge      # also wipe the data dir and volumes
ryra remove -a                   # remove everything, preserve data
ryra reset                       # full teardown, including Ryra's own config
```

An "orphan" is a service you removed whose data is still on disk. `ryra list` shows them alongside installed services; `ryra remove <name> --purge` cleans them up.

## Development

Requires Rust stable.

```sh
cargo build
cargo run -- init
cargo run -- add whoami
```

### E2E tests

Tests run in ephemeral QEMU VMs — each test gets a fresh Linux install. Requires KVM and QEMU.

```sh
cargo run -- test                    # every test
cargo run -- test immich             # name filter
cargo run -- test list -v            # list tests with step detail
cargo run -- test --parallel=3       # 3 VMs in parallel
cargo run -- test --keep-alive       # boot once, SSH in to poke around
```

See `CLAUDE.md` for architectural conventions and `docs/` for deep-dives.

## License

AGPL-3.0-or-later. See [LICENCE.md](LICENCE.md).
