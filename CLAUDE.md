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

## Auth and Caddy Integration

### How `--domain` works

When `ryra add <service> --domain foo.example.com` is called:
1. The domain is passed to the template context as `{{service.domain}}`
2. If Caddy is installed, a site block is added to the Caddyfile routing the domain to the service's port
3. `caddy reload` applies the new route
4. On `ryra remove`, the route is cleaned up (reload is skipped if the Caddyfile becomes empty)

### How `--auth` works

When `ryra add <service> --auth` is called:
1. An OIDC client ID and secret are generated and injected into the template context (`{{auth.client_id}}`, `{{auth.client_secret}}`, `{{auth.issuer}}`, etc.)
2. Services with native OIDC mappings (`[mappings.auth]` in service.toml) get OIDC env vars written to `.env`
3. Native OIDC services join the auth provider's podman network for direct HTTP communication
4. OIDC client registration is handled by `authelia.rs` (edits authelia's configuration.yml)
5. Post-start hooks (`[[post_start]]`) run after the service starts — these configure OIDC via APIs or config files
6. Services without native OIDC get Caddy forward auth instead (Authelia handles login at the proxy level)

### Pre-start and post-start hooks

Hooks in `[[pre_start]]` and `[[post_start]]` run on the **host** (not inside containers), with the service's `.env` sourced into the environment. Pre-start hooks run before the container starts (e.g., generating config files), post-start hooks run after. This means:
- Use `$RYRA_PORT_HTTP`, `$OAUTH_CLIENT_ID`, etc. directly — they're already in the environment
- Use `$HOME/.local/share/ryra/<service>/` paths to access bind-mounted volumes on the host
- Do NOT hardcode paths like `/var/lib/<service>/` — that's the container's view, not the host's

### Template variables for auth

Available when `--auth` is used and an auth provider (authelia) is installed:
- `{{auth.url}}` — external URL of the auth provider
- `{{auth.internal_url}}` — how containers reach the auth provider directly via HTTP (`http://systemd-authelia:<port>` on shared podman network)
- `{{auth.issuer}}` — OIDC issuer URL (provider-specific)
- `{{auth.client_id}}` — generated UUID for OIDC client
- `{{auth.client_secret}}` — generated 64-char secret for OIDC client
- `{{auth.provider}}` — provider name (e.g., "authelia")

## Podman & Quadlet-Native Solutions

Always prefer podman-native and quadlet-native features over workarounds:

- **Networking**: Use podman networks for cross-container DNS resolution instead of `AddHost` with hardcoded IPs. Services with `--auth` join the auth provider's network for direct HTTP communication.
- **Service discovery**: Containers on the same network can resolve each other by container name (e.g., `systemd-authelia`). Use this instead of `host.containers.internal` or IP addresses
- **Volumes**: Use named volumes (`.volume` quadlet files) for data persistence, bind mounts only when host access is needed
- **Dependencies**: Use `After=` and `Requires=` in quadlet `[Unit]` sections for startup ordering
- **Health checks**: Use quadlet `HealthCmd=` instead of custom wait scripts

## System Dependencies

- `podman` — rootless containers for services

## Debugging

When tests fail, don't just increase timeouts. SSH into the VM and study the actual logs to find the root cause:
- `journalctl --user -u <service>.service` for service logs
- `podman ps -a` to see container state
- `podman logs <container>` for container output
- `ss -tlnp` to check port bindings

## E2E Testing

Key points:

- Tests run inside ephemeral QEMU VMs — each test gets a fresh Linux install with its own kernel
- `--distro=debian-13` (default) or `--distro=fedora-43` selects the VM base image
- Test runner lives in `crates/ryra-test/`, VM orchestration in `crates/ryra-vm/`
- Tests are defined in `registry/` via `[[tests]]` in service.toml and lifecycle test files in `registry/tests/`
- VMs use cloud images + cloud-init for setup, SSH for command execution
- `--parallel=N` controls concurrency (default 1), each VM gets a unique SSH port
- VM memory is auto-sized per test based on `[requirements.ram]` in each service's service.toml
- KVM is required for reasonable speed (`--no-kvm` works but is ~10x slower)
- `--keep-alive` keeps the VM running after tests for interactive debugging
- `--verbose` dumps the serial log on failure
- Host prerequisites (Debian/Ubuntu): `qemu-system-arm qemu-utils qemu-efi-aarch64 genisoimage openssh-client curl`
- Host prerequisites (Fedora): `qemu-system-aarch64 qemu-img edk2-aarch64 genisoimage openssh-clients curl`

### Test types

- **SingleService tests**: defined in `[[tests]]` within `service.toml` — auto-discovered, run `ryra add` then assertions
- **Lifecycle tests**: defined in `registry/tests/<name>.toml` — multi-step sequences of add/remove/assert/wait/run steps

### OIDC lifecycle tests

OIDC tests must install caddy and authelia before adding services with `--auth --domain`. The test steps are:
1. `ryra add caddy` — reverse proxy
2. `ryra add authelia --domain auth.test.local` — OIDC provider
3. `ryra add <service> --auth --domain <service>.test.local` — service with OIDC
4. Assertions verify HTTP responds and OIDC is configured
