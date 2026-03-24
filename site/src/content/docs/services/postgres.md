---
title: PostgreSQL
description: Standalone PostgreSQL database server.
---

[PostgreSQL](https://www.postgresql.org/docs/) is the standalone database service. Use this when you want a shared PostgreSQL instance that multiple services can connect to.

:::note
Most services that need PostgreSQL (like Forgejo) come with their own scoped instance. You only need the standalone service if you want a general-purpose database.
:::

## Install

```bash
ryra add postgres
```

You'll be prompted for the admin username and default database name.

## Details

| | |
|---|---|
| **Image** | `postgres:17-alpine` |
| **Port** | 5432 (TCP) |
| **Storage** | `/var/lib/postgresql/data` — database files |
| **Kind** | Infrastructure |
| **Min RAM** | 128 MB |

## Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `POSTGRES_USER` | `postgres` | Database admin username |
| `POSTGRES_DB` | `postgres` | Default database name |
| `POSTGRES_PASSWORD` | auto-generated | Admin password (stored as a Podman secret) |

## Data

All database files are stored in a Podman volume mounted at `/var/lib/postgresql/data`.
