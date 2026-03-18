# Ryra Development Guidelines

## Core Principle: Make Invalid State Unrepresentable

Use enums and pattern matching everywhere instead of string comparisons, boolean flags, or if-chains. This applies at every layer:

- **Config values**: DNS, SSL, SMTP, auth providers are enums with associated data, not string fields with optional companions
- **Commands/actions**: Operations returned from core to CLI are typed enums (e.g., `Step::CreateUser { .. }`, `Step::WriteFile { .. }`), not string commands that get parsed with `.contains()`
- **Service status**: `Available | Installed`, not a bool flag
- **Service kind**: `Application | Infrastructure`, not a string

When adding new functionality, ask: "Can this state be invalid?" If yes, restructure with enums so the type system prevents it. Pattern matching (`match`) must be exhaustive — the compiler enforces that every case is handled.

**Anti-patterns to avoid:**
- `if config.provider == "cloudflare"` → use `match config.dns { DnsConfig::Cloudflare { .. } => .. }`
- `if cmd.contains("chown")` → use `match step { Step::ChownFiles { .. } => .. }`
- Optional fields that are only valid in certain states → put them inside enum variants

## No Unwraps — Handle Every Error

Never use `.unwrap()`, `.expect()`, or `panic!()`. Every fallible operation must be handled with `?`, `match`, or a meaningful default. This includes:

- `Option` values — use `?`, `ok_or()`, `unwrap_or_default()`, or pattern match
- `Result` values — propagate with `?` or handle explicitly
- Indexing — use `.get()` instead of `[]` where bounds aren't guaranteed

If something truly cannot fail, explain why in a comment and use `unwrap_or_else(|| unreachable!("reason"))` so the reasoning is documented.

## Architecture

- `ryra-core`: pure library, no CLI deps, no sudo, no side effects beyond file I/O to user-owned config
- `ryra-cli`: thin shell that calls core and handles sudo/system interaction
- Core returns typed results describing what needs to happen; CLI decides whether to apply or print
- Each service gets its own Linux user (`ryra-<name>`) running rootless podman
- nginx runs as a root system quadlet with `Network=host` — the only privileged component

## System Dependencies

- `podman` — rootless containers for services, root containers for nginx/cloudflared
- `systemd-container` — provides `systemd-machined` and `--machine=` support for managing user services of other users (e.g., `systemctl --machine=ryra-whoami@ --user start whoami`)
- `loginctl` linger — keeps service users' systemd alive without login sessions

## E2E Testing

See `E2E_TEST_PLAN.md` for the full plan. Key points:

- Tests run inside systemd-nspawn containers — each test gets a fresh Linux environment
- The test runner (`tests/e2e/`) is a standalone Rust binary, not shell scripts
- `--parallel=N` controls concurrency, `--private-network` isolates each container
- The `registry/` dir is currently empty — test fixtures go in `tests/e2e/fixtures/registry/`
- The nspawn `--capability=all --system-call-filter=add_key keyctl bpf` flags are needed for rootless podman inside containers — if podman fails inside nspawn, this is the first thing to check
- `sudo` inside nspawn is a no-op when running as root, so ryra's apply.rs works unmodified
- Base images are created with `debootstrap` called from Rust (`Command::new`), not shell scripts
