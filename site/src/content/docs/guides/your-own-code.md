---
title: Run Your Own Code
description: Build and run your own service on Ryra, no registry needed.
---

There are two ways to put a service on Ryra:

- **From the registry**: `ryra add forgejo`. Curated, pre-tested services.
- **Your own code**: scaffold a project with `ryra init`, run it with `ryra add .`.

This page covers the second.

## 1. Scaffold

In a project that listens on a port (Rust, Bun, Go, anything):

```bash
ryra init
```

Ryra detects the project and writes a `service.toml`:

```toml
[service]
name = "my-app"
runtime = "native"
build = "bun install"                 # optional
run = "bun --watch run src/index.ts"

[[ports]]
name = "http"
container_port = 3000
```

`run` is the command Ryra runs under `systemd --user`. Your code reads its port
from `SERVICE_PORT_HTTP` and its data directory from `SERVICE_HOME`.

## 2. Run it

```bash
ryra add .
```

Now edit your code. If your `run` command watches for changes (`bun --watch`,
`cargo watch`), it reloads on save. Otherwise `ryra upgrade` rebuilds from source
and restarts. `ryra remove --purge` tears it all down.

Your repo *is* the service.

## 3. Zero-downtime deploys (blue/green)

By default `ryra upgrade` restarts the service in place — a brief gap while the
new version starts. If that gap matters, opt into a blue/green deploy with two
lines:

```toml
[service]
name = "my-app"
runtime = "native"
build = "bun install"
run = "bun run src/index.ts"
deploy = "blue-green"          # <- opt in
health_check = "/healthz"      # <- how Ryra knows the new version is live

[[ports]]
name = "http"
container_port = 3000
```

Now Ryra runs **two slots**, `blue` and `green`. On `ryra upgrade` it builds the
new version on the idle slot, waits for `health_check` to return `200`, swaps the
reverse proxy over with a graceful reload (no dropped connections), then stops
the old slot. If the health check never passes, the deploy aborts with the old
version still serving — a failed deploy is a no-op, never an outage. Because the
old slot lingers through the swap, the next deploy rolls straight back onto it.

This works the same whether `runtime` is `native` (any language — each slot gets
its own isolated working copy of your code) or `podman` (each slot is a
container). Two requirements:

- **The health endpoint must mean it.** Return `200` only once the process is
  truly ready to serve — database reachable, migrations run. Ryra trusts it as
  the signal to move traffic.
- **Migrations must be backwards-compatible.** During the swap both versions are
  briefly live against the same database, so additive (expand/contract) changes
  only — don't ship a destructive migration in the same deploy as code that
  depends on it.
