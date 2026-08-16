//! A process-level lock on a directory: a PID-bearing lock file created
//! with `create_new`, reaped when its owner is provably dead. Guards
//! state a single process may write, e.g. a node's `.choir` dir, where
//! two writers appending one `ops.jsonl` would fork the chain.
//!
//! Ported from Oak `cli/src/workdir_lock.rs` (v0.102.1, commit
//! `8de9515`, Apache-2.0); error type localized to [`LockError`].

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Why a [`WorkdirLock`] could not be acquired.
#[derive(Debug)]
pub enum LockError {
    /// Another live process holds the lock.
    Locked,
    /// The lock file could not be created, read, or removed.
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Locked => write!(f, "directory is locked by another live process"),
            LockError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LockError {}

impl From<std::io::Error> for LockError {
    fn from(e: std::io::Error) -> Self {
        LockError::Io(e)
    }
}

/// Exclusive lock on a directory, released on drop.
pub struct WorkdirLock {
    lock_path: PathBuf,
}

impl WorkdirLock {
    /// Acquire the lock, reaping a lock file whose recorded PID is no
    /// longer alive. Fails with [`LockError::Locked`] if a live process
    /// holds it.
    pub fn acquire(dir: &Path) -> Result<Self, LockError> {
        fs::create_dir_all(dir)?;
        let lock_path = dir.join("wdlock");
        let pid = std::process::id();

        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut file) => {
                    if let Err(e) = write!(file, "{pid}").and_then(|()| file.sync_all()) {
                        let _ = fs::remove_file(&lock_path);
                        return Err(LockError::Io(e));
                    }
                    return Ok(WorkdirLock { lock_path });
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    if let Some(snapshot) = stale_lock_snapshot(&lock_path)? {
                        // Reap only if the file is still byte-identical to
                        // what we judged stale; either way, loop so
                        // create_new gets the next word.
                        remove_if_unchanged(&lock_path, &snapshot)?;
                        continue;
                    }
                    return Err(LockError::Locked);
                }
                Err(e) => return Err(LockError::Io(e)),
            }
        }
    }

    /// Acquire, waiting up to `timeout` while a live process holds the
    /// lock, so short concurrent writers don't force callers into
    /// process-level retries.
    pub fn acquire_wait(dir: &Path, timeout: Duration) -> Result<Self, LockError> {
        let start = Instant::now();
        loop {
            match Self::acquire(dir) {
                Ok(lock) => return Ok(lock),
                Err(LockError::Locked) if start.elapsed() < timeout => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(err) => return Err(err),
            }
        }
    }
}

impl Drop for WorkdirLock {
    fn drop(&mut self) {
        // Remove only if this process still owns it: a reaper may have
        // replaced the file after e.g. a laptop sleep.
        if let Ok(contents) = fs::read_to_string(&self.lock_path) {
            if contents.trim() == std::process::id().to_string() {
                let _ = fs::remove_file(&self.lock_path);
            }
        }
    }
}

#[derive(Debug)]
struct LockSnapshot {
    contents: String,
    len: u64,
    modified: Option<SystemTime>,
}

/// `Some(snapshot)` if the lock file looks reapable: missing, owned by a
/// dead PID, or malformed for over 30 seconds (a crashed partial
/// acquisition; a *fresh* malformed file may be a live contender that
/// created the file but has not written its PID yet).
fn stale_lock_snapshot(lock_path: &Path) -> Result<Option<LockSnapshot>, LockError> {
    let gone = || LockSnapshot {
        contents: String::new(),
        len: 0,
        modified: None,
    };
    let meta = match fs::metadata(lock_path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Some(gone())),
        Err(e) => return Err(LockError::Io(e)),
    };
    let contents = match fs::read_to_string(lock_path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Some(gone())),
        Err(e) => return Err(LockError::Io(e)),
    };
    let snapshot = LockSnapshot {
        len: meta.len(),
        modified: meta.modified().ok(),
        contents,
    };
    let Ok(pid) = snapshot.contents.trim().parse::<u32>() else {
        return Ok(lock_age(lock_path)
            .is_some_and(|age| age > Duration::from_secs(30))
            .then_some(snapshot));
    };
    Ok((!is_process_alive(pid)).then_some(snapshot))
}

/// Remove the lock file if it still matches `snapshot`; a mismatch means
/// another process re-created it between our read and now, so it is
/// theirs, not stale.
fn remove_if_unchanged(lock_path: &Path, snapshot: &LockSnapshot) -> Result<(), LockError> {
    if snapshot.modified.is_none() && snapshot.contents.is_empty() {
        return Ok(());
    }
    let meta = match fs::metadata(lock_path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(LockError::Io(e)),
    };
    let contents = match fs::read_to_string(lock_path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(LockError::Io(e)),
    };
    if contents != snapshot.contents
        || meta.len() != snapshot.len
        || meta.modified().ok() != snapshot.modified
    {
        return Ok(());
    }
    match fs::remove_file(lock_path) {
        Ok(()) | Err(_) => Ok(()),
    }
}

fn lock_age(lock_path: &Path) -> Option<Duration> {
    fs::metadata(lock_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
}

fn is_process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(target_os = "macos")]
    {
        // kill -0: exit 0 iff the process exists (or EPERM, also alive).
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        // No liveness probe: fail safe by treating the holder as alive
        // rather than admitting a second writer.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("choir-fs-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn second_acquire_is_refused_and_release_frees() {
        let dir = scratch("lock-basic");
        let lock = WorkdirLock::acquire(&dir).unwrap();
        assert!(dir.join("wdlock").is_file());
        match WorkdirLock::acquire(&dir) {
            Err(LockError::Locked) => {}
            other => panic!("expected Locked, got {:?}", other.map(|_| ())),
        }
        drop(lock);
        assert!(!dir.join("wdlock").exists());
        let _relock = WorkdirLock::acquire(&dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dead_owners_lock_is_reaped() {
        let dir = scratch("lock-reap");
        // A real PID that is provably dead: a child we already waited on.
        let dead = std::process::Command::new("true")
            .status()
            .map(|_| ())
            .and_then(|()| {
                let child = std::process::Command::new("true").spawn()?;
                let pid = child.id();
                child.wait_with_output()?;
                Ok(pid)
            })
            .unwrap();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("wdlock"), dead.to_string()).unwrap();

        let _lock = WorkdirLock::acquire(&dir).expect("dead owner's lock reaped");
        assert_eq!(
            fs::read_to_string(dir.join("wdlock")).unwrap().trim(),
            std::process::id().to_string()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fresh_malformed_lock_is_respected() {
        // A just-created file with no parseable PID may be a live
        // contender mid-acquisition; it must read as locked, not stale.
        let dir = scratch("lock-malformed");
        fs::write(dir.join("wdlock"), "not-a-pid").unwrap();
        match WorkdirLock::acquire(&dir) {
            Err(LockError::Locked) => {}
            other => panic!("expected Locked, got {:?}", other.map(|_| ())),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn acquire_wait_gets_the_lock_once_the_holder_releases() {
        let dir = scratch("lock-wait");
        let held = WorkdirLock::acquire(&dir).unwrap();
        let dir2 = dir.clone();
        let waiter =
            thread::spawn(move || WorkdirLock::acquire_wait(&dir2, Duration::from_secs(5)).is_ok());
        thread::sleep(Duration::from_millis(30));
        drop(held);
        assert!(waiter.join().unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }
}
