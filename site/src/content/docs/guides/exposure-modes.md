---
title: Exposing Services
description: Control how your services are accessed from the network.
---

By default, services bind to a dynamically allocated port on localhost. To make them accessible via a domain name with HTTPS, use Caddy as a reverse proxy.

## Local access (default)

When you run `ryra add <service>` without any flags, the service is only accessible on the host via `localhost:<port>`. Check the assigned port with:

```bash
ryra config <service>
```

## Public URL with Caddy

To expose a service on a public URL with automatic HTTPS:

1. **Install Caddy** (if not already installed):

   ```bash
   ryra add caddy
   ```

2. **Add a service with `--url`**:

   ```bash
   ryra add vaultwarden --url https://vault.example.com
   ```

Ryra adds a site block to the Caddyfile that routes `vault.example.com` to the service's port. Caddy handles TLS certificate provisioning automatically.

When you remove a service with `ryra remove`, its Caddy route is cleaned up automatically.

## Using your own reverse proxy

If you already run nginx, Traefik, a Cloudflare Tunnel, a Tailscale Funnel, or any other external routing, skip `ryra add caddy`. Still pass `--url` to tell Ryra the public URL your setup exposes:

```bash
ryra add vaultwarden --url https://vault.example.com
```

Ryra uses the URL to populate template variables (OIDC callback URLs, email links, `{{service.external_url}}`) but won't touch the Caddyfile or generate certs. Point your reverse proxy at the service's `http://127.0.0.1:<port>` binding shown by `ryra list`.

## Authentication with Authelia

To add SSO authentication to a service:

1. **Install Authelia**:

   ```bash
   ryra add authelia --url https://auth.example.com
   ```

2. **Add a service with `--auth`**:

   ```bash
   ryra add forgejo --auth --url https://git.example.com
   ```

Services with native OIDC support (Forgejo, Immich, Seafile) get OIDC configured automatically. Other services get Caddy forward auth via Authelia.
