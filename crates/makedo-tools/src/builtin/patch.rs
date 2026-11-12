//! Exact, version-checked patches. Validation precedes every filesystem mutation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::Deserialize;

use super::file_store::{FileLocks, MISSING, Snapshot, StagedWrite, failure, target_path};
use crate::{Tool, ToolCtx, ToolError};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApplyPatchArgs {
    /// *** Begin Patch / *** End Patch envelope. Add File uses +lines; Update File uses @@ hunks with space/-/+ lines; Delete File removes a file. Exact context only. Moves use add + delete.
    pub patch: String,
    /// Map of patch path to version from read (required for every update/delete). Added paths must be missing; their version may be omitted or "missing".
    #[serde(default)]
    pub expected_versions: BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct ApplyPatchTool;

impl Tool for ApplyPatchTool {
    type Args = ApplyPatchArgs;

    fn name(&self) -> &'static str {
        "apply_patch"
    }

    fn description(&self) -> &'static str {
        "Apply an exact patch to add, update or delete text files. Uses *** Begin Patch, \
         *** Add File: path (+lines), *** Update File: path (@@ hunks with space/-/+ lines), \
         *** Delete File: path, *** End Patch. Optional @@ exact anchor and *** End of File supported. \
         Requires expected_versions[path] from read for each existing file. All versions and hunks \
         are checked before writing; each file is replaced atomically. Multiple files are not a \
         crash-atomic transaction. On conflict read again. Moves use add + delete."
    }

    async fn run(&self, ctx: &ToolCtx, args: ApplyPatchArgs) -> Result<String, ToolError> {
        let ctx = ctx.clone();
        tokio::task::spawn_blocking(move || apply(&ctx, &args))
            .await
            .map_err(|e| ToolError::Failed(format!("apply_patch task: {e}")))?
    }

    fn access_paths(&self, args: &ApplyPatchArgs) -> Vec<PathBuf> {
        parse(&args.patch).map_or_else(
            |_| Vec::new(),
            |files| {
                files
                    .into_iter()
                    .map(|file| PathBuf::from(file.path))
                    .collect()
            },
        )
    }
}

#[derive(Debug)]
struct Hunk {
    anchor: Option<String>,
    old: Vec<String>,
    new: Vec<String>,
    eof: bool,
}

#[derive(Debug)]
enum Change {
    Add(String),
    Update(Vec<Hunk>),
    Delete,
}

#[derive(Debug)]
struct FilePatch {
    path: String,
    change: Change,
}

fn invalid(message: &str) -> ToolError {
    ToolError::InvalidArgs(format!("Invalid patch: {message}"))
}

fn parse(input: &str) -> Result<Vec<FilePatch>, ToolError> {
    let lines: Vec<_> = input.trim().lines().collect();
    if lines.first() != Some(&"*** Begin Patch") || lines.last() != Some(&"*** End Patch") {
        return Err(invalid("expected *** Begin Patch / *** End Patch envelope"));
    }
    let mut files = Vec::new();
    let mut i = 1;
    while i < lines.len() - 1 {
        let header = lines[i];
        i += 1;
        let (path, change) = if let Some(path) = header.strip_prefix("*** Add File: ") {
            let mut text = String::new();
            while i < lines.len() - 1 && !lines[i].starts_with("*** ") {
                let line = lines[i]
                    .strip_prefix('+')
                    .ok_or_else(|| invalid("added lines must start with +"))?;
                let _ = writeln!(text, "{line}");
                i += 1;
            }
            (path, Change::Add(text))
        } else if let Some(path) = header.strip_prefix("*** Delete File: ") {
            (path, Change::Delete)
        } else if let Some(path) = header.strip_prefix("*** Update File: ") {
            let mut hunks = Vec::new();
            while i < lines.len() - 1 && (lines[i] == "@@" || lines[i].starts_with("@@ ")) {
                let anchor = lines[i].strip_prefix("@@ ").map(str::to_owned);
                i += 1;
                let mut hunk = Hunk {
                    anchor,
                    old: Vec::new(),
                    new: Vec::new(),
                    eof: false,
                };
                while i < lines.len() - 1
                    && !lines[i].starts_with("@@")
                    && !lines[i].starts_with("*** ")
                {
                    let line = lines[i];
                    match line.as_bytes().first() {
                        Some(b' ') => {
                            hunk.old.push(line[1..].into());
                            hunk.new.push(line[1..].into());
                        }
                        Some(b'-') => hunk.old.push(line[1..].into()),
                        Some(b'+') => hunk.new.push(line[1..].into()),
                        _ => return Err(invalid("hunk lines must start with space, - or +")),
                    }
                    i += 1;
                }
                if lines.get(i) == Some(&"*** End of File") {
                    hunk.eof = true;
                    i += 1;
                }
                if hunk.old.is_empty() && hunk.new.is_empty() {
                    return Err(invalid("empty hunk"));
                }
                hunks.push(hunk);
            }
            if hunks.is_empty() {
                return Err(invalid("update requires at least one @@ hunk"));
            }
            (path, Change::Update(hunks))
        } else {
            return Err(invalid(
                "expected Add File, Update File or Delete File header (moves use add + delete)",
            ));
        };
        if path.trim().is_empty() {
            return Err(invalid("empty file path"));
        }
        files.push(FilePatch {
            path: path.into(),
            change,
        });
    }
    if files.is_empty() {
        return Err(invalid("no files"));
    }
    Ok(files)
}

fn update(text: &str, hunks: &[Hunk]) -> Result<String, ToolError> {
    // Preserve the file's line endings and final-newline convention. Mixed line
    // endings need exact edit, rather than silently normalizing unrelated lines.
    let crlf = text.contains("\r\n");
    if crlf && text.replace("\r\n", "").contains('\n') {
        return Err(invalid("mixed line endings; use edit for this file"));
    }
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut cursor = 0;
    for hunk in hunks {
        if let Some(anchor) = &hunk.anchor {
            let matches: Vec<_> = (cursor..lines.len())
                .filter(|i| lines[*i] == *anchor)
                .collect();
            if matches.len() != 1 {
                return Err(invalid(
                    "anchor must match exactly one line after the previous hunk",
                ));
            }
            cursor = matches[0] + 1;
        }
        let matches: Vec<_> = (cursor..=lines.len())
            .filter(|&i| {
                let end = i + hunk.old.len();
                end <= lines.len()
                    && lines[i..end] == hunk.old
                    && (!(hunk.eof || hunk.old.is_empty()) || end == lines.len())
            })
            .collect();
        if matches.len() != 1 {
            return Err(invalid(
                "hunk context is missing or ambiguous; read again and include unique exact context",
            ));
        }
        let start = matches[0];
        lines.splice(start..start + hunk.old.len(), hunk.new.clone());
        cursor = start + hunk.new.len();
    }
    let newline = if crlf { "\r\n" } else { "\n" };
    let mut out = lines.join(newline);
    if !lines.is_empty() && (text.ends_with('\n') || text.is_empty()) {
        out.push_str(newline);
    }
    Ok(out)
}

fn apply(ctx: &ToolCtx, args: &ApplyPatchArgs) -> Result<String, ToolError> {
    let files = parse(&args.patch)?;
    let paths: Vec<_> = files
        .iter()
        .map(|f| target_path(&ctx.cwd, &f.path))
        .collect::<Result<_, _>>()?;
    let unique = paths.iter().collect::<BTreeSet<_>>();
    if unique.len() != paths.len() {
        return Err(invalid(
            "a file may appear only once, including aliases; combine its hunks",
        ));
    }
    let sorted: Vec<_> = unique.into_iter().collect();
    if sorted.windows(2).any(|pair| pair[1].starts_with(pair[0])) {
        return Err(invalid(
            "a target file cannot also be another target's parent directory",
        ));
    }
    let _locks = FileLocks::acquire(&paths, &ctx.cancel)?;
    let mut plans = Vec::new();
    for (file, path) in files.iter().zip(&paths) {
        if matches!(file.change, Change::Delete)
            && std::fs::symlink_metadata(ctx.cwd.join(&file.path)).is_ok_and(|m| m.is_symlink())
        {
            return Err(failure(
                path,
                "Delete File through a symlink is unsupported; use the resolved path explicitly",
            ));
        }
        let expected = args.expected_versions.get(&file.path).map(String::as_str);
        if !matches!(file.change, Change::Add(_)) && expected.is_none_or(|v| v == MISSING) {
            return Err(failure(
                path,
                "expected_versions must include the version from read for every update/delete",
            ));
        }
        if matches!(file.change, Change::Add(_)) && expected.is_some_and(|v| v != MISSING) {
            return Err(failure(
                path,
                "Add File requires a missing path; use Update File for existing files",
            ));
        }
        let base = Snapshot::read(path, expected)?;
        let text = match &file.change {
            Change::Add(text) => Some(text.clone()),
            Change::Update(hunks) => Some(update(&base.text, hunks).map_err(|e| failure(path, e))?),
            Change::Delete => None,
        };
        plans.push((base, text));
    }
    // Stage everything only after validation. No target is changed if staging fails.
    let staged: Vec<_> = paths
        .iter()
        .zip(&plans)
        .map(|(path, (base, text))| {
            text.as_ref()
                .map(|text| StagedWrite::new(path, base, text))
                .transpose()
        })
        .collect::<Result<_, _>>()?;
    for (path, (base, _)) in paths.iter().zip(&plans) {
        base.verify(path)?;
    }
    if ctx.cancel.is_cancelled() {
        return Err(ToolError::Cancelled);
    }
    let mut out = String::new();
    // ponytail: atomic per file, not a multi-file transaction. Publish writes
    // before deletes; on an I/O failure report the already-applied paths.
    for delete in [false, true] {
        for (i, path) in paths.iter().enumerate() {
            if staged[i].is_none() != delete {
                continue;
            }
            let (base, _) = &plans[i];
            let result = if let Some(write) = &staged[i] {
                write.commit(path, base)
            } else {
                base.verify(path)
                    .and_then(|()| std::fs::remove_file(path).map_err(|e| failure(path, e)))
            };
            if let Err(error) = result {
                return Err(ToolError::Failed(format!(
                    "{error}\nAlready applied (not rolled back):\n{out}"
                )));
            }
            let version = staged[i].as_ref().map_or(MISSING, |write| &write.version);
            let _ = writeln!(out, "{}: version={version}", files[i].path);
        }
    }
    Ok(format!("Applied patch to {} file(s)\n{out}", files.len()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::super::file_store::version;
    use super::*;

    #[test]
    fn exact_hunks_preserve_endings_and_reject_ambiguity() {
        for (before, after) in [
            ("a\nb\n", "a\nB\n"),
            ("a\nb", "a\nB"),
            ("a\r\nb\r\n", "a\r\nB\r\n"),
        ] {
            let patch = parse("*** Begin Patch\n*** Update File: f\n@@\n a\n-b\n+B\n*** End of File\n*** End Patch").unwrap();
            let Change::Update(hunks) = &patch[0].change else {
                unreachable!()
            };
            assert_eq!(update(before, hunks).unwrap(), after);
        }
        let patch =
            parse("*** Begin Patch\n*** Update File: f\n@@\n-x\n+y\n*** End Patch").unwrap();
        let Change::Update(hunks) = &patch[0].change else {
            unreachable!()
        };
        assert!(update("x\nx\n", hunks).is_err());
        assert!(update("z\n", hunks).is_err());
        assert!(
            parse("*** Begin Patch\n*** Update File: f\n@@\nunprefixed\n*** End Patch").is_err()
        );
        let patch = parse("*** Begin Patch\n*** Update File: f\n@@\n first\n-x\n+y\n@@ second\n-x\n+z\n*** End of File\n*** End Patch").unwrap();
        let Change::Update(hunks) = &patch[0].change else {
            unreachable!()
        };
        assert_eq!(
            update("first\nx\nsecond\nx\n", hunks).unwrap(),
            "first\ny\nsecond\nz\n"
        );
    }

    #[tokio::test]
    async fn patch_validates_all_files_before_add_update_delete() {
        let root = std::env::temp_dir().join(format!("makedo-patch-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a"), "one\n").unwrap();
        std::fs::write(root.join("b"), "two\n").unwrap();
        let ctx = ToolCtx::new(root.clone(), root.join("spill"));
        let expected_versions = BTreeMap::from([
            ("a".into(), version(b"one\n")),
            ("b".into(), version(b"two\n")),
        ]);
        let bad = "*** Begin Patch\n*** Update File: a\n@@\n-one\n+ONE\n*** Update File: b\n@@\n-not present\n+bad\n*** End Patch";
        assert!(
            ApplyPatchTool
                .run(
                    &ctx,
                    ApplyPatchArgs {
                        patch: bad.into(),
                        expected_versions: expected_versions.clone()
                    }
                )
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "one\n");
        let good = "*** Begin Patch\n*** Update File: a\n@@\n-one\n+ONE\n*** Add File: nested/c\n+three\n*** Delete File: b\n*** End Patch";
        let mut stale = expected_versions.clone();
        stale.insert("b".into(), version(b"stale"));
        assert!(
            ApplyPatchTool
                .run(
                    &ctx,
                    ApplyPatchArgs {
                        patch: good.into(),
                        expected_versions: stale
                    }
                )
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "one\n");
        assert!(!root.join("nested").exists());
        let output = ApplyPatchTool
            .run(
                &ctx,
                ApplyPatchArgs {
                    patch: good.into(),
                    expected_versions,
                },
            )
            .await
            .unwrap();
        assert!(output.contains(&format!("a: version={}", version(b"ONE\n"))));
        assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "ONE\n");
        assert_eq!(
            std::fs::read_to_string(root.join("nested/c")).unwrap(),
            "three\n"
        );
        assert!(!root.join("b").exists());
        let duplicate = "*** Begin Patch\n*** Delete File: a\n*** Delete File: ./a\n*** End Patch";
        assert!(
            ApplyPatchTool
                .run(
                    &ctx,
                    ApplyPatchArgs {
                        patch: duplicate.into(),
                        expected_versions: BTreeMap::new()
                    }
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("aliases")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
