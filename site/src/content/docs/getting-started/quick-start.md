---
title: Quick Start
description: Deploy your first service with Ryra in under 5 minutes.
---

This guide walks you through deploying your first service on a fresh server.

## 1. Install Ryra

```bash
curl -fsSL https://raw.githubusercontent.com/ryanravn/ryra/main/install.sh | sh
```

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

## 3. Check the status

```bash
ryra status vaultwarden
```

This shows the service's container state, exposed ports, and configuration.

## 4. List your services

```bash
ryra list
```

## 5. Browse available services

```bash
ryra search
```

This shows all services available in the registry that you can deploy.

## Next steps

- [Configure reverse proxy, SMTP, and auth](/guides/configuration/) for production use
- Add `--domain` to expose services through Caddy with automatic HTTPS
- Add `--auth` to enable SSO authentication via Authelia
