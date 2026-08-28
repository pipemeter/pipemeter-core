//! Keeping one copy of the mixer running at a time.
//!
//! Two copies is not a cosmetic problem. Both would claim the same virtual
//! sinks, both would push their own idea of every fader down to the same
//! nodes, and both would fight over the routing matrix — the second one to
//! start would silently undo whatever the first had set.
//!
//! Held by an advisory lock on a file in the runtime directory rather than
//! by a pid file. A lock is released by the kernel when the process ends,
//! however it ends, so a crash cannot leave a stale one behind claiming the
//! mixer is already running.

use std::fs::{File, TryLockError};
use std::io;
use std::path::PathBuf;

/// A held claim. Dropping it, or exiting, releases the lock.
#[derive(Debug)]
pub struct Guard {
    // Kept for its lifetime: the lock lives on the open descriptor.
    _file: File,
}

/// Try to claim the mixer.
///
/// `Ok(None)` means another copy already holds it.
///
/// # Errors
///
/// When the claim could not be attempted at all — a read-only runtime
/// directory, say. The caller should carry on rather than refuse to start
/// over a lock file.
pub fn claim() -> io::Result<Option<Guard>> {
    let path = lock_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = File::create(&path)?;

    // Exclusive, and non-blocking so a second copy reports at once rather
    // than hanging until the first quits.
    match file.try_lock() {
        Ok(()) => Ok(Some(Guard { _file: file })),
        // Held by another copy, which is the case this exists for.
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

/// The runtime directory when there is one, falling back to the temporary
/// directory. Either way it is per-user, which is the scope that matters.
fn lock_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map_or_else(std::env::temp_dir, PathBuf::from);
    dir.join("pipemeeter").join("instance.lock")
}

#[cfg(test)]
mod tests {
    use super::lock_path;

    #[test]
    fn the_lock_lives_under_a_directory_of_our_own() {
        let path = lock_path();
        assert_eq!(path.file_name().unwrap(), "instance.lock");
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "pipemeeter");
    }

    #[test]
    fn a_second_claim_in_this_process_still_sees_the_file() {
        // Not a lock test: flock is per open file description, so a second
        // claim here would succeed. This only pins that the path is stable,
        // which is what makes the lock mean anything across processes.
        assert_eq!(lock_path(), lock_path());
    }
}
