# Ryra

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](LICENCE.md)
[![CI](https://github.com/ryanravn/ryra/actions/workflows/ci.yml/badge.svg)](https://github.com/ryanravn/ryra/actions/workflows/ci.yml)

> Self-host anything, lifecycle-tested in virtual machines.

Ryra scaffolds **rootless**, **daemonless** **podman** containers wired into a shared **SSO** and **email** setup. The bundled registry covers useful services, each **lifecycle-tested** in a fresh virtual machine, and the test framework is simple enough that you can have an AI add new services and prove they work the same way.

[Website](https://ryra.dev) · [Docs](https://ryra.dev/intro/) · [Services](https://ryra.dev/services/)

## Install

```sh
curl -fsSL https://ryra.dev/install.sh | sh
```

Works on Debian, Ubuntu, Fedora, Arch, and any Linux with systemd and Podman.

## What it does

`ryra add <service>` reads a recipe from a curated registry and writes:

- A **rootless Podman** container, owned by your user
- A **systemd quadlet**, so `systemctl --user` and `journalctl --user` work like normal
- Optionally: a **Caddy** route with auto-HTTPS, and an **Authelia** OIDC client for SSO

Service data lives at `~/.local/share/services/<name>/`. Back it up with `tar`. Uninstall ryra and your stack keeps running, because the systemd units and containers stay.

## Why

Self-hosting one service is a weekend of docker-compose files, reverse proxy configs, and TLS certs. Self-hosting ten is a part-time job. Ryra collapses each of those weekends into `ryra add <service>`, and every service in the registry is lifecycle-tested in a fresh QEMU VM before it ships.

## Examples

### Replace your cloud storage

<img src="https://raw.githubusercontent.com/ryanravn/ryra/main/site/public/screenshots/seafile.webp" alt="Seafile file storage UI" width="900" />

```sh
ryra add seafile
```

### Replace your todo list

<img src="https://raw.githubusercontent.com/ryanravn/ryra/main/site/public/screenshots/vikunja.webp" alt="Vikunja task manager UI" width="900" />

```sh
ryra add vikunja
```

### Run your own AI gateway

<img src="https://raw.githubusercontent.com/ryanravn/ryra/main/site/public/screenshots/openclaw.webp" alt="OpenClaw AI gateway UI" width="900" />

```sh
ryra add openclaw
```

### Install anything

<img src="https://raw.githubusercontent.com/ryanravn/ryra/main/site/public/screenshots/custom.webp" alt="A service.toml definition" width="900" />

The registry is plain TOML and quadlet files. Drop a definition in for your own app, point ryra at your registry, and install it the same way as anything bundled.

## Services

Run `ryra search` for the full list, or browse the [services catalog](https://ryra.dev/services/). The bundled registry includes Immich, Forgejo, Vaultwarden, Nextcloud, Twenty CRM, Paperless-ngx, Synapse, Supabase, Open WebUI, Authelia, Uptime Kuma, Caddy, DocuSeal, Zammad, Seafile, Vikunja, OpenClaw, and more.

## Documentation

Full docs at [ryra.dev/intro](https://ryra.dev/intro/).

## License

AGPL-3.0-or-later. See [LICENCE.md](LICENCE.md).
