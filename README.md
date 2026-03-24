# Ryra

[ryra.dev](https://ryra.dev) | [Docs](https://ryra.dev/docs)

A tool to test and deploy self-hosted services on a Linux server using rootless Podman and systemd. Built-in VM testing gives AI agents fast feedback loops for building infrastructure and deploying apps.

Each service gets its own Linux user, container isolation, and systemd lifecycle management. Nginx handles reverse proxying, with optional Cloudflare tunnel/DNS integration.

## Quick start

```
ryra init
ryra add whoami
```

## Development

Requires Rust (stable).

```
cargo build
cargo run -- init
cargo run -- add whoami
```

### E2E tests

Tests run in ephemeral QEMU VMs. Requires KVM and QEMU packages (see `CLAUDE.md`).

```
cargo run -- test --vm whoami
```

## License

AGPL-3.0
