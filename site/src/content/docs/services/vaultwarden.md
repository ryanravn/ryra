---
title: Vaultwarden
description: Bitwarden-compatible password vault server.
---

[Vaultwarden](https://github.com/dani-garcia/vaultwarden/wiki) is a lightweight, self-hosted server compatible with the Bitwarden ecosystem — browser extensions, mobile apps, and desktop clients all work with it.

## Install

```bash
ryra add vaultwarden
```

## Details

| | |
|---|---|
| **Image** | `vaultwarden/server:1.35.4` |
| **Port** | 80 (HTTP) |
| **Storage** | `/data` — vault database, attachments, icons |
| **Min RAM** | 128 MB |
| **SMTP** | Supported — for password reset and invitation emails |

## Configuration

Vaultwarden is configured with sensible defaults:
- **Signups disabled** by default (invite-only)
- **WebSocket enabled** for live sync across devices
- **Domain** set automatically based on your exposure config

### SMTP (optional)

If you've configured SMTP globally (`ryra config smtp`), Vaultwarden will use it for sending emails. This enables:
- Password reset emails
- User invitation emails
- Emergency access notifications

## Data

All vault data is stored in a Podman volume mounted at `/data`. This includes the SQLite database, file attachments, and icon cache.
