# Ryra Development Guidelines

## Core Principle: Make Invalid State Unrepresentable

Use enums and pattern matching everywhere instead of string comparisons, boolean flags, or if-chains. This applies at every layer:

- **Config values**: DNS, SSL, SMTP, auth providers are enums with associated data, not string fields with optional companions
- **Commands/actions**: Operations returned from core to CLI are typed enums (e.g., `Step::WriteFile { .. }`, `Step::StartService { .. }`), not string commands that get parsed with `.contains()`
- **Service status**: `Available | Installed`, not a bool flag
- **Service kind**: `Application | Infrastructure`, not a string

When adding new functionality, ask: "Can this state be invalid?" If yes, restructure with enums so the type system prevents it. Pattern matching (`match`) must be exhaustive — the compiler enforces that every case is handled.

**Anti-patterns to avoid:**
- `if config.provider == "letsencrypt"` → use `match config.ssl { SslConfig::Letsencrypt { .. } => .. }`
- `if cmd.contains("start")` → use `match step { Step::StartService { .. } => .. }`
- Optional fields that are only valid in certain states → put them inside enum variants

## No Unwraps — Handle Every Error

Never use `.unwrap()`, `.expect()`, or `panic!()`. Every fallible operation must be handled with `?`, `match`, or a meaningful default. This includes:

- `Option` values — use `?`, `ok_or()`, `unwrap_or_default()`, or pattern match
- `Result` values — propagate with `?` or handle explicitly
- Indexing — use `.get()` instead of `[]` where bounds aren't guaranteed

If something truly cannot fail, explain why in a comment and use `unwrap_or_else(|| unreachable!("reason"))` so the reasoning is documented.

## Commits

Follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/). Prefix every commit subject with a type: `feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`, `ci:`.

## Architecture

- `ryra-core`: pure library, no CLI deps, no sudo, no side effects beyond file I/O to user-owned config
- `ryra-cli`: thin shell that calls core and handles sudo/system interaction
- `ryra-vm`: QEMU/SSH/cloud-init VM orchestration library
- `ryra-test`: E2E test runner, depends on ryra-vm
- `ryra-dev-tools`: build tooling (generate-registry)
- Core returns typed results describing what needs to happen; CLI decides whether to apply or print
- All services run under the invoking user's rootless podman (`systemctl --user`)
- Quadlet files go to `~/.config/containers/systemd/`
- Service data goes to `~/.local/share/ryra/<name>/`
- Warns if running as root
- nginx runs as a root system quadlet with `Network=host` — the only privileged component

## System Dependencies

- `podman` — rootless containers for services, root containers for nginx

## E2E Testing

Key points:

- Tests run inside ephemeral QEMU VMs — each test gets a fresh Linux install with its own kernel
- `--distro=debian-13` (default) or `--distro=fedora-43` selects the VM base image
- Test runner lives in `crates/ryra-test/`, VM orchestration in `crates/ryra-vm/`
- Tests are defined in `registry/` via `[[tests]]` in service.toml and lifecycle test files in `registry/tests/`
- VMs use cloud images + cloud-init for setup, SSH for command execution
- `--parallel=N` controls concurrency, each VM gets a unique SSH port
- KVM is required for reasonable speed (`--no-kvm` works but is ~10x slower)
- `--keep-failed` keeps VMs alive and prints the SSH command for debugging
- `--verbose` dumps the serial log on failure
- Host prerequisites (Debian/Ubuntu): `qemu-system-arm qemu-utils qemu-efi-aarch64 genisoimage openssh-client curl`
- Host prerequisites (Fedora): `qemu-system-aarch64 qemu-img edk2-aarch64 genisoimage openssh-clients curl`
