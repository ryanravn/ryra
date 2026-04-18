---
title: Configuration
description: Set up DNS, SSL, SMTP, and authentication for your services.
---

Ryra stores its global configuration at `~/.config/ryra/config.toml`. You can view and edit it with `ryra config`.

## Reverse proxy (Caddy)

Ryra uses Caddy as an optional reverse proxy with automatic HTTPS:

```bash
ryra add caddy
ryra add vaultwarden --domain vault.example.com
```

When you add a service with `--domain`, Ryra configures a Caddy site block that routes traffic to the service. Caddy handles TLS automatically.

## SMTP

Some services need to send email (password resets, notifications, etc.):

```bash
ryra config smtp
```

Provide your SMTP server, port, username, and password. Services that support email will automatically pick up these credentials.

## Viewing config

```bash
# Show full global config
ryra config

# Show config for a specific service
ryra config vaultwarden
```
