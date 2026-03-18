# E2E Test Plan

## Goal

Test ryra end-to-end inside ephemeral systemd-nspawn containers. Each test gets a fresh Linux environment with systemd, podman, and nginx — the full stack ryra needs. Tests can run in parallel.

## Architecture

```
Linux host (developer's machine or CI)
  └── ryra-e2e binary (the test runner)
       ├── spawns nspawn container "ryra-test-a1b2c3"
       │    └── runs ryra commands, checks results
       ├── spawns nspawn container "ryra-test-d4e5f6"  (parallel)
       │    └── runs different test
       └── collects results, destroys containers
```

The test runner is a standalone Rust binary in `tests/e2e/`. It manages the full lifecycle: base image creation, container spawn/destroy, test execution, assertions.

## Prerequisites on host

```
sudo apt install systemd-container debootstrap podman
```

The test runner checks for these and errors with a clear message if missing.

## Directory structure

```
tests/
  e2e/
    Cargo.toml              # standalone binary crate
    src/
      main.rs               # CLI entry point: --parallel=N, --distro=, test filters
      container.rs           # nspawn lifecycle: create base image, spawn, destroy, exec
      distro.rs              # distro-specific base image setup (debian, ubuntu, arch)
      assert.rs              # assertion helpers
      tests/
        mod.rs               # test registry — lists all tests
        init.rs              # ryra init
        add_service.rs       # add whoami, verify it works
        remove_service.rs    # remove, verify cleanup
        add_compose.rs       # compose-based service
        reset.rs             # full reset
        parallel_services.rs # add multiple services at once
    fixtures/
      registry/
        whoami/
          service.toml       # minimal quadlet service for testing
```

## How it works

### Base image creation (`container.rs` + `distro.rs`)

Each distro has a `create_base_image()` function:

**Debian 13:**
```
debootstrap trixie /var/lib/machines/ryra-base-debian-13
```
Then inside the chroot (via nspawn --pipe or systemd-nspawn -D ... /bin/bash -c):
```
apt install -y podman uidmap slirp4netns nginx curl systemd-container
systemctl enable podman.socket
```

**Arch (later):**
```
pacstrap /var/lib/machines/ryra-base-arch base podman nginx curl
```

**Ubuntu (later):**
```
debootstrap noble /var/lib/machines/ryra-base-ubuntu-2404
```

Base images are cached at `/var/lib/machines/ryra-base-{distro}`. Rebuild with `--rebuild-base`.

### Container lifecycle (`container.rs`)

**Spawn:**
```rust
fn spawn(distro: &str, test_id: &str) -> Container {
    let name = format!("ryra-test-{test_id}");
    let base = format!("/var/lib/machines/ryra-base-{distro}");
    let dest = format!("/var/lib/machines/{name}");

    // Copy base image
    Command::new("sudo").args(["cp", "-a", &base, &dest]).status();

    // Copy ryra binary into container
    let ryra_bin = find_ryra_binary(); // from target/release or target/debug
    Command::new("sudo").args(["cp", &ryra_bin, &format!("{dest}/usr/local/bin/ryra")]).status();

    // Copy test fixtures (registry)
    Command::new("sudo").args(["cp", "-a", "tests/e2e/fixtures/registry", &format!("{dest}/opt/ryra-test-registry")]).status();

    // Boot container
    Command::new("sudo").args([
        "systemd-nspawn",
        "--boot",
        &format!("--machine={name}"),
        &format!("--directory={dest}"),
        "--capability=all",
        "--system-call-filter=add_key keyctl bpf",
        "--private-network",  // isolated networking per container — no conflicts
        "--quiet",
    ]).spawn();

    // Wait for container to be ready
    wait_for_machine(&name);

    Container { name, dir: dest }
}
```

Key flags:
- `--capability=all` — rootless podman needs user namespace capabilities
- `--system-call-filter=add_key keyctl bpf` — additional syscalls podman needs
- `--private-network` — each container gets its own network namespace, no port conflicts between parallel tests
- `--boot` — full systemd init inside

**Execute command inside:**
```rust
fn exec(container: &str, cmd: &str) -> Output {
    Command::new("sudo")
        .args(["machinectl", "shell", container, "/bin/bash", "-c", cmd])
        .output()
}
```

**Destroy:**
```rust
fn destroy(container: &Container) {
    Command::new("sudo").args(["machinectl", "poweroff", &container.name]).status();
    // Wait for shutdown
    Command::new("sudo").args(["rm", "-rf", &container.dir]).status();
}
```

### Test structure

Each test is a function that receives a `Container` handle:

```rust
// tests/add_service.rs

pub fn test_add_whoami(c: &Container) -> TestResult {
    // Init ryra with local-only config, pointing to test registry
    c.exec("ryra init --repo /opt/ryra-test-registry")?;

    // Add whoami (non-interactive picks Local exposure)
    c.exec("ryra add whoami --repo /opt/ryra-test-registry")?;

    // Wait for service to start
    c.wait_for_service("whoami", "whoami.service", Duration::from_secs(30))?;

    // Assert service is active
    c.assert_service_active("whoami", "whoami.service")?;

    // Get allocated port from env file
    let port = c.exec("grep RYRA_PORT /var/lib/whoami/.env | head -1 | cut -d= -f2")?
        .stdout_trimmed();

    // Assert HTTP response
    c.assert_curl(&format!("http://127.0.0.1:{port}"), 200)?;

    // Assert no errors in journal
    c.assert_journal_clean("whoami.service")?;

    // Assert config was updated
    let config = c.exec("cat /etc/ryra/ryra.toml")?.stdout_trimmed();
    assert!(config.contains("whoami"));

    Ok(())
}
```

### Assertions (`assert.rs`)

```rust
fn assert_service_active(container: &str, user: &str, unit: &str) -> Result<()>
    // machinectl shell -> systemctl --machine={user}@ --user is-active {unit}

fn assert_service_inactive(container: &str, user: &str, unit: &str) -> Result<()>

fn assert_curl(container: &str, url: &str, expected_status: u16) -> Result<()>
    // machinectl shell -> curl -sf -o /dev/null -w '%{http_code}' {url}

fn assert_journal_clean(container: &str, unit: &str) -> Result<()>
    // machinectl shell -> journalctl _SYSTEMD_USER_UNIT={unit} -p err -q --no-pager
    // fails if any error-level entries exist

fn assert_user_exists(container: &str, username: &str) -> Result<()>
    // machinectl shell -> id {username}

fn assert_user_not_exists(container: &str, username: &str) -> Result<()>

fn assert_file_exists(container: &str, path: &str) -> Result<()>

fn assert_file_not_exists(container: &str, path: &str) -> Result<()>

fn assert_port_listening(container: &str, port: u16) -> Result<()>
    // machinectl shell -> ss -tlnp | grep :{port}
```

### Test runner (`main.rs`)

```
Usage: ryra-e2e [OPTIONS] [TESTS...]

Options:
  --parallel <N>      Max concurrent containers (default: 1)
  --distro <name>     Base image distro (default: debian-13)
  --rebuild-base      Recreate the base image from scratch
  --ryra-bin <path>   Path to ryra binary (default: auto-detect from workspace)
  --keep-failed       Don't destroy containers for failed tests (for debugging)
  --list              List available tests

Examples:
  ryra-e2e                           # run all tests sequentially
  ryra-e2e --parallel=4              # run 4 at a time
  ryra-e2e add_service remove        # run specific tests
  ryra-e2e --distro=arch --parallel=2
```

Parallel execution uses a semaphore/thread pool:

```rust
let semaphore = Arc::new(Semaphore::new(parallel));
let mut handles = vec![];

for test in tests {
    let permit = semaphore.acquire().await;
    handles.push(tokio::spawn(async move {
        let id = random_id();
        let container = spawn(&distro, &id);
        let result = test.run(&container);
        if !keep_failed || result.is_ok() {
            destroy(&container);
        }
        (test.name, result)
    }));
}

// Collect and report results
```

### Test fixtures

`fixtures/registry/whoami/service.toml`:
```toml
[service]
name = "whoami"
description = "Simple HTTP service that returns request info"
image = "docker.io/traefik/whoami:latest"

[[ports]]
name = "http"
container_port = 80

[nginx]
upstream_port = "http"
```

This is a minimal service that starts fast and responds to HTTP — ideal for testing the full flow.

## Refactoring needed in ryra (optional but helpful)

### 1. Config paths via env vars

Currently hardcoded in `config/mod.rs:17-19`. Not strictly needed since each nspawn container has its own `/etc/ryra`, but useful for local dev:

```rust
pub fn resolve() -> Result<Self> {
    let config_dir = std::env::var("RYRA_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/ryra"));
    let cache_dir = std::env::var("RYRA_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/var/cache/ryra"));
    // ...
}
```

### 2. Non-interactive mode already works

The CLI checks `is_terminal()` and falls back to defaults. `Local` exposure is picked automatically in non-interactive mode. `--yes` exists on remove/reset. No changes needed.

### 3. Sudoless mode inside containers

Inside nspawn containers, the test runner runs as root. ryra's `apply.rs` prepends `sudo` to everything. This works fine — `sudo` as root is a no-op. No changes needed.

## Test list (initial)

| Test | What it verifies |
|------|-----------------|
| `init` | `ryra init` creates config at `/etc/ryra/ryra.toml` |
| `add_service` | Add whoami → user created, podman container running, port responding, journal clean |
| `remove_service` | Remove whoami → user deleted, files cleaned up, port freed |
| `add_compose` | Add a compose-based service (needs fixture) → compose stack running |
| `reset` | Init + add service + reset → everything gone |
| `parallel_services` | Add 2-3 services → all running, no port conflicts |
| `host_port_exposure` | Add with HostPort mode → bound to 0.0.0.0 |
| `idempotent_init` | Init twice → no errors, config unchanged |

## Build and run

```bash
# On Linux (your VM or any Debian machine):
cd tests/e2e

# Build ryra first
cargo build --release -p ryra-cli

# Build test runner
cargo build --release

# Create base image (first time only)
sudo ./target/release/ryra-e2e --rebuild-base

# Run all tests
sudo ./target/release/ryra-e2e --parallel=4

# Run specific test
sudo ./target/release/ryra-e2e add_service

# Debug a failure (keeps container alive)
sudo ./target/release/ryra-e2e --keep-failed add_service
# Then: sudo machinectl shell ryra-test-xxxxx
```
