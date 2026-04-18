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

## Domain access with Caddy

To expose a service on a domain with automatic HTTPS:

1. **Install Caddy** (if not already installed):

   ```bash
   ryra add caddy
   ```

2. **Add a service with `--domain`**:

   ```bash
   ryra add vaultwarden --domain vault.example.com
   ```

Ryra adds a site block to the Caddyfile that routes `vault.example.com` to the service's port. Caddy handles TLS certificate provisioning automatically.

When you remove a service with `ryra remove`, its Caddy route is cleaned up automatically.

## Authentication with Authelia

To add SSO authentication to a service:

1. **Install Authelia**:

   ```bash
   ryra add authelia --domain auth.example.com
   ```

2. **Add a service with `--auth`**:

   ```bash
   ryra add forgejo --auth --domain git.example.com
   ```

Services with native OIDC support (Forgejo, Immich, Seafile) get OIDC configured automatically. Other services get Caddy forward auth via Authelia.
