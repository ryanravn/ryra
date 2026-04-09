---
name: Prefer quadlet-native over Caddy workarounds
description: Minimize reliance on Caddy — prefer podman networks, quadlet features. Only use Caddy when HTTPS/domain is actually required.
type: feedback
---

Prefer quadlet-native and podman-native features over Caddy whenever possible. Caddy should only be used when HTTPS or domain-based routing is genuinely required (e.g., OIDC session cookies needing Secure flag).

**Why:** User values simplicity and wants infrastructure to rely on quadlet primitives (networks, aliases, volumes, dependencies) rather than routing everything through a reverse proxy.

**How to apply:** When designing inter-container communication, default to podman network DNS (container names) and network aliases. Only route through Caddy when TLS is needed for the protocol to work correctly.
