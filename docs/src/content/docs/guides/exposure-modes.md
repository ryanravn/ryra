---
title: Exposing Services
description: How --url and --auth change what ryra add wires up.
---

`ryra add <service>` is the only command you need. Two optional flags change how the service is reached and authenticated:

- `--url https://...` gives the service a public URL.
- `--auth` wires it to SSO.

Use either, both, or neither.

## Default: localhost

```bash
ryra add forgejo
```

Binds to `localhost:<dynamic-port>`. Find the port with `ryra list -l`.

## With a public URL

```bash
ryra add forgejo --url https://git.example.com
```

If Caddy is installed (`ryra add caddy`), ryra adds a site block routing `git.example.com` to the service's port; Caddy provisions TLS automatically. Removing the service cleans up the route.

If Caddy isn't installed, ryra leaves routing to whatever you already run (nginx, Traefik, Cloudflare Tunnel, Tailscale Funnel, etc.). It still uses the URL to populate template variables for OIDC callbacks, email links, and `{{service.external_url}}`. Point your reverse proxy at the `http://127.0.0.1:<port>` shown by `ryra list`.

## With SSO

```bash
ryra add forgejo --auth
```

Authelia auto-installs at `https://auth.internal:<port>` if it isn't there yet, and the service is wired in. To pick the Authelia URL yourself, add Authelia first:

```bash
ryra add authelia --url https://auth.example.com
```

Services with native OIDC get an OIDC client registered with Authelia and the matching env vars injected at install time. Services without native OIDC get Caddy forward auth, so Authelia handles login at the proxy level.

Run `ryra search` to see which services advertise `oidc` in the `SUPPORTS` column. As of v0.1.0 that includes Forgejo, Immich, Nextcloud, Open WebUI, Paperless-ngx, Seafile, Synapse, Vikunja, and Zammad.

## Combine them

```bash
ryra add forgejo --auth --url https://git.example.com
```

The two flags are independent: each works with or without the other.
