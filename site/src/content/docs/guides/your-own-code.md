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
