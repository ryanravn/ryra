---
title: Configuration
description: Set up DNS, SSL, SMTP, and authentication for your services.
---

Ryra stores its global configuration at `~/.config/ryra/config.toml`. You can view and edit it with `ryra config`.

## DNS (Cloudflare)

Ryra can automatically create DNS records for your services via the Cloudflare API:

```bash
ryra config dns
```

You'll be prompted for:
- **API token** — a Cloudflare API token with DNS edit permissions
- **Zone ID** — the zone ID for your domain (found in the Cloudflare dashboard)
- **Domain** — your base domain (e.g., `example.com`)

Once configured, services exposed via `proxy` or `dns-only` modes will get DNS records created automatically.

## SSL

Ryra supports automatic SSL via Let's Encrypt:

```bash
ryra config ssl
```

Two challenge types are available:
- **DNS-01** — uses Cloudflare DNS to verify domain ownership (requires DNS config above). Works for wildcard certs.
- **HTTP-01** — uses a file served by nginx. Requires port 80 to be publicly accessible.

You can also use custom certificates by providing paths to your cert and key files.

## SMTP

Some services need to send email (password resets, notifications, etc.):

```bash
ryra config smtp
```

Provide your SMTP server, port, username, and password. Services that support email will automatically pick up these credentials.

## Viewing config

```bash
# Show full global config
ryra status

# Show config for a specific service
ryra status vaultwarden
```
