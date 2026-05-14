# Ryra

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](LICENCE.md)
[![CI](https://github.com/ryanravn/ryra/actions/workflows/ci.yml/badge.svg)](https://github.com/ryanravn/ryra/actions/workflows/ci.yml)

> Self-host anything, automatically test it works.

Ryra scaffolds **rootless**, **daemonless** **podman** containers wired with the same authorization system (**SSO**) and e-mail system with **SMTP**. The bundled registry covers useful services, each **end-to-end tested** in a fresh virtual machine, and the test framework is simple enough that you can have an AI add new services and prove they work the same way.

[Website](https://ryra.dev) · [Docs](https://ryra.dev/intro/) · [Services](https://ryra.dev/services/)

## Install

```sh
curl -fsSL https://ryra.dev/install.sh | sh
```

Works on Debian, Ubuntu, Fedora, Arch, and any Linux with systemd and Podman.

## Quickstart

```sh
ryra search           # browse the registry
ryra add <service>    # install one
```

## What it does

`ryra add <service>` reads a recipe from a curated registry and writes:

- A **rootless Podman** container, owned by your user
- A **systemd quadlet**, so `systemctl --user` and `journalctl --user` work like normal
- Optionally: a **Caddy** route with auto-HTTPS, and an **Authelia** OIDC client for SSO

Service data lives at `~/.local/share/services/<name>/`. Back it up with `tar`. Uninstall ryra and your stack keeps running, because the systemd units and containers stay.

## Why

SaaS prices keep climbing and the products keep moving slower than you want. Self-hosting is the way out, but the operational cost (compose files, reverse proxies, expiring certs, half-finished install scripts) is what stops most people from leaving.

No other self-hosting toolkit ships the full combination: rootless **podman quadlets** for security and clean systemd integration, **automated VM tests** that prove every registry service works before you install it, and a TOML-based recipe format that an AI can read and extend without hand-holding. You stay in control: customise per host, add your own services, and grow your stack at the pace your vendors won't.

## Examples

### Replace your cloud storage

<img src="site/public/screenshots/seafile.webp" alt="Seafile file storage UI" width="900" />

```sh
ryra add seafile
```

### Replace your todo list

<img src="site/public/screenshots/vikunja.webp" alt="Vikunja task manager UI" width="900" />

```sh
ryra add vikunja
```

### Run your own AI gateway

<img src="site/public/screenshots/openclaw.webp" alt="OpenClaw AI gateway UI" width="900" />

```sh
ryra add openclaw
```

### Install anything

<img src="site/public/screenshots/custom.webp" alt="A service.toml definition" width="900" />

The registry is plain TOML and quadlet files. Drop a definition in for your own app, point ryra at your registry, and install it the same way as anything bundled.

## Services

Run `ryra search` for the full list, or browse the [services catalog](https://ryra.dev/services/). The bundled registry includes Immich, Forgejo, Vaultwarden, Nextcloud, Twenty CRM, Paperless-ngx, Synapse, Supabase, Open WebUI, Authelia, Uptime Kuma, Caddy, DocuSeal, Zammad, Seafile, Vikunja, OpenClaw, and more.

## Documentation

Full docs at [ryra.dev/intro](https://ryra.dev/intro/).

## License

AGPL-3.0-or-later. See [LICENCE.md](LICENCE.md).
