---
title: Uptime Kuma
description: Self-hosted monitoring and status pages.
---

[Uptime Kuma](https://github.com/louislam/uptime-kuma) is a self-hosted monitoring tool. It watches your services and shows a status page — like a self-hosted Pingdom or UptimeRobot.

## Install

```bash
ryra add uptime-kuma
```

## Details

| | |
|---|---|
| **Image** | `louislam/uptime-kuma:1` |
| **Port** | 3001 (HTTP) |
| **Storage** | `/app/data` — monitoring history, settings |
| **Min RAM** | 128 MB |

## Features

- HTTP(S), TCP, DNS, ping, and keyword monitoring
- Beautiful status pages you can share publicly
- Notification integrations (Slack, Discord, Telegram, email, etc.)
- Multi-language support

## Data

All monitoring data and configuration is stored in a SQLite database in the `/app/data` volume.
