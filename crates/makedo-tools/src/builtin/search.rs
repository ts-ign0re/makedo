//! `grep` и `glob` — движок ripgrep как библиотека (`ignore` + `grep-searcher`):
//! уважает .gitignore, не требует rg-бинаря на машине (PLAN §4).
//! Тяжёлый walk — в `spawn_blocking`, отмена проверяется в цикле.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{Tool, ToolCtx, ToolError};

pub const GREP_MAX_MATCHES: usize = 1_000;
pub const GREP_MAX_LINE_CHARS: usize = 500;
pub const GLOB_MAX_RESULTS: usize = 100;

fn base_dir(cwd: &Path, path: Option<&String>) -> PathBuf {
    match path {
        Some(p) if Path::new(p).is_absolute() => PathBuf::from(p),
        Some(p) => cwd.join(p),
        None => cwd.to_owned(),
    }
}

/// Сводка «какие файлы здесь есть» для пустого результата: модель исправляет
/// промах по типу файлов за один шаг, а не перебором масок.
fn file_types(types: &BTreeMap<String, usize>) -> String {
    if types.is_empty() {
        return "There are no files here.".to_owned();
    }
    let mut list: Vec<_> = types.iter().collect();
    list.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let top: Vec<String> = list
        .iter()
        .take(8)
        .map(|(ext, n)| format!("{ext} {n}"))
        .collect();
    format!("File types here: {}.", top.join(", "))
}

fn count_type(types: &mut BTreeMap<String, usize>, path: &Path) {
    let ext = path.extension().map_or_else(
        || "(no extension)".to_owned(),
        |e| e.to_string_lossy().to_lowercase(),
    );
    *types.entry(ext).or_default() += 1;
}

// ---------------------------------------------------------------- grep

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepArgs {
    /// Regex pattern (Rust regex syntax)
    pub pattern: String,
    /// Directory or file to search (default: cwd)
    pub path: Option<String>,
    /// Only search files matching this glob (e.g. "*.rs")
    pub glob: Option<String>,
}

#[derive(Debug)]
pub struct GrepTool;

impl Tool for GrepTool {
    type Args = GrepArgs;

    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents with a regex. Respects .gitignore. \
         Returns `path:line: text` matches (first 1000)."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &ToolCtx, args: GrepArgs) -> Result<String, ToolError> {
        let dir = base_dir(&ctx.cwd, args.path.as_ref());
        let cancel = ctx.cancel.clone();
        let matcher = RegexMatcher::new_line_matcher(&args.pattern)
            .map_err(|e| ToolError::InvalidArgs(format!("bad regex: {e}")))?;
        let file_glob = args
            .glob
            .map(|g| {
                globset::GlobBuilder::new(&g)
                    .literal_separator(false)
                    .build()
                    .map(|g| g.compile_matcher())
                    .map_err(|e| ToolError::InvalidArgs(format!("bad glob: {e}")))
            })
            .transpose()?;

        let out = tokio::task::spawn_blocking(move || {
            let mut searcher = SearcherBuilder::new()
                .binary_detection(BinaryDetection::quit(b'\x00'))
                .line_number(true)
                .build();
            let mut out = String::new();
            let mut hits = 0usize;
            // сколько файлов прошло маску и какие типы файлов вообще встретились
            let mut in_scope = 0usize;
            let mut types = BTreeMap::new();
            for entry in ignore::WalkBuilder::new(&dir).build() {
                if cancel.is_cancelled() || hits >= GREP_MAX_MATCHES {
                    break;
                }
                let Ok(entry) = entry else { continue };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let path = entry.path();
                let rel_path = path.strip_prefix(&dir).unwrap_or(path);
                if let Some(g) = &file_glob {
                    count_type(&mut types, path);
                    // матчим по ОТНОСИТЕЛЬНОМУ пути: glob "src/*.rs" против абсолютного
                    // "/abs/src/main.rs" никогда не совпадал (баг корректности)
                    let name = path.file_name().map(|n| n.to_string_lossy());
                    if !g.is_match(rel_path) && !name.is_some_and(|n| g.is_match(n.as_ref())) {
                        continue;
                    }
                }
                in_scope += 1;
                let rel = rel_path.display().to_string();
                let _ = searcher.search_path(
                    &matcher,
                    path,
                    UTF8(|line_no, line| {
                        if hits >= GREP_MAX_MATCHES {
                            return Ok(false);
                        }
                        hits += 1;
                        let line = line.trim_end();
                        let line: String = line.chars().take(GREP_MAX_LINE_CHARS).collect();
                        let _ = writeln!(out, "{rel}:{line_no}: {line}");
                        Ok(true)
                    }),
                );
            }
            (out, hits, in_scope, types)
        })
        .await
        .map_err(|e| ToolError::Failed(format!("search task failed: {e}")))?;

        let (mut text, hits, in_scope, types) = out;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if hits == 0 {
            // маска не подошла ни к одному файлу: промах по типу файлов, а не по шаблону
            return Ok(if in_scope == 0 && !types.is_empty() {
                format!("No files match that glob. {}", file_types(&types))
            } else {
                "No matches found.".to_owned()
            });
        }
        if hits >= GREP_MAX_MATCHES {
            let _ = writeln!(
                text,
                "[Stopped at {GREP_MAX_MATCHES} matches — narrow the pattern.]"
            );
        }
        Ok(text)
    }
}

// ---------------------------------------------------------------- glob

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GlobArgs {
    /// Glob pattern, e.g. "**/*.rs" or "src/*.ts"
    pub pattern: String,
    /// Base directory (default: cwd)
    pub path: Option<String>,
}

#[derive(Debug)]
pub struct GlobTool;

impl Tool for GlobTool {
    type Args = GlobArgs;

    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files by glob pattern. Respects .gitignore. Returns up to 100 paths."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &ToolCtx, args: GlobArgs) -> Result<String, ToolError> {
        let dir = base_dir(&ctx.cwd, args.path.as_ref());
        let cancel = ctx.cancel.clone();
        let glob = globset::GlobBuilder::new(&args.pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| ToolError::InvalidArgs(format!("bad glob: {e}")))?
            .compile_matcher();

        let (found, types) = tokio::task::spawn_blocking(move || {
            let mut found: Vec<String> = Vec::new();
            let mut types = BTreeMap::new();
            for entry in ignore::WalkBuilder::new(&dir).build() {
                if cancel.is_cancelled() || found.len() > GLOB_MAX_RESULTS * 4 {
                    break;
                }
                let Ok(entry) = entry else { continue };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let rel = entry
                    .path()
                    .strip_prefix(&dir)
                    .unwrap_or(entry.path())
                    .display()
                    .to_string();
                count_type(&mut types, entry.path());
                if glob.is_match(&rel) {
                    found.push(rel);
                }
            }
            found.sort();
            (found, types)
        })
        .await
        .map_err(|e| ToolError::Failed(format!("glob task failed: {e}")))?;

        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if found.is_empty() {
            return Ok(format!("No files matched. {}", file_types(&types)));
        }
        let total = found.len();
        let mut out: String =
            found
                .iter()
                .take(GLOB_MAX_RESULTS)
                .fold(String::new(), |mut s, p| {
                    s.push_str(p);
                    s.push('\n');
                    s
                });
        if total > GLOB_MAX_RESULTS {
            let _ = writeln!(
                out,
                "[{total}+ matches, showing {GLOB_MAX_RESULTS} — narrow the pattern.]"
            );
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn setup() -> (PathBuf, ToolCtx) {
        let d = std::env::temp_dir().join(format!(
            "makedo-search-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        std::fs::write(d.join("sub/b.txt"), "alpha in text\n").unwrap();
        let ctx = ToolCtx::new(d.clone(), d.join("spill"));
        (d, ctx)
    }

    #[tokio::test]
    async fn empty_result_for_a_wrong_file_type_says_which_types_exist() {
        let (_d, ctx) = setup();
        let grep = GrepTool
            .run(
                &ctx,
                GrepArgs {
                    pattern: "alpha".into(),
                    path: None,
                    glob: Some("*.py".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            grep,
            "No files match that glob. File types here: rs 1, txt 1."
        );
        let glob = GlobTool
            .run(
                &ctx,
                GlobArgs {
                    pattern: "**/*.py".into(),
                    path: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(glob, "No files matched. File types here: rs 1, txt 1.");
        // маска подошла, совпадений нет — это обычный пустой ответ без сводки
        let none = GrepTool
            .run(
                &ctx,
                GrepArgs {
                    pattern: "zzz".into(),
                    path: None,
                    glob: Some("*.rs".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(none, "No matches found.");
    }

    #[tokio::test]
    async fn grep_finds_and_filters_by_glob() {
        let (_d, ctx) = setup();
        let out = GrepTool
            .run(
                &ctx,
                GrepArgs {
                    pattern: "alpha".into(),
                    path: None,
                    glob: Some("*.rs".into()),
                },
            )
            .await
            .unwrap();
        assert!(out.contains("a.rs:1:"));
        assert!(!out.contains("b.txt"));
    }

    #[tokio::test]
    async fn glob_lists_files() {
        let (_d, ctx) = setup();
        let out = GlobTool
            .run(
                &ctx,
                GlobArgs {
                    pattern: "**/*.txt".into(),
                    path: None,
                },
            )
            .await
            .unwrap();
        assert!(out.contains("sub/b.txt"));
    }
}
