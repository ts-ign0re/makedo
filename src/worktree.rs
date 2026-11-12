//! Worktree-режим (PLAN §3.7): изоляция фич через git worktrees в
//! `.makedo/worktrees/<name>`, merge в основную ветку — только по явному
//! `makedo finish <name>`. Git CLI, без libgit2.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

fn git(root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .context("git not found in PATH")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

pub fn branch_name(name: &str) -> String {
    format!("makedo/{name}")
}

fn validate_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || name.len() > 80
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        bail!("worktree name must be 1–80 ASCII letters, digits, '-' or '_'");
    }
    Ok(())
}

fn state_dir(root: &Path) -> PathBuf {
    root.join(".makedo/worktrees/.state")
}

pub struct Lease(File);

impl Drop for Lease {
    fn drop(&mut self) {
        // Explicit unlock also releases copies briefly inherited by concurrent forks.
        let _ = self.0.unlock();
    }
}

fn lock(path: &Path) -> anyhow::Result<Lease> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.try_lock()
        .context("worktree is busy in another MAKEDO process; do not reuse or finish it")?;
    Ok(Lease(file))
}

fn metadata(root: &Path, name: &str) -> anyhow::Result<serde_json::Value> {
    validate_name(name)?;
    let path = state_dir(root).join(format!("{name}.json"));
    let data: serde_json::Value = serde_json::from_slice(&std::fs::read(path).context(
        "worktree has no MAKEDO ownership record; inspect git worktree list before adopting it",
    )?)?;
    if data["root"].as_str() != Some(root.canonicalize()?.to_string_lossy().as_ref()) {
        bail!("worktree belongs to a different checkout");
    }
    let dir = root.join(".makedo/worktrees").join(name);
    if PathBuf::from(git(&dir, &["rev-parse", "--show-toplevel"])?).canonicalize()?
        != dir.canonicalize()?
        || git(&dir, &["symbolic-ref", "--short", "HEAD"])? != branch_name(name)
    {
        bail!("worktree path or branch no longer matches its ownership record");
    }
    Ok(data)
}

/// Held for the whole editing session, shared across all MAKEDO processes.
pub fn lease(root: &Path, name: &str) -> anyhow::Result<Lease> {
    validate_name(name)?;
    let owner = lock(&state_dir(root).join(format!("{name}.lock")))?;
    metadata(root, name)?;
    Ok(owner)
}

/// A new task starts at the current committed HEAD; dirty changes are never copied.
pub fn start(root: &Path, name: &str) -> anyhow::Result<PathBuf> {
    validate_name(name)?;
    let dir = root.join(".makedo/worktrees").join(name);
    std::fs::create_dir_all(state_dir(root))?;
    let _operation = lock(&state_dir(root).join("operations.lock"))?;
    if dir.exists() {
        metadata(root, name)?;
        return Ok(dir);
    }
    let base_branch = git(root, &["symbolic-ref", "--short", "HEAD"])
        .context("start a worktree from a named branch, not detached HEAD")?;
    let base_commit = git(root, &["rev-parse", "HEAD"])?;
    makedo_wiki::seed::ensure_makedo_gitignore(&root.join(".makedo"))?;
    let branch = branch_name(name);
    git(
        root,
        &["worktree", "add", "-b", &branch, &dir.display().to_string()],
    )
    .with_context(|| format!("creating worktree for `{name}`"))?;
    let data = serde_json::json!({"root":root.canonicalize()?, "base_branch":base_branch, "base_commit":base_commit});
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(state_dir(root).join(format!("{name}.json")))?;
    file.write_all(&serde_json::to_vec(&data)?)?;
    Ok(dir)
}

/// Explicit finish commits this worktree, previews integration, then merges.
/// Returns true only when the worktree was removed. Logs/history are retained.
pub fn finish(root: &Path, name: &str) -> anyhow::Result<bool> {
    let data = metadata(root, name)?;
    let _operation = lock(&state_dir(root).join("operations.lock"))?;
    let _owner = lease(root, name)?;
    let main_branch = git(root, &["symbolic-ref", "--short", "HEAD"])?;
    if data["base_branch"].as_str() != Some(&main_branch) {
        bail!(
            "main checkout changed branch; expected {}. No changes were staged or merged",
            data["base_branch"]
        );
    }
    if !git(root, &["status", "--porcelain", "--untracked-files=all"])?.is_empty() {
        bail!(
            "main checkout has uncommitted changes; finish refuses to stage, stash or overwrite them"
        );
    }
    let dir = root.join(".makedo/worktrees").join(name);
    let branch = branch_name(name);
    let base = data["base_commit"]
        .as_str()
        .context("missing worktree base commit")?;
    git(&dir, &["merge-base", "--is-ancestor", base, "HEAD"])
        .context("worktree history no longer descends from its recorded base")?;
    if !git(
        &dir,
        &[
            "diff",
            "--cached",
            "--name-only",
            "--",
            ".makedo/sessions",
            ".makedo/spill",
            ".makedo/worktrees",
        ],
    )?
    .is_empty()
    {
        bail!("runtime logs/history are staged; inspect and unstage them before finish");
    }
    // Explicit finish authorizes committing the reviewed feature, never the main checkout.
    git(
        &dir,
        &[
            "add",
            "-A",
            "--",
            ".",
            ":!.makedo/sessions",
            ":!.makedo/spill",
            ":!.makedo/worktrees",
        ],
    )?;
    if !git(&dir, &["diff", "--cached", "--name-only"])?.is_empty() {
        git(&dir, &["commit", "-m", &format!("makedo: finish {name}")])?;
    }
    // Detect conflicts without touching the main index/worktree (Git >= 2.38).
    git(root, &["merge-tree", "--write-tree", &main_branch, &branch])
        .context("integration preview failed; resolve conflicts in the feature worktree and retry; main checkout is unchanged")?;
    git(
        root,
        &[
            "merge",
            "--no-ff",
            "-m",
            &format!("Merge {branch} into {main_branch}"),
            &branch,
        ],
    )?;
    let integration_sha = git(root, &["rev-parse", "HEAD"])?;
    let memory = makedo_memory::Memory::open(root, makedo_config::load(root)?.memory)?;
    memory.record_merge(
        &branch,
        &format!("refs/heads/{main_branch}"),
        &integration_sha,
    )?;
    let has_runtime_data = ["sessions", "spill"].iter().any(|p| {
        std::fs::read_dir(dir.join(".makedo").join(p))
            .is_ok_and(|mut entries| entries.next().is_some())
    });
    if has_runtime_data {
        return Ok(false);
    }
    git(root, &["worktree", "remove", &dir.display().to_string()])?;
    git(root, &["branch", "-d", &branch])?;
    std::fs::remove_file(state_dir(root).join(format!("{name}.json")))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn repo() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "makedo-wt-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        git(&d, &["init", "-b", "main"]).unwrap();
        git(&d, &["config", "user.email", "t@t"]).unwrap();
        git(&d, &["config", "user.name", "t"]).unwrap();
        std::fs::write(d.join("a.txt"), "base\n").unwrap();
        makedo_wiki::seed::ensure_makedo_gitignore(&d.join(".makedo")).unwrap();
        git(&d, &["add", "-A"]).unwrap();
        git(&d, &["commit", "-m", "base"]).unwrap();
        d
    }

    #[test]
    fn start_work_finish_merges_into_main() {
        let root = repo();
        let wt = start(&root, "feat-x").unwrap();
        assert!(wt.ends_with(".makedo/worktrees/feat-x"));
        // .gitignore исключает worktrees
        let gi = std::fs::read_to_string(root.join(".makedo/.gitignore")).unwrap();
        assert!(gi.contains("worktrees/"));
        // работа в фиче
        std::fs::write(wt.join("feature.txt"), "new\n").unwrap();
        // повторный start — идемпотентен
        assert_eq!(start(&root, "feat-x").unwrap(), wt);

        assert!(finish(&root, "feat-x").unwrap());
        // изменения в main, worktree и ветка убраны
        assert!(root.join("feature.txt").is_file());
        assert!(!wt.exists());
        assert!(
            git(&root, &["branch", "--list", "makedo/feat-x"])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn finish_unknown_worktree_fails() {
        let root = repo();
        assert!(finish(&root, "nope").is_err());
    }
    #[test]
    fn unsafe_names_foreign_directories_and_active_owners_are_rejected() {
        let root = repo();
        assert!(start(&root, "../escape").is_err());
        std::fs::create_dir_all(root.join(".makedo/worktrees/foreign")).unwrap();
        assert!(start(&root, "foreign").is_err());
        start(&root, "owned").unwrap();
        let owner = lease(&root, "owned").unwrap();
        assert!(lease(&root, "owned").is_err());
        assert!(finish(&root, "owned").is_err());
        drop(owner);
        assert!(finish(&root, "owned").unwrap());
    }

    #[test]
    fn dirty_or_changed_parent_is_never_touched() {
        let root = repo();
        let wt = start(&root, "feature").unwrap();
        std::fs::write(wt.join("feature.txt"), "feature").unwrap();
        std::fs::write(root.join("a.txt"), "user changes").unwrap();
        let before = git(&wt, &["rev-parse", "HEAD"]).unwrap();
        assert!(finish(&root, "feature").is_err());
        assert_eq!(git(&wt, &["rev-parse", "HEAD"]).unwrap(), before);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "user changes"
        );
        git(&root, &["add", "a.txt"]).unwrap();
        git(&root, &["commit", "-m", "user changes"]).unwrap();
        git(&root, &["switch", "-c", "other"]).unwrap();
        assert!(finish(&root, "feature").is_err());
        assert_eq!(
            git(&root, &["symbolic-ref", "--short", "HEAD"]).unwrap(),
            "other"
        );
    }

    #[test]
    fn merge_conflict_stays_out_of_parent_and_logs_survive_finish() {
        let root = repo();
        let wt = start(&root, "conflict").unwrap();
        std::fs::write(root.join("a.txt"), "parent").unwrap();
        git(&root, &["add", "a.txt"]).unwrap();
        git(&root, &["commit", "-m", "parent update"]).unwrap();
        std::fs::write(wt.join("a.txt"), "feature").unwrap();
        assert!(finish(&root, "conflict").is_err());
        assert!(git(&root, &["status", "--porcelain"]).unwrap().is_empty());
        assert!(!root.join(".git/MERGE_HEAD").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "parent"
        );
        let logged = start(&root, "logged").unwrap();
        // Old projects may not yet ignore session history. Finish still excludes it.
        std::fs::write(logged.join(".makedo/.gitignore"), "worktrees/\nspill/\n").unwrap();
        std::fs::create_dir_all(logged.join(".makedo/sessions")).unwrap();
        let log = logged.join(".makedo/sessions/keep.jsonl");
        std::fs::write(&log, "history").unwrap();
        assert!(!finish(&root, "logged").unwrap());
        assert_eq!(std::fs::read_to_string(log).unwrap(), "history");
        assert!(
            git(&root, &["ls-files", ".makedo/sessions"])
                .unwrap()
                .is_empty()
        );
    }
}
