---
title: Quick Start
description: Deploy your first service with Ryra in under 5 minutes.
---

This guide walks you through deploying your first service on a fresh server.

## 1. Install Ryra

```bash
curl -fsSL https://raw.githubusercontent.com/ryanravn/ryra/main/install.sh | sh
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
ryra config vaultwarden
```

`ryra config <service>` shows whether the service is installed, the URL it's reachable at, and the commands you can run against it. For live container output, use `journalctl --user -fu vaultwarden.service` or `systemctl --user status vaultwarden.service`. The unit names are plain systemd, exactly what you'd expect.

## 5. Browse available services

```bash
ryra search
ryra search forgejo    # filter
```

The `SUPPORTS` column tells you whether a service has native OIDC and/or SMTP integration.

## Next steps

- [Configure reverse proxy, SMTP, and auth](/guides/configuration/) for production use
- Add `--url https://service.example.com` to expose services through Caddy with automatic HTTPS
- Add `--auth` to enable SSO authentication via Authelia
- Run `ryra remove <service>` to stop and deregister a service. By default the data is preserved on disk; pass `--purge` to wipe it too.
