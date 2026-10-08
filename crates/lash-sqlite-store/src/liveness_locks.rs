//! Liveness locks beside a file database: SQLite's stand-in for the session
//! advisory locks PostgreSQL holds while a node's listener or a sweep pass
//! lives (FIG-5422).
//!
//! Each lock is one file in `<database>-liveness/`, locked through std's file
//! locks, which are `flock(2)` locks on Linux and macOS: a lock belongs to the
//! open file description, so a probe in the holder's own process, through a
//! descriptor of its own, sees the lock held, and the kernel drops it when the
//! holder closes it or its process dies, however it dies. POSIX record locks
//! (`fcntl`) would not do: they belong to the process, so a probe in the
//! holder's process sees its own lock as free, and closing any descriptor of
//! the file drops them.
//!
//! - A **holder** takes its lock exclusively and keeps it for as long as it
//!   lives.
//! - A **probe** takes it shared and lets it go: probes never see each other
//!   as holders. A probe that finds it free may keep it, so the holder cannot
//!   take it again until the probe's transaction ends.
//! - A file nobody locks is the same answer as no file: free. A free lock's
//!   file may be deleted by anyone who takes it exclusively, so a holder
//!   checks, once it has its lock, that its path still names the file it
//!   locked, and takes it again otherwise.
//!
//! The directory must be on a local filesystem, as the database itself must:
//! a network filesystem emulates or ignores `flock`.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// The suffix of the lock directory, named after its database the way SQLite
/// names its `-wal` and `-shm` files.
const DIR_SUFFIX: &str = "-liveness";

/// The extension of every lock file.
const EXTENSION: &str = "lock";

/// How many times a holder takes its lock again after finding its path
/// deleted under it before it reports the lock as held elsewhere.
const HOLD_ATTEMPTS: usize = 8;

/// The liveness locks of one file database.
#[derive(Clone, Debug)]
pub(crate) struct LivenessLocks {
    dir: PathBuf,
}

/// What a probe saw.
pub(crate) enum Probed {
    /// Some holder holds the lock.
    Held,
    /// Nobody holds it. The probe keeps it shared, if its file exists, until
    /// this guard drops.
    Free(Option<ProbeGuard>),
}

/// A held liveness lock: exclusive until it drops, or until
/// [`HeldLock::release`] deletes its file.
#[derive(Debug)]
pub(crate) struct HeldLock {
    file: File,
    path: PathBuf,
}

/// A free lock a probe keeps shared.
#[derive(Debug)]
pub(crate) struct ProbeGuard {
    file: File,
    path: PathBuf,
}

impl LivenessLocks {
    /// The liveness locks of the database at `database`.
    pub(crate) fn beside(database: &Path) -> Self {
        let mut dir = database.as_os_str().to_owned();
        dir.push(DIR_SUFFIX);
        Self {
            dir: PathBuf::from(dir),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.{EXTENSION}"))
    }

    /// Take `name` exclusively, unless some other holder has it.
    #[expect(
        clippy::disallowed_methods,
        reason = "liveness locks are files beside the host-supplied database file"
    )]
    pub(crate) fn try_hold(&self, name: &str) -> io::Result<Option<HeldLock>> {
        let path = self.path(name);
        for _ in 0..HOLD_ATTEMPTS {
            std::fs::create_dir_all(&self.dir)?;
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)?;
            match file.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => return Ok(None),
                Err(TryLockError::Error(error)) => return Err(error),
            }
            let held = HeldLock {
                file,
                path: path.clone(),
            };
            // A free lock's file may have been deleted between the open and
            // the lock: a lock on it would be one no probe can find.
            if held.intact() {
                return Ok(Some(held));
            }
        }
        Ok(None)
    }

    /// Whether `name` is held now.
    #[expect(
        clippy::disallowed_methods,
        reason = "liveness locks are files beside the host-supplied database file"
    )]
    pub(crate) fn probe(&self, name: &str) -> io::Result<Probed> {
        let path = self.path(name);
        let file = match OpenOptions::new().read(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Probed::Free(None));
            }
            Err(error) => return Err(error),
        };
        match file.try_lock_shared() {
            Ok(()) => Ok(Probed::Free(Some(ProbeGuard { file, path }))),
            Err(TryLockError::WouldBlock) => Ok(Probed::Held),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }

    /// Delete the file of every free lock whose name starts with `prefix`:
    /// what holders that died or were never released left behind.
    #[expect(
        clippy::disallowed_methods,
        reason = "liveness locks are files beside the host-supplied database file"
    )]
    pub(crate) fn sweep(&self, prefix: &str) -> io::Result<()> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let path = entry?.path();
            let ours = path
                .extension()
                .is_some_and(|extension| extension == EXTENSION)
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.starts_with(prefix));
            if !ours {
                continue;
            }
            let Ok(file) = OpenOptions::new().write(true).open(&path) else {
                continue;
            };
            if file.try_lock().is_ok() {
                HeldLock { file, path }.release();
            }
        }
        Ok(())
    }
}

/// Whether `path` names the file `file` has open.
#[expect(
    clippy::disallowed_methods,
    reason = "liveness locks are files beside the host-supplied database file"
)]
fn names(path: &Path, file: &File) -> bool {
    let (Ok(named), Ok(open)) = (std::fs::metadata(path), file.metadata()) else {
        return false;
    };
    same_file(&named, &open)
}

#[cfg(unix)]
fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(_: &std::fs::Metadata, _: &std::fs::Metadata) -> bool {
    true
}

/// Delete `path` while `file`, which it names, is locked: nobody can hold it
/// in between, so no holder's lock is lost to the deletion.
#[expect(
    clippy::disallowed_methods,
    reason = "liveness locks are files beside the host-supplied database file"
)]
fn delete_while_locked(path: &Path, file: &File) {
    if names(path, file) {
        let _ = std::fs::remove_file(path);
    }
}

impl HeldLock {
    /// Whether the lock's path still names the file this holder locked: a
    /// lock whose file was deleted is one no probe can see.
    pub(crate) fn intact(&self) -> bool {
        names(&self.path, &self.file)
    }

    /// Give the lock up and delete its file.
    pub(crate) fn release(self) {
        delete_while_locked(&self.path, &self.file);
    }
}

impl ProbeGuard {
    /// Delete the free lock's file, then let it go: its holder is gone for
    /// good.
    pub(crate) fn delete(self) {
        delete_while_locked(&self.path, &self.file);
    }
}

#[cfg(test)]
impl LivenessLocks {
    /// Delete `name`'s file whoever holds it.
    #[expect(
        clippy::disallowed_methods,
        reason = "test fixture: delete a held lock's file under its holder"
    )]
    pub(crate) fn delete_for_testing(&self, name: &str) {
        let _ = std::fs::remove_file(self.path(name));
    }
}
