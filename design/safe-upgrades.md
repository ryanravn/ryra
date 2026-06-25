# Safe upgrades: auto-revert on failure

Status: proposed. Grew out of the 0.9.14 drift fix (`fix(upgrade): stop
flagging auth-bridge hook-rewritten files as drift`) while reviewing whether the
upgrade path is actually safe end to end.

## Why

"Safe" is three different properties, and they have different answers:

- **Availability** — no downtime.
- **Integrity** — no data loss / corruption.
- **Reversibility** — if it goes wrong, you can get back.

Two of these can't be had in full and we should stop implying otherwise:
strict *atomicity* of an upgrade is impossible (it's heterogeneous OS side
effects, not a DB transaction), and blue/green is fundamentally unsafe for a
service that owns its own mutable on-disk state. Everything else is reachable
with machinery we already have. The realistic target is **reversible +
self-healing + zero-downtime-for-stateless**, not "atomic".

## What already exists (don't rebuild)

- **Drift detection** — `upgrade::diff_service` classifies every rendered file
  Unchanged / Modified / Drift / Added / Removed against `service.manifest`
  (SHA256 lock). Strong; this is the right model.
- **Backup-before-overwrite** — `upgrade_service` computes a timestamped
  `~/.local/state/ryra/backups/<ts>/<svc>/` and inserts `Step::CopyFile` to back
  up every displaced file *before* the write. The pre-upgrade manifest is always
  backed up so revert can compute Added files.
- **`ryra revert`** — `upgrade::revert_service` already restores a backup dir and
  deletes upgrade-added files. Auto-revert is "run this for the user on failure",
  not new logic.
- **Blue/green health gate** — `deploy::color_swap_steps` starts the idle color,
  then `WaitForHttpHealthy` (real `curl`, expects 200, polled) before Caddy is
  repointed and the old color stopped. Failure aborts *before* stop-old.
- **restic backups** — `ryra backup` snapshots the data dir. The integrity
  safety net for stateful changes already exists; it just isn't wired to upgrade.

## The gap

`system::apply::execute_all` is `for step in steps { execute(step).await?; }` —
it stops on the first error and **does not roll back**. So a failed
`RestartService` / health step leaves a half-applied state, and recovery is
manual. Today's only mitigation is the operator remembering `ryra revert`.

## Design: `--on-failure` toggle

Model the failure policy as an enum (not two booleans), per "make invalid state
unrepresentable":

```
ryra upgrade --on-failure keep      # default: stop, leave it, print the fix
ryra upgrade --on-failure rollback  # opt-in: return to pre-upgrade state
```

**Default: `keep`.** Two reasons it must not auto-revert by default:

1. **Fix-forward is legitimate.** Operators may want the failed state left intact
   to inspect logs and patch forward; a silent rollback erases the evidence.
2. **Rollback can make a stateful failure worse.** If the upgrade already ran a
   migration that mutated the shared data dir, reverting the *files* leaves
   old-code-against-new-data — a new broken state. Auto-revert is only truly safe
   for config/quadlet/code-only changes.

This is the mainstream posture, not us being timid: **Kamal** leaves the old
container serving and fails the deploy (manual `kamal rollback`); a **k8s**
Deployment with readiness probes stalls a bad rollout rather than auto-reverting
(manual `kubectl rollout undo`). "Keep, old keeps serving, rollback is opt-in" is
the standard.

**v1 is flag-only — no prompt.** The simplest safe shape:
- Default `keep`; `--on-failure rollback` is the opt-in.
- On failure, **always** print `↩ undo with: ryra revert <svc>` — so even the
  default path makes rollback one copy-paste. `--on-failure rollback` just runs
  that for you.
- Headless falls out for free: omit the flag → `keep`, never hang; `--yes` takes
  `keep` too (a script that didn't ask for rollback must not silently get it).

An interactive `auto-revert on failure? [y/N]` prompt is deliberately deferred:
blue/green already keeps you up, and "print the revert command" already covers
restart, so a prompt on every upgrade is friction for little gain. Add it (and/or
a persistent `preferences.toml` default) only if it's actually wanted — fewer
features first.

### The revert action is strategy-specific

Both strategies revert on fail; what "revert" *does* differs:

| Strategy | "fail" means | keep (default) | rollback |
|----------|--------------|----------------|----------|
| **Restart** | new version won't come up healthy | service left **down** with the new quadlet, for fix-forward | restore last-known-good files + `daemon-reload` + restart |
| **Blue/green** | green fails the health gate | green left **running but unhealthy** to `podman exec` into and debug (no downtime — blue still serves) | stop/remove green, back to blue-live / green-idle |

Note `keep` is arguably *more* useful for blue/green: a started-but-unhealthy
green is a live container you can inspect with zero downtime cost. That's a real
reason to choose No.

### What the toggle can't cover

A swap that **passes** the health gate but is secretly bad. ryra thinks it
succeeded, so nothing triggers. This is where blue/green shines: the old color is
still on disk, so the *manual* rollback is a near-instant pointer flip-back, not
a rebuild. Always `ryra revert` territory, never automatic — nothing told ryra it
failed.

## Companion: blue/green statefulness guard

`podman_color_quadlet` rewrites only `ContainerName=` and `${SERVICE_PORT_*}` —
**not** `Volume=${SERVICE_HOME}/...`. So both colors bind-mount the same data
dir (deliberate: one state, one backup target). That makes blue/green a
**stateless-service** strategy: safe for an app in front of a separate DB, unsafe
for a service that owns local DB state (the swap window runs two versions against
one store; rollback can't un-migrate).

Nothing stops `deploy = "blue-green"` on a stateful monolith today. The catch:
ryra has no *typed* signal for "owns its state" — a volume could be a cache or a
database — so hard validation would need a new `service.toml` field
(e.g. `owns_state = true`), which is exactly the new surface "fewer features"
resists. So **doc-first**: document the stateless contract and, at most, a soft
warning. Add the typed field (and real rejection) only if the doc contract proves
insufficient.

## Companion: optional pre-upgrade snapshot

Availability comes from blue/green; reversibility-of-*state* comes from a
snapshot. The only thing that makes a destructive migration safe is rolling the
*data* back, and `ryra backup` (restic) already does this. Natural pattern:
risky upgrade → snapshot the data dir first → upgrade → restore on failure (or on
user say-so). Closes the one gap file-backups and blue/green can't.

## Security posture

What a current-day infra/security review expects, and how ryra handles it:

- **Image pinning.** The registry *already* pins — zero `:latest` across ~60
  `Image=` lines, specific version tags throughout, `ente` even digest-pins
  (`@sha256:`). Keep it that way with a `ServiceDef::validate()` lint rejecting
  `:latest`/untagged images (boundary validation; codifies an existing norm).
  Digest pinning stays *encouraged* for sensitive images. Signature verification
  (cosign/sigstore) stays **podman-native** (`policy.json`), user-configured —
  ryra documents it, doesn't build it. Pinning also closes the mutable-tag drift
  blind spot: a pinned digest changes the quadlet content, so drift sees it.
- **Rollback can undo a security patch.** If you upgrade to fix a CVE and a flaky
  health check trips rollback, you're silently back on the vulnerable version.
  Default-`keep` already neutralizes this (rollback is opt-in). The only addition
  is *messaging*: an opted-in rollback prints "reverted to the prior version —
  re-apply your fix after resolving the failure." No mechanism.
- **Drift is tamper-detection.** `service.manifest` + `ryra diff` is a lightweight
  integrity tripwire. Every exclusion (`.env`, the manifest, the hook-rewritten
  files) is a small blind spot — keep exclusions to *machine-owned files only*
  (ours are), and ensure `ryra diff` exits non-zero on drift so it's usable from
  cron / monitoring.

## Concurrency

There is **no per-service lock** around upgrade/apply today, so two concurrent
`ryra upgrade <svc>` can race on file writes and `daemon-reload`. The locking
idiom already exists in-tree (`file.lock()` in `registry/fetch.rs`, and the
`.authelia-oidc.lock` in `add.rs`/`remove.rs`) — closing it is a small
`.{svc}.apply.lock` around the mutating path, not new machinery.

## Non-goals / fundamental limits

- **Atomic upgrades.** Not achievable across files + systemd + image pulls.
  Target reversible + self-healing instead.
- **Safe blue/green for databases.** Inherent to the pattern, not fixable in
  code. Use stateless + separate DB, or expand/contract migrations (a discipline
  ryra can't enforce).

## Priority

1. **Per-service apply lock** — smallest, removes a real race; reuses the
   existing `file.lock()` idiom.
2. **`:latest` lint in `validate()`** — one boundary check; locks in the pinning
   norm the registry already follows.
3. **`--on-failure rollback` (flag-only) + auto-revert** — turns the non-atomic
   window from manual recovery into self-healing. Builds on the existing backup
   dir + `revert_service`.
4. **Blue/green statefulness contract** — doc-first; typed `owns_state` only if
   the doc proves insufficient.
5. **Optional pre-upgrade snapshot** — hooks `ryra backup` into the upgrade for
   stateful safety.

Out of scope here, tracked separately: image *signature* verification (podman
`policy.json`), and any cross-machine / fleet concerns (see `backup-fleet.md`).
