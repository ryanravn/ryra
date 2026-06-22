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

## The model: one cadence + a retention ladder (NOT two schedules)

The flexible-but-simple answer (restic's model) keeps two knobs independent:

- **Schedule = how often you snapshot.** Pick the finest granularity you care
  about (hourly / daily / weekly). ONE job — `ryra backup run` on a timer, plus
  manual `ryra backup run` any time. We do NOT take "1,2,3 at once"; one snapshot
  per run.
- **Retention ladder = how much history to keep at each level.**
  `keep-last N` (safety floor) + `keep-hourly/daily/weekly/monthly/yearly`. A
  snapshot survives if ANY rule keeps it (a union — conservative).

So "keep 3 last days + 3 last weeks" is a **daily** schedule + `keep-daily 3` +
`keep-weekly 3`: restic promotes the daily snapshots into the weekly slots; you
never schedule a separate weekly job. "Keep N daily backups" is just
`keep-daily N`. This composes to anything (hourly bursts, yearly archives)
without a cron DSL.

**The CLI manages the WHOLE fleet — every command has a target machine.** The
unifying rule: a `ryra backup` command always acts on ONE machine, and the
target is:
- **auto-derived from the hostname by default** — so on a box you just run
  `ryra backup list` / `run` / `forget` and it means THIS box, no flag, no
  network round-trip for its own ops; and
- **chosen with `--machine <name>`** — to manage any other machine in your
  account from anywhere (a laptop, or one box managing another). When the
  hostname isn't one of your machines (e.g. a laptop), `--machine` is required.
- **`--machine all`** — fleet-wide aggregate for read views (`list`, `status`):
  every machine's restore points in one table (machine + service columns).

Remote-target ops (`--machine` != local) go through the control plane (ryra-api
fleet endpoints), which resolves the name to the owned machine and routes the
rpc to that box. Local-target ops run the engine directly. The dashboard is a
GUI over the SAME fleet endpoints — CLI and dashboard are peers.

CLI shape (both modes):
- `ryra backup schedule <hourly|daily|weekly>` — cadence (one job; one snapshot
  per run; manual `ryra backup run` any time).
- `ryra backup retention --keep-last N --keep-hourly N --keep-daily N
  --keep-weekly N --keep-monthly N --keep-yearly N` (set) / no-args (show) — the
  ladder. "3 last days + 3 last weeks" = daily schedule + `--keep-daily 3
  --keep-weekly 3`.
- `ryra backup list [--machine X]`, `forget [--machine X]`, `status` — retention
  shown in status.

Engine has the ladder (last/daily/weekly/monthly); REMAINING UX: hourly+yearly
knobs, the retention set/show command, status display, and the fleet endpoints +
`--machine`/aggregation wiring (Phase 2-B).

## Machine identity (identity != name)

Backups must NOT key off the hostname: hostnames are mutable (rename = backups
appear to move) and collide (ten boxes called `debian`). Split the two:

- **`machine_id` — the ONLY persisted thing; stable, source of truth.** Minted
  ONCE and stored in `preferences.toml` as `[machine] { id }`; it IS the
  per-machine bucket prefix and is never derived from mutable state.
  - Managed: the orchestrator's machine id (a UUID, injected as
    `RYRA_MACHINE_ID`) — persist it on the box so it's local truth too.
  - Self-host: a fresh UUID minted on first `ryra backup config`, persisted.
- **`label` — human display, mutable, NOT persisted on the box.** It's the
  hostname, read at backup time and stamped as the snapshot's `--host`. Renaming
  never moves backups (the folder/id is untouched); future snapshots just carry
  the new name. Display labels are read from where they already live — managed:
  the control-plane DB (`machine.name`); self-host: the snapshots' Host field.
  No third copy to keep in sync. `--machine` matches the label; the fleet view
  disambiguates with the short id when labels collide.

Consequences:
- Rename the host -> backups unaffected (prefix = the id). No "change here,
  breaks there".
- Same-label machines are fine (ids are unique).
- Self-host works with no control plane: the id is local, and a shared bucket's
  PREFIXES are the machine registry -- `list --machine all` discovers machines
  by listing prefixes. (Managed uses the control-plane registry + SSH routing.)
- The CLI's default target = the LOCAL persisted id (NOT the hostname).
- Re-adopting backups (reinstall / clone): point a new box's `machine_id` at an
  existing prefix to continue its history -- ties into cross-machine restore.

Replaces the current `machine_prefix()` hostname fallback (ryra-api) with the
persisted id. TEST: rename hostname -> same prefix; two same-label boxes ->
distinct prefixes; self-host id survives across reinstall when preferences.toml
is kept.

**How id + label are stored (NOT `{id}__{label}` folders).** The folder name
would couple the mutable label to the data location -- rename = move the repo.
Instead:
- **Folder/prefix = `{id}` only** (immutable). restic's repo lives there.
- **Label rides inside the backup as restic metadata**: each snapshot is taken
  with `--host {label}` + `--tag machine_id:{id}` (alongside `service:{svc}`).
  `restic snapshots` already shows Host + Tags, so the label surfaces natively;
  a rename only changes future snapshots' host, never the folder.
- **Persisted on the box**: `preferences.toml` `[machine] { id }` only — the id
  is the sole local source of truth; the label is the hostname (stamped per
  snapshot), not stored.
- **Self-describing for DR/discovery**: with only the bucket, list `{id}/`
  prefixes (registry) and read each repo's snapshots for the `machine_id` tag +
  label. Managed also keeps id<->label in the control-plane DB. Optional: a
  plaintext `{id}/machine.json` marker so labels show from a password-less
  `mc ls` too.

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
