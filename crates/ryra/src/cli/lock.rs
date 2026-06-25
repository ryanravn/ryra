//! Global mutation lock — one ryra mutation at a time.
//!
//! Mutations (`add`, `upgrade`, `configure`, `remove`, `revert`) touch shared
//! state — the Caddyfile, `/etc/hosts`, podman networks — that a per-service
//! lock can't protect. A single box has one operator, so the simplest correct
//! guard is one global advisory lock, taken non-blocking: if another ryra
//! mutation already holds it, fail fast (like git's `index.lock`) instead of
//! interleaving writes. `flock` is advisory and self-clearing, so a killed
//! ryra never leaves a stale lock behind.

use std::fs::{OpenOptions, TryLockError};

use anyhow::{Context, Result, bail};
use ryra_core::config::ConfigPaths;

/// Held for the duration of a mutating command; released when dropped (or when
/// the process dies). Bind it to a named variable — `let _ = …` drops it
/// immediately and locks nothing, which is why the type is `#[must_use]`.
#[must_use = "binding the lock to `_` drops it immediately and locks nothing"]
pub struct MutationLock(#[allow(dead_code)] std::fs::File);

impl MutationLock {
    /// Take the global lock for a mutating command, or fail fast if another
    /// ryra mutation holds it. Returns `None` for a `dry_run`: a read-only
    /// preview has nothing to lock and must not block real mutations.
    pub fn acquire(dry_run: bool) -> Result<Option<Self>> {
        if dry_run {
            return Ok(None);
        }
        let paths = ConfigPaths::resolve()?;
        paths.ensure_dirs()?;
        let path = paths.config_dir.join(".ryra.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self(file))),
            // No "delete the lock file" advice: `flock` is released the moment
            // the holding process exits (even on crash/kill), so a WouldBlock
            // always means another ryra is genuinely running right now — there
            // is never a stale file to remove.
            Err(TryLockError::WouldBlock) => {
                bail!("another ryra operation is already running — wait for it to finish and retry")
            }
            Err(TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("lock {}", path.display()))
            }
        }
    }
}
