---
title: OpenClaw
description: Self-hosted AI assistant gateway.
---

[OpenClaw](https://docs.openclaw.ai/) is an AI assistant gateway — a self-hosted proxy that sits in front of Claude, GPT, and other AI APIs. It provides a unified interface, usage tracking, and access control.

## Install

```bash
ryra add openclaw
```

During setup, you'll be prompted for API keys (Anthropic, OpenAI — both optional).

## Details

| | |
|---|---|
| **Image** | `ghcr.io/openclaw/openclaw:2026.3.13-1` |
| **Port** | 18789 (gateway) |
| **Storage** | `/home/node/.openclaw` — configuration |
| **Min RAM** | 1 GB |

## Configuration

OpenClaw runs in local gateway mode with the control UI enabled. You can configure API keys during installation or update them later in the container environment.

## Data

Configuration is stored in a Podman volume mounted at `/home/node/.openclaw`.
