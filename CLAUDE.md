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

### Validate at the boundary

All TOML files are validated immediately after deserialization — if parsing succeeds, the data is safe to use without further checks:

- **`ServiceDef::validate()`** — duplicate names (ports, env), env var name format, env kind consistency, RAM consistency (recommended >= minimum)
- **`TestToml::validate()`** — mutually exclusive `[[tests]]`/`[[steps]]`, required fields per step action type
- **`Config::validate()`** — no duplicate service names
- **`StepAction`** is an enum, not a string — serde rejects unknown actions at parse time

When adding new fields or service definitions, the compiler and `validate()` should catch structural errors. Never silently default on missing/invalid data — error loudly at load time.

## No Unwraps, No Silent Failures

Never use `.unwrap()`, `.expect()`, or `panic!()`. Every fallible operation must be handled with `?`, `match`, or a meaningful default. This includes:

- `Option` values — use `?`, `ok_or()`, `unwrap_or_default()`, or pattern match
- `Result` values — propagate with `?` or handle explicitly
- Indexing — use `.get()` instead of `[]` where bounds aren't guaranteed

If something truly cannot fail, explain why in a comment and use `unwrap_or_else(|| unreachable!("reason"))` so the reasoning is documented.

**No silent failures:** Never use `.ok()`, `let _ =`, or `unwrap_or_default()` to discard errors that could leave the system in a bad state. Specifically:
- Don't fall back to `/tmp` or empty strings when paths/config can't be resolved — error instead
- Don't silently skip template rendering errors — if auth/SMTP mappings render to empty, that's an error
- Config file permission errors (`chmod 600`) must propagate — security-relevant
- Use `|| true` in shell hooks only when failure is genuinely acceptable, and document why

## Commits

Follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/). Prefix every commit subject with a type: `feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`, `ci:`.

## Architecture

- `ryra-core`: pure library, no CLI deps, no sudo, no side effects beyond file I/O to user-owned config
- `ryra-cli`: thin shell that calls core and handles sudo/system interaction
- `ryra-vm`: QEMU/SSH/cloud-init VM orchestration library
- `ryra-test`: E2E test runner, depends on ryra-vm
- Core returns typed results describing what needs to happen; CLI decides whether to apply or print
- All services run under the invoking user's rootless podman (`systemctl --user`)
- Quadlet files go to `~/.config/containers/systemd/`
- Service data goes to `~/.local/share/ryra/<name>/`
- `service_home()` and `quadlet_dir()` return `Result<PathBuf>` — they error if `$HOME` is unset rather than silently falling back to `/tmp`
- Warns if running as root

## Auth and Caddy Integration

### Domain philosophy

Services run at `http://127.0.0.1:<port>` by default — no domain, no HTTPS, no `/etc/hosts` entries needed. The `--domain` flag is opt-in for when the user wants to expose a service through Caddy with a custom hostname.

The only exception is **authelia**, which requires a domain because its OIDC implementation enforces HTTPS for `authelia_url`. When `--auth` is used, authelia auto-installs with a domain (defaults to `auth.localhost`). `.localhost` domains resolve to 127.0.0.1 automatically (RFC 6761) — no `/etc/hosts` entry needed.

This keeps the default experience frictionless (no sudo, no DNS, no certs) while still supporting custom domains for production use via Caddy, Tailscale, Cloudflare tunnels, or port forwarding.

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
5. Quadlet `ExecStartPost=` scripts handle runtime OIDC setup (API calls, config injection) after the container starts
6. Services without native OIDC get Caddy forward auth instead (Authelia handles login at the proxy level)
7. Services work at `http://127.0.0.1:<port>` without `--domain` — the OIDC redirect goes through authelia (HTTPS) and back to the service (HTTP)

### Pre-start and post-start scripts

Hooks are implemented as **quadlet-native `ExecStartPre=` / `ExecStartPost=`** directives in `.container` files — there is no hook abstraction in service.toml. Scripts live in `registry/<service>/configs/scripts/` and are copied to the service's data directory during `ryra add`. The quadlet file references them with `ExecStartPost=/bin/bash %h/.local/share/ryra/<service>/configs/scripts/<script>.sh`.

The service's `.env` file is loaded by the quadlet `EnvironmentFile=` directive, so env vars like `$RYRA_PORT_HTTP`, `$OAUTH_CLIENT_ID`, etc. are available to ExecStartPost scripts. Scripts access bind-mounted volumes via `$RYRA_SERVICE_HOME` (pointing to `~/.local/share/ryra/<service>/`).

### Template variables for auth

Available when `--auth` is used and an auth provider (authelia) is installed:
- `{{auth.url}}` — external URL of the auth provider
- `{{auth.internal_url}}` — how containers reach the auth provider directly via HTTP (`http://systemd-authelia:<port>` on shared podman network)
- `{{auth.issuer}}` — OIDC issuer URL (provider-specific)
- `{{auth.client_id}}` — generated UUID for OIDC client
- `{{auth.client_secret}}` — generated 64-char secret for OIDC client
- `{{auth.provider}}` — provider name (e.g., "authelia")

## Service Configuration Philosophy

Prefer environment variables and declarative config for all service setup. When a service can't be fully configured through envs alone (e.g., it requires plugin installation, API calls, or config file generation), use quadlet `ExecStartPre=` / `ExecStartPost=` scripts to automate those steps. The goal is that `ryra add <service>` is turnkey — the user shouldn't need to manually configure the service afterward. If some manual steps are truly unavoidable (e.g., initial web wizard, admin account creation via UI), document them clearly in the service description and guide the user through them after installation.

## Podman & Quadlet-Native Solutions

Always prefer podman-native and quadlet-native features over workarounds:

- **Networking**: Use podman networks for cross-container DNS resolution instead of `AddHost` with hardcoded IPs. Services with `--auth` join the auth provider's network for direct HTTP communication.
- **Service discovery**: Containers on the same network can resolve each other by container name (e.g., `systemd-authelia`). Use this instead of `host.containers.internal` or IP addresses
- **Volumes**: Use named volumes (`.volume` quadlet files) for data persistence, bind mounts only when host access is needed
- **Dependencies**: Use `After=` and `Requires=` in quadlet `[Unit]` sections for startup ordering
- **Health checks**: Use quadlet `HealthCmd=` instead of custom wait scripts

## System Dependencies

- `podman` — rootless containers for services
- `systemd` — user-level service management (`systemctl --user`)

## Debugging

**Never increase timeouts before identifying the root cause.** When a test times out, check logs first to understand *why* it's slow — don't just bump the number. Only increase a timeout after you've confirmed the issue is genuinely timing-related (e.g., a heavy image pull or slow SPA hydration) and there's no underlying bug. Timeouts mask real issues and make tests slow.

When tests fail or services error, **always check logs first** before proposing fixes:
- `journalctl --user -u <service>.service` for service logs
- `podman logs <container>` for container output (includes application logs)
- `podman exec <container> cat /path/to/app.log` for application-specific logs (e.g., seahub.log)
- `podman ps -a` to see container state
- `ss -tlnp` to check port bindings

This applies during development too — when a service fails after `ryra add`, check the logs to find the root cause rather than guessing.

## E2E Testing

Key points:

- Tests run inside ephemeral QEMU VMs — each test gets a fresh Linux install with its own kernel
- `--distro=debian-13` (default) or `--distro=fedora-43` selects the VM base image (flags on the test runner binary, not `ryra test`)
- Test runner lives in `crates/ryra-test/`, VM orchestration in `crates/ryra-vm/`
- Tests are defined in `registry/` via `[[tests]]` in service.toml and lifecycle test files in `registry/tests/`
- VMs use cloud images + cloud-init for setup, SSH for command execution
- `--parallel=N` controls concurrency (default 1), each VM gets a unique SSH port
- VM memory is auto-sized per test based on `[requirements.ram]` in each service's service.toml
- KVM is required for reasonable speed (`--no-kvm` works but is ~10x slower, flag on test runner binary)
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

### Adding a new service

After creating a service definition (service.toml, quadlets, configs), always run the E2E tests to verify it works. Use `--keep-alive` extensively to iterate — it boots the VM once and keeps it running so you can SSH in, inspect logs, check the UI, and fix issues without waiting for a fresh boot each time.

1. Start with `--keep-alive` to validate the service starts:
   - Boot the VM: `ryra test <service> --keep-alive --yes`
   - SSH in and check logs: `journalctl --user -u <service>.service`, `podman logs systemd-<service>`
   - Verify the service is actually responding before writing assertions
2. Run the simple tests: `ryra test <service>` — verify the service starts and responds
3. If the service has OIDC integration (`auth = ["oidc"]` in service.toml), write a browser test:
   - Create `registry/tests/<service>-auth-browser.toml` with `browser = true`
   - Create `registry/tests/browser/<service>-auth.spec.ts` with Playwright tests
   - Use `--keep-alive` on the auth browser test to boot the VM with caddy + authelia + the service, then SSH in and use `curl` to inspect the actual login page HTML — find the real CSS selectors, button text, and post-login indicators before writing the Playwright spec
   - The browser test must click the SSO button, authenticate with Authelia (fill username/password, submit, handle consent), and verify the redirect back results in an authenticated session
   - Don't guess at selectors — look at the real page. Iterate with `--keep-alive` until the test passes
4. Run the full auth browser test: `ryra test <service>-auth-browser`

When E2E tests have long setup times (pulling images, waiting for services), don't just wait — check logs periodically. If a service takes more than 60s to become ready, SSH in via `--keep-alive` and investigate rather than increasing timeouts blindly.
