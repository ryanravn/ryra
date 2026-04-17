## Core Principle: Make Invalid State Unrepresentable

Use enums and pattern matching everywhere instead of string comparisons, boolean flags, or if-chains. This applies at every layer

When adding new functionality, ask: "Can this state be invalid?" If yes, restructure with enums so the type system prevents it. Pattern matching (`match`) must be exhaustive — the compiler enforces that every case is handled.

**Anti-patterns to avoid:**
- `if config.provider == "letsencrypt"` → use `match config.ssl { SslConfig::Letsencrypt { .. } => .. }`
- `if cmd.contains("start")` → use `match step { Step::StartService { .. } => .. }`
- Optional fields that are only valid in certain states → put them inside enum variants

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

## Auth and Caddy Integration

### Domain philosophy

Services run at `http://127.0.0.1:<port>` by default — no domain, no HTTPS, no `/etc/hosts` entries needed. The `--domain` flag is opt-in for when the user wants to expose a service through Caddy with a custom hostname.

The only exception is **authelia**, which requires a domain because its OIDC implementation enforces HTTPS for `authelia_url`. When `--auth` is used, authelia auto-installs with a domain (defaults to `auth.localhost`). `.localhost` domains resolve to 127.0.0.1 automatically (RFC 6761) — no `/etc/hosts` entry needed. Some services require HTTPS to work.

This keeps the default experience frictionless (no sudo, no DNS, no certs) while still supporting custom domains for production use via Caddy, Tailscale, Cloudflare tunnels, or port forwarding.


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

When creating tests of any kind, always aim to understand the structure first. The fastest way to do this is to start systems normally using `ryra add ...`, making sure we have a clean testing environment on the host, then perhaps call endpoints. If you intend to write a playwright test, you can use things like chrome MCP / extensions to explore the webpage, or use playwirght to understand more than test at first, and only then create the test and iterate on the host. After this, try running it in the VM, if it fails, do --keep-alive, and iterate untill you find issues and finalize the test with ephemeral VM test.

When E2E tests have long setup times (pulling images, waiting for services), don't just wait — check logs periodically. If a service takes more than 60s to become ready, SSH in via `--keep-alive` and investigate rather than increasing timeouts blindly.
