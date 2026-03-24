---
title: Exposure Modes
description: Control how your services are accessed from the network.
---

Ryra supports several ways to expose services. You choose the mode when adding a service, or change it later:

```bash
ryra expose <service>
```

## Modes

### `local`

The service is only accessible from the server itself (localhost). Use this for services that don't need to be accessed remotely, or for infrastructure services like databases.

### `host-port`

Binds the service to a port on the host. Accessible from the local network without a reverse proxy. Useful for development or LAN-only services.

### `tunnel`

Exposes the service via a **Cloudflare Tunnel**. No need to open ports on your firewall — traffic goes through Cloudflare's network. Requires Cloudflare DNS to be configured.

### `proxy`

Routes traffic through **nginx** with a domain name. Ryra creates a DNS record pointing to your server and configures nginx with SSL termination. Requires:
- Cloudflare DNS configured (`ryra config dns`)
- SSL configured (`ryra config ssl`)
- Ports 80/443 open on your firewall

### `dns-only`

Creates a DNS record but doesn't configure nginx. Useful when the service handles its own TLS or when you want to manage the reverse proxy yourself.

### `public`

Similar to `proxy` but marks the service as intentionally public-facing. Functionally the same as `proxy` — the distinction is for your own bookkeeping.

## Changing exposure

You can change how a service is exposed at any time:

```bash
ryra expose vaultwarden
```

Ryra will prompt you to pick a new mode and reconfigure nginx and DNS accordingly.
