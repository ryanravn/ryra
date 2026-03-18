# E2E Test Plan

## Goal

Test ryra end-to-end inside ephemeral QEMU VMs. Each test gets a fresh Debian install with its own kernel, systemd, podman, and nginx — identical to what a real user would have. Tests can run in parallel.

## Architecture

```
Linux host (developer's machine or CI with KVM)
  └── ryra-e2e binary (the test runner)
       ├── spawns QEMU VM "ryra-test-a1b2c3" (ssh port 10022)
       │    └── runs ryra commands via SSH, checks results
       ├── spawns QEMU VM "ryra-test-d4e5f6" (ssh port 10023, parallel)
       │    └── runs different test
       └── collects results, destroys VMs
```

The test runner is a standalone Rust binary in `tests/e2e/`. It manages the full lifecycle: cloud image download, VM spawn/destroy, test execution, assertions, and result reporting.

## Prerequisites on host

```
sudo apt install qemu-system-arm qemu-utils qemu-efi-aarch64 \
    genisoimage openssh-client curl
```

The test runner checks for these and errors with a clear message if missing. KVM (`/dev/kvm`) is required by default; use `--no-kvm` for software emulation (slower).

## Directory structure

```
tests/
  e2e/
    Cargo.toml              # standalone binary crate
    src/
      main.rs               # CLI entry point: --parallel=N, --distro=, --no-kvm, etc.
      machine.rs             # VM lifecycle: spawn QEMU, SSH exec, SCP, destroy
      image.rs               # cloud image download/cache, EFI firmware discovery
      ports.rs               # atomic SSH port allocator for parallel VMs
      scenario.rs            # scenario builder + result types with per-event tracing
      assert.rs              # assertion helpers (service, curl, user, file, journal)
      tests/
        mod.rs               # scenario registry — all test scenarios defined here
    fixtures/
      registry/
        whoami/
          service.toml       # minimal quadlet service for testing
```

## How it works

### Base image (`image.rs`)

Downloads the Debian 13 generic cloud image (qcow2) from `cloud.debian.org` and caches it locally at `~/.cache/ryra-e2e/`. EFI firmware is located from standard system paths (`/usr/share/AAVMF/` or `/usr/share/qemu-efi-aarch64/`).

Re-download with `--redownload`.

### VM lifecycle (`machine.rs`)

**Spawn:**
1. `qemu-img create -b base.qcow2` — copy-on-write disk per test (fast, small)
2. Generate SSH key pair per VM
3. Build cloud-init seed ISO with:
   - Root SSH key injection
   - Package install: podman, uidmap, slirp4netns, nginx, curl, systemd-container
   - Enable podman.socket, configure sshd for root login
4. Boot QEMU with KVM acceleration, port-forwarded SSH
5. Wait for SSH + cloud-init completion
6. SCP ryra binary + test fixtures into the VM

**Execute command:**
```
ssh -i /tmp/ryra-test-xxx/id_ed25519 -p 10022 root@127.0.0.1 "ryra add whoami"
```

**Destroy:**
```
ssh poweroff → kill QEMU → rm -rf work_dir
```

**Debug failed tests:**
With `--keep-failed`, the VM stays running and prints the SSH command:
```
ssh -o StrictHostKeyChecking=no -i /tmp/ryra-test-xxx/id_ed25519 -p 10022 root@127.0.0.1
```

### Scenario builder (`scenario.rs`)

Tests are defined declaratively with a builder pattern:

```rust
Scenario::new("add-whoami")
    .add("whoami")
    .assert_running("whoami")
    .assert_user_exists("ryra-whoami")
    .assert_http("whoami", 200)
    .assert_journal_clean("whoami")
    .assert_config_contains("whoami")
```

Steps and assertions interleave in order — you can assert between steps:

```rust
Scenario::new("remove-whoami")
    .add("whoami")
    .assert_running("whoami")    // runs after add
    .remove("whoami")
    .assert_not_running("whoami") // runs after remove
```

Every scenario automatically runs `ryra init` first.

**Result tracking:** Each step and assertion produces an `Event` with description, outcome (pass/fail/skip), and duration. Failed steps skip all remaining phases. The output looks like:

```
PASS  add-whoami (45.2s)
  [ ok ] init: ryra init --repo /opt/ryra-test-registry (1.2s)
  [ ok ] step: ryra add whoami (25.1s)
  [ ok ] assert: whoami is running (0.3s)
  [ ok ] assert: user ryra-whoami exists (0.1s)
  [ ok ] assert: whoami returns HTTP 200 (0.2s)
  [ ok ] assert: whoami.service journal is clean (0.1s)
  [ ok ] assert: config contains 'whoami' (0.1s)

FAIL  remove-whoami (38.7s)
  [ ok ] init: ryra init --repo /opt/ryra-test-registry (1.1s)
  [ ok ] step: ryra add whoami (24.3s)
  [ ok ] assert: whoami is running (0.2s)
  [FAIL] step: ryra remove whoami (2.1s)
         command failed in VM: exit 1
  [skip] assert: whoami is not running
  [skip] assert: user ryra-whoami does not exist
```

### Available assertions

| Method | What it checks |
|--------|---------------|
| `assert_running(service)` | `systemctl --machine=ryra-{service}@ --user is-active` |
| `assert_not_running(service)` | opposite of above |
| `assert_http(service, status)` | `curl` the service's allocated port, check HTTP status |
| `assert_user_exists(username)` | `id {username}` succeeds |
| `assert_user_not_exists(username)` | `id {username}` fails |
| `assert_file_exists(path)` | `test -e {path}` |
| `assert_file_not_exists(path)` | opposite of above |
| `assert_config_contains(text)` | grep ryra.toml for substring |
| `assert_config_not_contains(text)` | opposite of above |
| `assert_journal_clean(service)` | no error-level journal entries |

### Test runner (`main.rs`)

```
Usage: ryra-e2e [OPTIONS] [TESTS...]

Options:
  --parallel <N>      Max concurrent VMs (default: 1)
  --distro <name>     Base image distro (default: debian-13)
  --redownload        Re-download the base cloud image
  --ryra-bin <path>   Path to ryra binary (default: auto-detect from workspace)
  --keep-failed       Keep VMs alive for failed tests (prints SSH command)
  --no-kvm            Disable KVM acceleration (software emulation, slower)
  --memory <MB>       VM memory in MB (default: 2048)
  --cpus <N>          VM CPU count (default: 2)
  --list              List available scenarios

Examples:
  ryra-e2e                           # run all tests sequentially
  ryra-e2e --parallel=4              # run 4 VMs at a time
  ryra-e2e add-whoami remove         # run specific tests (substring match)
  ryra-e2e --keep-failed add-whoami  # debug a failure
  ryra-e2e --no-kvm                  # software emulation (no /dev/kvm needed)
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

[integrations]
auth = false
smtp = false
```

## Scenario list

| Scenario | What it verifies |
|----------|-----------------|
| `init` | `ryra init` creates config at `/etc/ryra/ryra.toml` |
| `idempotent-init` | Running init twice works, config preserved |
| `add-whoami` | Add whoami → user created, container running, HTTP 200, journal clean |
| `remove-whoami` | Add then remove → user deleted, service stopped, config cleaned |
| `reset` | Init + add + reset → everything gone |
| `re-add-after-remove` | Remove then re-add same service → works correctly |

## Build and run

```bash
# Build ryra first
cargo build --release -p ryra-cli

# Build test runner
cd tests/e2e
cargo build --release

# Run all tests (needs KVM)
./target/release/ryra-e2e --parallel=4

# Run specific test
./target/release/ryra-e2e add-whoami

# Debug a failure (VM stays alive, prints SSH command)
./target/release/ryra-e2e --keep-failed add-whoami

# Without KVM (slower, works anywhere)
./target/release/ryra-e2e --no-kvm
```
