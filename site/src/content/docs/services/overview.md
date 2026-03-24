---
title: Services
description: Available services in the Ryra registry.
---

Ryra deploys services from a curated registry. Each service is a pre-configured container definition with nginx, systemd, and DNS integration built in.

## Browse & install

```bash
# See what's available
ryra search

# Install a service
ryra add <service>
```

## Available services

| Service | Description | Kind | Min RAM |
|---------|-------------|------|---------|
| [Vaultwarden](/services/vaultwarden/) | Bitwarden-compatible password vault | Application | 128 MB |
| [Forgejo](/services/forgejo/) | Self-hosted Git forge | Application | 256 MB |
| [Uptime Kuma](/services/uptime-kuma/) | Monitoring and status pages | Application | 128 MB |
| [OpenClaw](/services/openclaw/) | AI assistant gateway | Application | 1 GB |
| [PostgreSQL](/services/postgres/) | Relational database | Infrastructure | 128 MB |

## How services work

Each service gets:
- **Its own Linux user** (`ryra-<service>`) for isolation
- **A rootless Podman container** running under that user
- **A systemd quadlet** for lifecycle management
- **An nginx reverse proxy** entry (for web-facing services)
- **loginctl linger** to keep systemd alive without a login session

Services are independent — if one breaks, the others keep running.

## Service lifecycle

```bash
ryra add <service>       # Install and start
ryra status <service>    # Check status
ryra diff <service>      # See what changed in the registry
ryra update <service>    # Re-scaffold with latest definition
ryra remove <service>    # Remove completely
```
