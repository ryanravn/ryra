---
title: Configuration
description: Set up DNS, SSL, SMTP, and authentication for your services.
---

Ryra stores its global configuration at `~/.config/ryra/config.toml`. You can view and edit it with `ryra config`.

## Reverse proxy (Caddy)

Ryra uses Caddy as an optional reverse proxy with automatic HTTPS:

```bash
ryra add caddy
ryra add vaultwarden --url https://vault.example.com
```

When you add a service with `--url`, Ryra configures a Caddy site block that routes traffic from the URL's hostname to the service. Caddy handles TLS automatically.

If you run your own reverse proxy (nginx, Traefik, Cloudflare Tunnel, Tailscale Funnel, etc.), pass `--url` with the public URL your setup exposes. Ryra uses it to populate template variables like OIDC callback URLs, email links, and `{{service.external_url}}` without installing Caddy or changing your routing.

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
