//! The one-way move to the flat threads directory (issue #9, step 1).
//!
//! A log used to live at `threads/<project>/<id>.jsonl`, with its
//! directory taken to be its project. The moment a thread switches
//! project that is a lie, so the logs move to `threads/<id>.jsonl` and
//! the project comes from the log (#9, step 8 of the log's own rule).
//! This is the move, not the reader: nothing calls it yet.
//!
//! What it never does: it never rewrites a log, never removes a
//! directory, and never touches any other file — `issue-<n>.lock` and
//! stray files stay where they are.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use ulid::Ulid;

/// The lock that serialises two daemons starting together.
const LOCK: &str = ".migrate.lock";

/// What one [`migrate`] did.
#[derive(Debug, Default, Clone)]
pub struct Migrated {
    /// Logs renamed to `<base>/<id>.jsonl`.
    pub moved: usize,
    /// Ids left in a legacy directory where another process held a
    /// lock (a lead being driven, or an issue's check-and-create): the
    /// whole directory stays, logs and locks, until a later daemon start
    /// finds every lock free. A lead's step children have no lock of
    /// their own, so moving any log there could split a live one.
    pub held: Vec<Ulid>,
    /// Ids that already had a flat log: the legacy file is left alone
    /// and the flat one wins.
    pub clashes: Vec<Ulid>,
}

/// Move every legacy `threads/<project>/<id>.jsonl` to
/// `threads/<id>.jsonl`.
///
/// A missing base is nothing to do. Otherwise two callers on one base
/// migrate one after the other, on a blocking lock so neither errors.
pub fn migrate(base: &Path) -> io::Result<Migrated> {
    let mut out = Migrated::default();
    if !base.is_dir() {
        return Ok(out);
    }
    // Held to the end of the function: the lock is released on drop.
    let _serialise = lock_for_update(&base.join(LOCK))?;

    for sub in legacy_dirs(base)? {
        // Every lock in the directory first (#9 review): a lead's step
        // children have no lock of their own and are written beside it,
        // so while any lock here is held — a lead being driven, or an
        // issue's check-and-create — the whole directory is left for a
        // later start. Moving one of its logs would let the live writer
        // recreate it at the old path, splitting it in two.
        let mut free: Vec<(Ulid, File)> = Vec::new();
        let mut busy = false;
        for (stem, lock_path) in locks_in(&sub)? {
            match take_lead_lock(&lock_path)? {
                Lead::None => {}
                Lead::Held => {
                    busy = true;
                    break;
                }
                Lead::Free(file) => {
                    if let Ok(id) = stem.parse::<Ulid>() {
                        free.push((id, file));
                    }
                }
            }
        }
        let logs = legacy_logs(&sub)?;
        if busy {
            out.held.extend(logs);
            continue;
        }
        for id in logs {
            let flat = base.join(format!("{id}.jsonl"));
            if flat.exists() {
                // The flat log wins; the legacy file stays put.
                out.clashes.push(id);
                continue;
            }
            match std::fs::rename(sub.join(format!("{id}.jsonl")), &flat) {
                Ok(()) => out.moved += 1,
                // Raced away by another caller: already moved.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            // A lead's lock goes with its log; we hold it, so no one
            // else is between the check and the removal.
            if free.iter().any(|(lead, _)| *lead == id) {
                let _ = std::fs::remove_file(sub.join(format!("{id}.lock")));
            }
        }
        // The free locks are released here, as `free` drops.
    }
    Ok(out)
}

/// The lock a lead's own driver holds (`runs::LeadLock`), as this file
/// sees it.
enum Lead {
    /// There is no `<id>.lock`.
    None,
    /// The lock is free; this caller holds it now.
    Free(File),
    /// Someone else holds it. `WouldBlock`, or a lock the OS refused to
    /// inspect — either way the log is not ours to move.
    Held,
}

/// Open (creating it) `path` and lock it, blocking.
fn lock_for_update(path: &Path) -> io::Result<File> {
    let file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    file.lock()?;
    Ok(file)
}

/// Take `path`'s lock if it is there, or say who holds it.
fn take_lead_lock(path: &Path) -> io::Result<Lead> {
    let file = match File::options().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Lead::None),
        Err(e) => return Err(e),
    };
    match file.try_lock() {
        Ok(()) => Ok(Lead::Free(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(Lead::Held),
        // A lock we cannot even inspect is not ours to move a log out
        // from under. Leave it, the way a held one is left.
        Err(std::fs::TryLockError::Error(_)) => Ok(Lead::Held),
    }
}

/// Every `<stem>.lock` in `dir`, in name order: a lead's
/// `<ulid>.lock` and an issue's `issue-<n>.lock` alike.
fn locks_in(dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(stem) = name.strip_suffix(".lock") {
            out.push((stem.to_owned(), entry.path()));
        }
    }
    out.sort();
    Ok(out)
}

/// Every subdirectory of `base` whose name doesn't start with `.`, in
/// name order. A dot-named one is left alone, logs and all.
fn legacy_dirs(base: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(base)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        out.push(entry.path());
    }
    out.sort();
    Ok(out)
}

/// Every `<ulid>.jsonl` in `dir`, in id order.
fn legacy_logs(dir: &Path) -> io::Result<Vec<Ulid>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let Ok(id) = stem.parse::<Ulid>() else {
            continue;
        };
        out.push(id);
    }
    out.sort();
    Ok(out)
}
