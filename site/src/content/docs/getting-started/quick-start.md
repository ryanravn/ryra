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
- Install system dependencies (podman, nginx, etc.) if needed
- Create a `ryra-vaultwarden` Linux user
- Pull the container image
- Generate systemd quadlet units
- Configure nginx as a reverse proxy
- Prompt you for exposure settings (local, tunnel, domain, etc.)

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

- [Configure DNS, SSL, and SMTP](/guides/configuration/) for production use
- [Learn about exposure modes](/guides/exposure-modes/) to control how services are accessed
- [Rust Docs](https://docs.rs/ryra) for the full API and command reference
