//! Private, unique output files: neither model-provided IDs nor repeated sessions overwrite logs.
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

pub fn create(dir: &Path, label: &str, extension: &str) -> std::io::Result<(PathBuf, File)> {
    std::fs::create_dir_all(dir)?;
    let label: String = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(80)
        .collect();
    loop {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            "{label}-{}-{sequence}.{extension}",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn repeated_or_path_shaped_ids_cannot_overwrite_or_escape() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("makedo-artifacts-{}", std::process::id()));
        let (first, mut file) = create(&dir, "../../same", "log").unwrap();
        file.write_all(b"first").unwrap();
        let (second, _) = create(&dir, "../../same", "log").unwrap();
        assert_ne!(first, second);
        assert_eq!(first.parent(), Some(dir.as_path()));
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
