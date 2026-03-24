---
title: Forgejo
description: Self-hosted Git forge with built-in CI/CD.
---

[Forgejo](https://forgejo.org/docs/) is a self-hosted Git forge — like GitHub, but on your own server. It includes repository hosting, issue tracking, pull requests, and Forgejo Actions (CI/CD).

## Install

```bash
ryra add forgejo
```

## Details

| | |
|---|---|
| **Image** | `codeberg.org/forgejo/forgejo:11` |
| **Port** | 3000 (HTTP) |
| **Storage** | `/data` — repositories, avatars, attachments |
| **Database** | Scoped PostgreSQL (automatically set up) |
| **Min RAM** | 256 MB |

## Scoped PostgreSQL

Forgejo comes with its own dedicated PostgreSQL instance. You don't need to install PostgreSQL separately — Ryra creates a scoped sidecar that:
- Runs under the same user as Forgejo
- Uses a dedicated volume for database storage
- Is not shared with other services
- Connects via a private Podman network

## Data

- **Git repositories**: stored in the `/data` volume
- **Database**: stored in a separate `db-data` volume managed by the scoped PostgreSQL sidecar
