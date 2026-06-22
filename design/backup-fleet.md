# Backup: retention, restore points, and the fleet

Status: in progress (branch `feat/backup-fleet`). Spans three repos; **ryra (the
engine) is built first** because the others build on it.

## Why

A customer should be able to pick **basically anything**: any machine they own
(a ryra-cloud box or a Raspberry Pi they connected), any **restore point** (an
hour ago, a week ago, a month ago, a specific manual snapshot), per service, and
ideally restore one machine's data onto another. Today we have a strong
per-machine foundation but no retention, no fleet view, and no cross-machine
restore.

## What already exists (don't rebuild)

- **Multiple restore points** — restic snapshots. `Snapshots { service }` RPC
  lists them; the box dashboard already renders a snapshot picker and restores a
  chosen one (`ryra-api web/src/main.rs`).
- **Point-in-time restore** — `Restore { service, snapshot }` ("latest" or a
  specific id); CLI `restore --at <id>` (`ryra/src/cli/backup.rs`).
- **Manual + scheduled** — `backup run` on demand; systemd `--user` timers for
  hourly / daily / weekly.
- **Per-service enrollment** — `SetBackupEnrolled`; `backup_enabled` metadata.
- **Managed (ryra-vended R2) and custom (own S3)** backends.
- **Unified machine model** — cloud (Azure) and self-hosted (Pi, BYO) are the
  same `Machine` (`orchestrator/machines.rs`); both back up the same way.
- **One account bucket, per-machine prefixes** — every machine writes under its
  own prefix in one shared account bucket (`ryra-api backup.rs machine_prefix`).
  This is the key enabler: **all of an account's backup data already lives in
  one place** — it just isn't surfaced across machines yet.

## Gaps

| Gap | State | Phase |
|-----|-------|-------|
| Retention / `restic forget` + `prune` | MISSING (snapshots grow forever) | 1 (ryra) |
| Fleet view (all machines' restore points in one place) | MISSING (box-local only) | 2 (api + orc) |
| Cross-machine restore (machine A's data onto B) | MISSING (`Restore` has no source target) | 3 (ryra + api) |
| Custom schedule cadence / per-service schedules | PARTIAL (fixed hourly/daily/weekly) | minor |
| File-level restore (a path within a snapshot) | PARTIAL (whole-service only) | minor |

## Phase 1 — Retention (this branch, ryra core)

Snapshots currently accumulate forever: storage cost creeps and there's no
policy guaranteeing a clean "daily for a week, weekly for a month, monthly for
longer" ladder. restic's `forget`/`prune` do exactly this; we don't expose them.

**Config** — add an optional retention policy to the `[backup]` section
(`config.backup`). Absent → today's behaviour (no auto-forget); present → applied
after scheduled runs.

```toml
[backup.retention]
keep_last    = 3   # always keep the N most recent, regardless of age
keep_daily   = 7
keep_weekly  = 4
keep_monthly = 6
```

Default policy (used when a managed backup is configured): `last=3, daily=7,
weekly=4, monthly=6`. Conservative; never deletes anything still inside the
ladder.

**Engine** (`ryra-core/src/backup.rs`) — a `BackupForgetPlan` and
`plan_backup_forget(config, repo_dir, service: Option<&str>)`, mirroring
`plan_backup_run`. It carries the repo, password, env, the `--keep-*` flags, and
the `service:<name>` tag so forget is grouped/scoped per service (snapshots are
already tagged `service:<name>` + `manifest_sha:<16>`). The CLI spawns
`restic forget --group-by tags --tag service:<name> --keep-* [--prune] [--dry-run]`.

**Protocol** (`ryra-protocol`) — `ForgetBackups { service: Option<String>,
dry_run: bool }` (None = every enrolled service). Lets the control plane and the
dashboard trigger/preview retention later.

**CLI** (`ryra/src/cli/backup.rs`) — `ryra backup forget [service] [--dry-run]`,
and the scheduled `backup run` applies retention afterward when a policy is set.
`--dry-run` prints what restic *would* remove (`restic forget --dry-run`), so a
customer can preview before pruning.

**Safety**: opt-in via config (with a sane default on managed setups), `--dry-run`
everywhere, per-service grouping so one service's policy can't evict another's,
`forget` (mark) is reversible until `prune` (reclaim) runs.

## Phase 2 — Fleet view (ryra-api + orchestrator)

One place to see every machine's restore points. The central control plane
already SSHes into each machine, so v1 **fans out over that existing seam**:
list the account's machines (`orchestrator/machines.rs`) → for each, query its
`Snapshots`/`BackupStatus` over the box rpc → present a unified per-account view
(machine -> service -> restore points). No new sync infra. A cached snapshot
index (boxes report after each run) is a later optimisation if fan-out latency
hurts.

## Phase 3 — Cross-machine restore (ryra + ryra-api)

Because all machines share one account bucket with per-machine prefixes, machine
B can already *read* machine A's prefix with account-scoped creds. Work: let
`Restore` accept a **source** (machine prefix + snapshot) distinct from the local
target; vend creds scoped to read the source prefix; dashboard UI to pick source
machine + snapshot -> target machine. Unlocks "migrate my Pi onto a fresh cloud
box" and clone/duplicate.

## Sequencing

1. **ryra**: Phase 1 retention (this branch) — config + engine + protocol + CLI.
2. **ryra-api + orchestrator**: Phase 2 fleet view; surface retention in the
   dashboard backup panel.
3. **ryra + ryra-api**: Phase 3 cross-machine restore.

Minor items (custom cadence, file-level restore) slot in opportunistically.
