//! Shared version checks and per-path, cross-process atomic file replacement.

use std::fmt::Write as _;
use std::fs::{File, OpenOptions, Permissions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ring::digest::{Context, SHA256};
use tokio_util::sync::CancellationToken;

use crate::ToolError;

pub(super) const MISSING: &str = "missing";

pub(super) fn failure(path: &Path, error: impl std::fmt::Display) -> ToolError {
    ToolError::Failed(format!("{}: {error}", path.display()))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub(super) fn version(bytes: &[u8]) -> String {
    format!(
        "sha256:{}",
        hex(ring::digest::digest(&SHA256, bytes).as_ref())
    )
}

pub(super) fn file_version(file: &mut File) -> std::io::Result<String> {
    let mut digest = Context::new(&SHA256);
    let mut buffer = [0; 8 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("sha256:{}", hex(digest.finish().as_ref())))
}

pub(super) fn target_path(cwd: &Path, name: &str) -> Result<PathBuf, ToolError> {
    let path = cwd.join(name);
    // Follow valid symlinks, but never replace a dangling symlink with a new file.
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink()) {
        path.canonicalize().map_err(|e| failure(&path, e))?;
    }
    Ok(crate::executor::lock_path(cwd, Path::new(name)))
}

/// Lock files persist: unlinking them would let two processes lock different inodes.
#[derive(Debug)]
pub(super) struct FileLocks {
    _files: Vec<File>,
}

impl FileLocks {
    #[allow(clippy::disallowed_methods)] // Called only on spawn_blocking workers, never an async executor thread.
    pub(super) fn acquire(
        paths: &[PathBuf],
        cancel: &CancellationToken,
    ) -> Result<Self, ToolError> {
        // Intentionally independent of MAKEDO_HOME: sessions with different configs
        // still coordinate writes to the same checkout, for the same OS user.
        let dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| ToolError::Failed("HOME is required for file locks".into()))?
            .join(".makedo/file-locks");
        std::fs::create_dir_all(&dir).map_err(|e| failure(&dir, e))?;
        let mut paths = paths.to_vec();
        paths.sort();
        paths.dedup();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut files = Vec::new();
        for path in paths {
            let key =
                hex(ring::digest::digest(&SHA256, path.as_os_str().as_encoded_bytes()).as_ref());
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(dir.join(key))
                .map_err(|e| failure(&path, e))?;
            loop {
                if cancel.is_cancelled() {
                    return Err(ToolError::Cancelled);
                }
                match file.try_lock() {
                    Ok(()) => break,
                    Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => return Err(failure(&path, format!("file lock unavailable: {e}"))),
                }
            }
            files.push(file);
        }
        Ok(Self { _files: files })
    }
}

fn conflict(path: &Path) -> ToolError {
    failure(
        path,
        "version conflict; file changed or expected_version is missing. Read it again before editing; no changes applied to this file",
    )
}

#[derive(Debug)]
pub(super) struct Snapshot {
    pub text: String,
    pub version: String,
    permissions: Option<Permissions>,
}

impl Snapshot {
    /// No expected version means create-only, never a blind overwrite.
    pub(super) fn read(path: &Path, expected: Option<&str>) -> Result<Self, ToolError> {
        if std::fs::metadata(path).is_ok_and(|m| !m.is_file()) {
            return Err(failure(path, "not a regular file"));
        }
        let snapshot = match File::open(path) {
            Ok(mut file) => {
                let metadata = file.metadata().map_err(|e| failure(path, e))?;
                if !metadata.is_file() {
                    return Err(failure(path, "not a regular file"));
                }
                let mut text = String::new();
                file.read_to_string(&mut text)
                    .map_err(|e| failure(path, e))?;
                Self {
                    version: version(text.as_bytes()),
                    text,
                    permissions: Some(metadata.permissions()),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self {
                text: String::new(),
                version: MISSING.into(),
                permissions: None,
            },
            Err(e) => return Err(failure(path, e)),
        };
        if snapshot.version != expected.unwrap_or(MISSING) {
            return Err(conflict(path));
        }
        Ok(snapshot)
    }

    pub(super) fn verify(&self, path: &Path) -> Result<(), ToolError> {
        if std::fs::metadata(path).is_ok_and(|m| !m.is_file()) {
            return Err(failure(path, "not a regular file"));
        }
        let current = match File::open(path) {
            Ok(mut file) => {
                if !file.metadata().map_err(|e| failure(path, e))?.is_file() {
                    return Err(failure(path, "not a regular file"));
                }
                file_version(&mut file).map_err(|e| failure(path, e))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => MISSING.into(),
            Err(e) => return Err(failure(path, e)),
        };
        if current != self.version {
            return Err(conflict(path));
        }
        Ok(())
    }
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(super) struct StagedWrite {
    pub version: String,
    temp: PathBuf,
}

impl StagedWrite {
    pub(super) fn new(path: &Path, base: &Snapshot, text: &str) -> Result<Self, ToolError> {
        let parent = path
            .parent()
            .ok_or_else(|| failure(path, "no parent directory"))?;
        std::fs::create_dir_all(parent).map_err(|e| failure(parent, e))?;
        let (temp, mut file) = loop {
            let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temp = parent.join(format!(".makedo-write-{}-{id}", std::process::id()));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)
            {
                Ok(file) => break (temp, file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(failure(path, e)),
            }
        };
        let staged = Self {
            temp,
            version: version(text.as_bytes()),
        };
        file.write_all(text.as_bytes())
            .map_err(|e| failure(path, e))?;
        if let Some(permissions) = &base.permissions {
            file.set_permissions(permissions.clone())
                .map_err(|e| failure(path, e))?;
        }
        file.sync_all().map_err(|e| failure(path, e))?;
        Ok(staged)
    }

    pub(super) fn commit(&self, path: &Path, base: &Snapshot) -> Result<(), ToolError> {
        base.verify(path)?;
        // Publishing a new file must not overwrite a concurrently created file,
        // including one written by a process that does not use our advisory locks.
        let result = if base.version == MISSING {
            std::fs::hard_link(&self.temp, path)
        } else {
            std::fs::rename(&self.temp, path)
        };
        result.map_err(|e| failure(path, e))
    }
}

impl Drop for StagedWrite {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temp);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    #[allow(clippy::disallowed_methods)] // Synchronous cross-process lock probe.
    fn locks_coordinate_processes_and_staging_does_not_modify_targets() {
        const PROBE: &str = "MAKEDO_FILE_LOCK_TEST_DIR";
        if let Some(root) = std::env::var_os(PROBE) {
            let root = PathBuf::from(root);
            let token = CancellationToken::new();
            let _other = FileLocks::acquire(&[root.join("other")], &token).unwrap();
            let cancel = token.clone();
            let timer = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                cancel.cancel();
            });
            let result = FileLocks::acquire(&[root.join("shared")], &token);
            timer.join().unwrap();
            assert!(matches!(result, Err(ToolError::Cancelled)));
            return;
        }
        let root = std::env::temp_dir().join(format!("makedo-file-lock-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("shared");
        let locks =
            FileLocks::acquire(std::slice::from_ref(&path), &CancellationToken::new()).unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "builtin::file_store::tests::locks_coordinate_processes_and_staging_does_not_modify_targets", "--nocapture"])
            .env(PROBE, &root).output().unwrap();
        assert!(
            child.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        std::fs::write(&path, "before").unwrap();
        let base = Snapshot::read(&path, Some(&version(b"before"))).unwrap();
        let staged = StagedWrite::new(&path, &base, "after").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "before");
        std::fs::write(&path, "external edit").unwrap();
        assert!(
            staged
                .commit(&path, &base)
                .unwrap_err()
                .to_string()
                .contains("version conflict")
        );
        let temp = staged.temp.clone();
        drop(staged);
        assert!(!temp.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external edit");
        drop(locks);
        std::fs::remove_dir_all(root).unwrap();
    }
}
