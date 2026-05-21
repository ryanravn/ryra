---
title: Quick Start
description: Deploy your first service with Ryra in under 5 minutes.
---

This guide walks you through deploying your first service on a fresh server.

## 1. Install Ryra

```bash
curl -fsSL https://ryra.dev/install.sh | sh
```

Verify your environment is wired up correctly:

```bash
ryra doctor
```

It checks subuid/subgid for rootless Podman, loginctl linger, and any drift in installed services. Fixes are printed inline.

## 2. Deploy a service

Let's start with Vaultwarden, a Bitwarden-compatible password manager:

```bash
ryra add vaultwarden
```

Ryra will:
- Install system dependencies (podman, etc.) if needed
- Pull the container image
- Generate systemd quadlet units
- Start the service under your user via rootless Podman

## 3. List your services

```bash
ryra list           # installed services
ryra list -l        # also include data sizes and volumes
```

## 4. Inspect a service

```bash
ryra list -l
```

Shows every installed service with its URL, status, and storage. For live container output use `journalctl --user -fu vaultwarden.service` or `systemctl --user status vaultwarden.service` — the unit names are plain systemd, exactly what you'd expect.

## 5. Browse available services

```bash
ryra search
```

The `SUPPORTS` column tells you whether a service has native OIDC and/or SMTP integration.

## Next steps

Optional flags on `ryra add`:

- `--url https://...` for a public URL (auto-HTTPS if Caddy is installed)
- `--auth` for SSO (Authelia auto-installs)
- `--smtp=inbucket` for a local test inbox; for a real relay set `[smtp]` in `~/.config/services/preferences.toml`

`ryra remove <service>` to uninstall. Data is preserved; add `--purge` to wipe it too.
