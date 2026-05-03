---
title: Configuration
description: Set up DNS, SSL, SMTP, and authentication for your services.
---

Ryra stores its global configuration at `~/.config/services/preferences.toml`. You can view and edit it with `ryra config`.

## Reverse proxy (Caddy)

Ryra uses Caddy as an optional reverse proxy with automatic HTTPS:

```bash
ryra add caddy
ryra add vaultwarden --url https://vault.example.com
```

When you add a service with `--url`, Ryra configures a Caddy site block that routes traffic from the URL's hostname to the service. Caddy handles TLS automatically.

If you run your own reverse proxy (nginx, Traefik, Cloudflare Tunnel, Tailscale Funnel, etc.), pass `--url` with the public URL your setup exposes. Ryra uses it to populate template variables like OIDC callback URLs, email links, and `{{service.external_url}}` without installing Caddy or changing your routing.

## SMTP

Some services need to send email (password resets, notifications, signup confirmations, etc.):

```bash
ryra config smtp
```

This drops you into an interactive prompt for server, port, credentials, and from-address. Services with native SMTP integration (the `smtp` row of the `SUPPORTS` column in `ryra search`) will pick up the credentials automatically.

For local testing, install the [Inbucket](https://inbucket.org) sink instead:

```bash
ryra add inbucket
ryra add forgejo --smtp=inbucket    # wires this service to inbucket non-interactively
```

## Authentication

```bash
ryra config auth
```

Lets you point Ryra at an external OIDC provider, or (more commonly) install Authelia as the provider via `ryra add authelia`. Once an auth provider is configured, pass `--auth` on subsequent `ryra add` calls to wire SSO into a service.

## Viewing config

```bash
ryra config                # global overview: SMTP, auth, service count
ryra config vaultwarden    # per-service: install state, URL, commands
ryra list                  # services + status (add -l for data size and volumes)
ryra doctor                # diagnose subuid/subgid, linger, drift; fixes printed inline
```
