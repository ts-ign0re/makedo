//! Файловые тулы: `read`, `write`, `edit`. Лимиты PLAN §3.1.

use std::fmt::Write as _;
use std::io::{BufRead, Seek};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Deserialize;

use super::file_store::{FileLocks, Snapshot, StagedWrite, file_version, target_path};
use crate::{Tool, ToolCtx, ToolError};

pub const READ_MAX_LINES: usize = 2_000;
pub const READ_MAX_LINE_CHARS: usize = 2_000;

fn resolve(cwd: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    }
}

// ---------------------------------------------------------------- read

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadArgs {
    /// Path to the file (absolute or relative to cwd)
    pub file_path: String,
    /// 1-based line to start from (default 1)
    pub offset: Option<usize>,
    /// Max lines to return (default 2000)
    pub limit: Option<usize>,
}

#[derive(Debug)]
pub struct ReadTool;

impl Tool for ReadTool {
    type Args = ReadArgs;

    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read a text file. Returns its SHA-256 version and numbered lines. Pass this version \
         as expected_version to edit/write or in expected_versions to apply_patch. Use offset/limit for large files \
         (max 2000 lines per call, lines longer than 2000 chars are cut)."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &ToolCtx, args: ReadArgs) -> Result<String, ToolError> {
        let path = resolve(&ctx.cwd, &args.file_path);
        tokio::task::spawn_blocking(move || {
            read_range(
                &path,
                args.offset.unwrap_or(1).max(1),
                args.limit.unwrap_or(READ_MAX_LINES).min(READ_MAX_LINES),
            )
        })
        .await
        .map_err(|e| ToolError::Failed(format!("read task: {e}")))?
    }
    fn access_paths(&self, args: &ReadArgs) -> Vec<PathBuf> {
        vec![PathBuf::from(&args.file_path)]
    }
}

// Keep at most one bounded line, even when a log contains a multi-gigabyte line.
fn bounded_line(reader: &mut impl BufRead) -> std::io::Result<Option<(Vec<u8>, bool)>> {
    let mut line = Vec::new();
    let mut found = false;
    let mut cut = false;
    loop {
        let bytes = reader.fill_buf()?;
        if bytes.is_empty() {
            return Ok(found.then_some((line, cut)));
        }
        found = true;
        let end = bytes.iter().position(|b| *b == b'\n').map(|i| i + 1);
        let n = end.unwrap_or(bytes.len());
        let keep = n.min((READ_MAX_LINE_CHARS * 4).saturating_sub(line.len()));
        line.extend_from_slice(&bytes[..keep]);
        cut |= keep < n;
        reader.consume(n);
        if end.is_some() {
            return Ok(Some((line, cut)));
        }
    }
}

fn read_range(path: &Path, offset: usize, limit: usize) -> Result<String, ToolError> {
    let acquire = || -> std::io::Result<String> {
        if !std::fs::metadata(path)?.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        let mut file = std::fs::File::open(path)?;
        let before = file.metadata()?;
        if !before.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        // ponytail: stream the whole file for a trustworthy content version; add
        // a metadata-keyed cache only if large-file reads make this measurable.
        let version = file_version(&mut file)?;
        file.rewind()?;
        let mut reader = std::io::BufReader::new(file);
        let mut out = String::new();
        let mut number = 0;
        while number < offset.saturating_sub(1).saturating_add(limit) {
            let Some((mut bytes, cut)) = bounded_line(&mut reader)? else {
                break;
            };
            number += 1;
            if number < offset {
                continue;
            }
            if cut {
                // A bounded UTF-8 prefix may end in the middle of a character.
                if let Err(e) = std::str::from_utf8(&bytes)
                    && e.error_len().is_none()
                {
                    bytes.truncate(e.valid_up_to());
                }
            }
            let line = std::str::from_utf8(&bytes)
                .map_err(|_| std::io::Error::other("not valid UTF-8 (binary file?)"))?
                .trim_end_matches(['\r', '\n']);
            let clipped: String = line.chars().take(READ_MAX_LINE_CHARS).collect();
            let _ = writeln!(
                out,
                "{number:>6}\t{clipped}{}",
                if cut || clipped.len() < line.len() {
                    "… (line truncated)"
                } else {
                    ""
                }
            );
            if out.len() >= crate::truncate::MAX_BYTES {
                break;
            }
        }
        if out.is_empty() {
            let _ = writeln!(out, "(empty: offset {offset}, scanned {number} lines)");
        }
        if !reader.fill_buf()?.is_empty() {
            let _ = writeln!(out, "… more lines. Continue with offset={}.", number + 1);
        }
        let after = reader.get_ref().metadata()?;
        if before.len() != after.len() || before.modified()? != after.modified()? {
            return Err(std::io::Error::other(
                "file changed during read; read it again",
            ));
        }
        Ok(format!("version={version}\n{out}"))
    };
    acquire().map_err(|e| ToolError::Failed(format!("Cannot read {}: {e}", path.display())))
}

/// Helpful context on a failed edit; matching itself remains exact.
fn edit_hint(text: &str, old: &str, new: &str) -> String {
    if new.trim().len() >= 20 && text.contains(new) {
        return " Replacement is already present; check whether the edit was applied.".into();
    }
    let keys: Vec<_> = old
        .lines()
        .map(str::trim)
        .filter(|s| s.chars().any(char::is_alphanumeric))
        .collect();
    if let Some((n, _)) = text
        .lines()
        .enumerate()
        .find(|(_, line)| keys.contains(&line.trim()))
    {
        let mut hint = String::from("\nNearby lines (use exact file text):\n");
        for (i, line) in text.lines().enumerate().skip(n.saturating_sub(2)).take(8) {
            let line: String = line.chars().take(200).collect();
            let _ = writeln!(hint, "{}: {line}", i + 1);
        }
        return hint;
    }
    " Search the file for the relevant region and use its exact text.".into()
}

// ---------------------------------------------------------------- write

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteArgs {
    /// Path to write (absolute or relative to cwd)
    pub file_path: String,
    /// Full file content
    pub content: String,
    /// Version returned by read or the last edit/write/patch. Omit (or use "missing") only to create a new file.
    pub expected_version: Option<String>,
}

#[derive(Debug)]
pub struct WriteTool;

impl Tool for WriteTool {
    type Args = WriteArgs;

    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &'static str {
        "Atomically write full file content. For an existing file, expected_version from read is required. \
         Omit expected_version only to create a new file. Conflicts leave the file unchanged; read again."
    }

    async fn run(&self, ctx: &ToolCtx, args: WriteArgs) -> Result<String, ToolError> {
        let ctx = ctx.clone();
        tokio::task::spawn_blocking(move || {
            let path = target_path(&ctx.cwd, &args.file_path)?;
            let _locks = FileLocks::acquire(std::slice::from_ref(&path), &ctx.cancel)?;
            let base = Snapshot::read(&path, args.expected_version.as_deref())?;
            let staged = StagedWrite::new(&path, &base, &args.content)?;
            if ctx.cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            staged.commit(&path, &base)?;
            Ok(format!(
                "Wrote {} bytes to {}\nversion={}",
                args.content.len(),
                path.display(),
                staged.version
            ))
        })
        .await
        .map_err(|e| ToolError::Failed(format!("write task: {e}")))?
    }

    fn access_paths(&self, args: &WriteArgs) -> Vec<PathBuf> {
        vec![PathBuf::from(&args.file_path)]
    }
}

// ---------------------------------------------------------------- edit

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditArgs {
    /// Path to edit (absolute or relative to cwd)
    pub file_path: String,
    /// Version returned by read or the last successful edit/write/patch of this file.
    pub expected_version: String,
    /// Exact text to replace (must be unique unless `replace_all`)
    pub old_string: String,
    /// Replacement text
    pub new_string: String,
    /// Replace every occurrence (default: false, `old_string` must be unique)
    pub replace_all: Option<bool>,
}

#[derive(Debug)]
pub struct EditTool;

impl Tool for EditTool {
    type Args = EditArgs;

    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Atomically replace exact text in a file. Requires expected_version from read; \
         on version conflict read again. old_string must match exactly once; \
         pass replace_all=true to replace every occurrence."
    }

    async fn run(&self, ctx: &ToolCtx, args: EditArgs) -> Result<String, ToolError> {
        let ctx = ctx.clone();
        tokio::task::spawn_blocking(move || edit(&ctx, &args))
            .await
            .map_err(|e| ToolError::Failed(format!("edit task: {e}")))?
    }

    fn access_paths(&self, args: &EditArgs) -> Vec<PathBuf> {
        vec![PathBuf::from(&args.file_path)]
    }
}

fn edit(ctx: &ToolCtx, args: &EditArgs) -> Result<String, ToolError> {
    if args.old_string.is_empty() {
        return Err(ToolError::InvalidArgs(
            "old_string must not be empty; use write for a new file".into(),
        ));
    }
    let path = target_path(&ctx.cwd, &args.file_path)?;
    let _locks = FileLocks::acquire(std::slice::from_ref(&path), &ctx.cancel)?;
    let base = Snapshot::read(&path, Some(&args.expected_version))?;
    let text = &base.text;
    let count = text.matches(&args.old_string).count();
    if count == 0 {
        return Err(ToolError::Failed(format!(
            "old_string not found in file.{}",
            edit_hint(text, &args.old_string, &args.new_string)
        )));
    }
    let replace_all = args.replace_all.unwrap_or(false);
    if count > 1 && !replace_all {
        return Err(ToolError::Failed(format!(
            "old_string matches {count} times. Add surrounding context to make it unique, \
                 or pass replace_all=true."
        )));
    }
    let new_text = if replace_all {
        text.replace(&args.old_string, &args.new_string)
    } else {
        text.replacen(&args.old_string, &args.new_string, 1)
    };
    let staged = StagedWrite::new(&path, &base, &new_text)?;
    if ctx.cancel.is_cancelled() {
        return Err(ToolError::Cancelled);
    }
    staged.commit(&path, &base)?;
    Ok(format!(
        "Replaced {count} occurrence(s) in {}\nversion={}",
        path.display(),
        staged.version
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::builtin::file_store::version;

    fn ctx(dir: &Path) -> ToolCtx {
        ToolCtx::new(dir.to_owned(), dir.join("spill"))
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "makedo-fs-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn write_read_edit_roundtrip() {
        let d = tmp();
        let c = ctx(&d);
        WriteTool
            .run(
                &c,
                WriteArgs {
                    file_path: "a.txt".into(),
                    content: "one\ntwo\nthree\n".into(),
                    expected_version: None,
                },
            )
            .await
            .unwrap();
        let out = ReadTool
            .run(
                &c,
                ReadArgs {
                    file_path: "a.txt".into(),
                    offset: Some(2),
                    limit: Some(1),
                },
            )
            .await
            .unwrap();
        assert!(out.contains("2\ttwo"));
        EditTool
            .run(
                &c,
                EditArgs {
                    file_path: "a.txt".into(),
                    old_string: "two".into(),
                    expected_version: out
                        .lines()
                        .next()
                        .unwrap()
                        .trim_start_matches("version=")
                        .into(),
                    new_string: "TWO".into(),
                    replace_all: None,
                },
            )
            .await
            .unwrap();
        assert!(
            std::fs::read_to_string(d.join("a.txt"))
                .unwrap()
                .contains("TWO")
        );
    }

    #[tokio::test]
    async fn edit_requires_unique_match() {
        let d = tmp();
        let c = ctx(&d);
        std::fs::write(d.join("b.txt"), "x x").unwrap();
        let err = EditTool
            .run(
                &c,
                EditArgs {
                    file_path: "b.txt".into(),
                    expected_version: version(b"x x"),
                    old_string: "x".into(),
                    new_string: "y".into(),
                    replace_all: None,
                },
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("2 times"));
    }

    #[tokio::test]
    async fn concurrent_writers_conflict_without_losing_content_or_mode() {
        use std::io::Read as _;
        use std::os::unix::fs::{PermissionsExt, symlink};

        let d = tmp();
        let c = ctx(&d);
        let path = d.join("script");
        std::fs::write(&path, "before").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
        symlink(&path, d.join("link")).unwrap();
        let mut old_reader = std::fs::File::open(&path).unwrap();
        let first = WriteArgs {
            file_path: "script".into(),
            content: "first".into(),
            expected_version: Some(version(b"before")),
        };
        let second = WriteArgs {
            file_path: "link".into(),
            content: "second".into(),
            expected_version: Some(version(b"before")),
        };
        let (a, b) = tokio::join!(WriteTool.run(&c, first), WriteTool.run(&c, second));
        assert_ne!(a.is_ok(), b.is_ok());
        let error = a.err().or_else(|| b.err()).unwrap();
        assert!(error.to_string().contains("version conflict"));
        let winner = std::fs::read_to_string(&path).unwrap();
        assert!(winner == "first" || winner == "second");
        assert!(
            std::fs::symlink_metadata(d.join("link"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o751
        );
        let mut old = String::new();
        old_reader.read_to_string(&mut old).unwrap();
        assert_eq!(old, "before", "open readers retain a complete old inode");
        let error = WriteTool
            .run(
                &c,
                WriteArgs {
                    file_path: "script".into(),
                    content: "blind".into(),
                    expected_version: None,
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("version conflict"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), winner);
        assert!(!std::fs::read_dir(&d).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".makedo-write-")
        }));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test]
    async fn other_files_proceed_and_waiting_writer_can_cancel() {
        let d = tmp();
        let c = ctx(&d);
        let path = target_path(&d, "locked").unwrap();
        let locks = FileLocks::acquire(&[path], &c.cancel).unwrap();
        let other = WriteArgs {
            file_path: "other".into(),
            content: "ok".into(),
            expected_version: None,
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), WriteTool.run(&c, other))
            .await
            .unwrap()
            .unwrap();
        let waiting = WriteTool.run(
            &c,
            WriteArgs {
                file_path: "locked".into(),
                content: "bad".into(),
                expected_version: None,
            },
        );
        let cancel = async {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            c.cancel.cancel();
        };
        let (result, ()) = tokio::join!(waiting, cancel);
        assert!(matches!(result, Err(ToolError::Cancelled)));
        assert!(!d.join("locked").exists());
        drop(locks);
        std::fs::remove_dir_all(d).unwrap();
    }
}

#[cfg(test)]
mod bounded_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn huge_line_is_bounded_and_failed_edit_hint_is_local() {
        let path = std::env::temp_dir().join(format!("makedo-bounded-line-{}", std::process::id()));
        std::fs::write(&path, format!("{}\nneedle\nlater\n", "x".repeat(1_000_000))).unwrap();
        let first = read_range(&path, 1, 1).unwrap();
        assert!(first.len() < 3_000);
        assert!(first.contains("line truncated"));
        assert!(read_range(&path, 2, 1).unwrap().contains("needle"));
        assert!(
            edit_hint("fn x() {\n    value\n}\n", "fn x() {\n value\n}", "other")
                .contains("Nearby lines")
        );
        std::fs::remove_file(path).unwrap();
    }
}
