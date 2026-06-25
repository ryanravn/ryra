//! Global mutation lock — one ryra mutation at a time.
//!
//! Mutations (`add`, `upgrade`, `configure`, `remove`, `revert`) touch shared
//! state — the Caddyfile, `/etc/hosts`, podman networks — that a per-service
//! lock can't protect. A single box has one operator, so the simplest correct
//! guard is one global advisory lock, taken non-blocking: if another ryra
//! mutation already holds it, fail fast (like git's `index.lock`) instead of
//! interleaving writes. `flock` is advisory and self-clearing, so a killed
//! ryra never leaves a stale lock behind.
//!
//! The lock is **re-entrant within a single process**: a mutating command may
//! invoke another in-process (e.g. `add --smtp=inbucket` installs inbucket via
//! a nested `add`, and `add --auth` may install a provider). The nested call
//! must reuse the lock this process already holds rather than dead-failing on
//! its own `flock`. We track nesting in a process-global depth counter and only
//! take/release the underlying `flock` at depth 0. ryra never runs two
//! mutations concurrently within one process, so a plain counter is sufficient;
//! a genuinely separate ryra process still contends on the `flock` as before.

use std::fs::{OpenOptions, TryLockError};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};
use ryra_core::config::ConfigPaths;

/// Process-global nesting depth. The real `flock` is held only while this is
/// `> 0`; nested acquires within the same process reuse it instead of
/// re-locking (which would `WouldBlock` against ourselves).
static LOCK_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// Held for the duration of a mutating command; released when dropped (or when
/// the process dies). Bind it to a named variable — `let _ = …` drops it
/// immediately and locks nothing, which is why the type is `#[must_use]`.
///
/// `Some(file)` is the outer guard that owns the `flock`; `None` is a
/// re-entrant inner guard that only unwinds the depth counter on drop.
#[must_use = "binding the lock to `_` drops it immediately and locks nothing"]
pub struct MutationLock(#[allow(dead_code)] Option<std::fs::File>);

impl MutationLock {
    /// Take the global lock for a mutating command, or fail fast if another
    /// ryra *process* holds it. Returns `None` for a `dry_run`: a read-only
    /// preview has nothing to lock and must not block real mutations.
    ///
    /// Re-entrant: if this process already holds the lock (a mutating command
    /// invoked another), the nested acquire succeeds without re-locking.
    pub fn acquire(dry_run: bool) -> Result<Option<Self>> {
        if dry_run {
            return Ok(None);
        }
        // Already held by THIS process (nested mutation) — reuse it instead of
        // blocking on our own flock.
        if LOCK_DEPTH.fetch_add(1, Ordering::SeqCst) > 0 {
            return Ok(Some(Self(None)));
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
            Ok(()) => Ok(Some(Self(Some(file)))),
            // No "delete the lock file" advice: `flock` is released the moment
            // the holding process exits (even on crash/kill), so a WouldBlock
            // always means another ryra is genuinely running right now — there
            // is never a stale file to remove.
            Err(TryLockError::WouldBlock) => {
                LOCK_DEPTH.fetch_sub(1, Ordering::SeqCst);
                bail!("another ryra operation is already running — wait for it to finish and retry")
            }
            Err(TryLockError::Error(e)) => {
                LOCK_DEPTH.fetch_sub(1, Ordering::SeqCst);
                Err(e).with_context(|| format!("lock {}", path.display()))
            }
        }
    }
}

impl Drop for MutationLock {
    fn drop(&mut self) {
        // Unwind one level of nesting. The underlying `flock` (owned by the
        // outer `Some(file)` guard) is released when its `File` drops here at
        // depth 0; inner `None` guards just decrement the counter.
        LOCK_DEPTH.fetch_sub(1, Ordering::SeqCst);
    }
}
