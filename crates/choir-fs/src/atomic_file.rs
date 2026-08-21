//! Atomic file replacement: write a temp file in the destination
//! directory, sync it, rename it into place, sync the parent directory.
//! Readers see the old contents or the new, never a torn write, and a
//! crash between any two steps leaves the destination untouched.
//!
//! Ported from Oak `cli/src/atomic_file.rs` (v0.102.1, commit `8de9515`,
//! Apache-2.0); error type changed to `std::io::Error`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Atomically replace `path` with `contents`.
///
/// # Examples
///
/// The destination either holds the old bytes or the new ones. A reader
/// racing the write never sees a prefix of the new contents, which is the
/// whole reason this is not `fs::write`.
///
/// ```
/// # use std::fs;
/// let dir = std::env::temp_dir().join("choir-fs-doctest-write-atomic");
/// fs::create_dir_all(&dir)?;
/// let path = dir.join("policy");
///
/// choir_fs::atomic_file::write_atomic(&path, "first\n")?;
/// assert_eq!(fs::read_to_string(&path)?, "first\n");
///
/// // Replacing is one rename, not a truncate-then-write.
/// choir_fs::atomic_file::write_atomic(&path, "second\n")?;
/// assert_eq!(fs::read_to_string(&path)?, "second\n");
///
/// // Nothing is left behind in the directory the temp file was written to.
/// let strays: Vec<_> = fs::read_dir(&dir)?
///     .filter_map(Result::ok)
///     .filter(|e| e.file_name() != "policy")
///     .collect();
/// assert!(strays.is_empty(), "temp file survived the rename");
/// # fs::remove_dir_all(&dir)?;
/// # Ok::<(), std::io::Error>(())
/// ```
///
/// # Errors
///
/// Any `io::Error` from creating, writing, syncing or renaming the
/// replacement file, and `InvalidInput` when `path` has no parent
/// directory to write the replacement into.
pub fn write_atomic(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    write_atomic_impl(path, contents, false)
}

/// Atomically replace `path` with `contents`, creating the replacement
/// file with owner-only permissions (0600) before any contents are
/// written on Unix. For secrets: unlike write-then-chmod there is no
/// window where the bytes exist world-readable.
pub fn write_atomic_private(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    write_atomic_impl(path, contents, true)
}

fn write_atomic_impl(path: &Path, contents: impl AsRef<[u8]>, private: bool) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write path has no parent",
        )
    })?;
    fs::create_dir_all(parent)?;

    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write path has no file name",
        )
    })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp_name = format!(
        ".{}.tmp-{}-{nonce}",
        file_name.to_string_lossy(),
        std::process::id()
    );
    let tmp_path = path.with_file_name(tmp_name);

    let write_result = (|| -> io::Result<()> {
        let mut file = create_temp_file(&tmp_path, private)?;
        file.write_all(contents.as_ref())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp_path, path)?;
        sync_parent_dir(parent)?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_result
}

fn create_temp_file(path: &Path, private: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        let file = options.open(path)?;
        // mode() is masked by the process umask; assert 0600 outright.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        return Ok(file);
    }
    let _ = private;
    options.open(path)
}

#[cfg(unix)]
fn sync_parent_dir(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_parent: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("choir-fs-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn creates_replaces_and_leaves_no_temp_files() {
        let dir = scratch("atomic");
        let path = dir.join("state.txt");

        write_atomic(&path, "first").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "first");

        write_atomic(&path, "second").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second");

        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "left temp files behind: {leftovers:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn private_writes_are_owner_only_even_when_replacing_a_readable_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch("private");
        let path = dir.join("secret");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        write_atomic_private(&path, "new").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_parent_directories_are_created() {
        let dir = scratch("mkdirs");
        let path = dir.join("a/b/state.txt");
        write_atomic(&path, "deep").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "deep");
        std::fs::remove_dir_all(&dir).ok();
    }
}
