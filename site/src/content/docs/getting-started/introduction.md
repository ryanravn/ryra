---
title: Introduction
description: What Ryra is and how it works.
---

Ryra is a CLI tool that deploys self-hosted services on a single Linux machine using **rootless Podman** and **systemd quadlets**.

Instead of writing Docker Compose files, nginx configs, and systemd units by hand, you run `ryra add <service>` and Ryra scaffolds everything from a curated registry of service definitions.

## How it works

1. **You run a command** like `ryra add vaultwarden`
2. **Ryra creates a dedicated Linux user** for the service
3. **Rootless Podman** runs the container under that user — no root containers
4. **A systemd quadlet** manages the container lifecycle (start, stop, restart)
5. **Nginx** is configured as a reverse proxy with SSL termination
6. **DNS and SSL** are set up automatically via Cloudflare (optional)

Every service is isolated: its own user, its own container runtime, its own systemd scope. If one service breaks, the others keep running.

## Architecture

- **`ryra-core`** — pure library that generates configs and returns typed steps
- **`ryra-cli`** — thin shell that executes steps (creates users, writes files, runs systemctl)
- **nginx** — the only root service, reverse-proxies all applications with `Network=host`
- **podman** — rootless containers for every service
- **systemd** — manages everything via quadlet units and `loginctl` linger

## What Ryra is not

- **Not a PaaS** — there's no web UI, no dashboard, no multi-tenant abstractions. It's a CLI tool.
- **Not multi-server** — Ryra manages one machine. For multiple servers, run independent instances.
- **Not Docker** — Ryra uses Podman exclusively. No Docker daemon required.

## License

Ryra is licensed under [AGPL-3.0](https://www.gnu.org/licenses/agpl-3.0.html).
