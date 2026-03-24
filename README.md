# Ryra

[ryra.dev](https://ryra.dev) | [Docs](https://ryra.dev/docs)

A CLI tool that scaffolds self-hosted services on a single Linux server using rootless Podman and systemd.

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
