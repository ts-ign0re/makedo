//! Детерминированный seed vault без LLM (PLAN §3.3): дерево модулей, манифесты,
//! каркасные ноды. Секунды, не минуты — обогащение придёт инкрементально.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// First words of a seeded stub: a node that still carries them knows nothing yet.
pub const SEED_MARKER: &str = "Seeded node.";

const MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "go.mod",
    "pyproject.toml",
    "requirements.txt",
    "pom.xml",
    "build.gradle",
    "Makefile",
];

/// Стек по имени манифеста (`Makefile` языком не считается).
fn stack_label(manifest: &str) -> Option<&'static str> {
    Some(match manifest {
        "Cargo.toml" => "Rust (Cargo)",
        "package.json" => "JavaScript/TypeScript (npm)",
        "go.mod" => "Go",
        "pyproject.toml" | "requirements.txt" => "Python",
        "pom.xml" => "Java (Maven)",
        "build.gradle" => "JVM (Gradle)",
        _ => return None,
    })
}

/// Карточка проекта для системного промпта: стек и верхний уровень дерева.
/// Без неё модель угадывает язык и тратит шаги на поиск не тех файлов.
/// Смотрит только корень и один уровень вниз (уважая .gitignore) — полного
/// обхода нет, старт остаётся дешёвым. Пустая строка — сказать нечего.
#[must_use]
pub fn project_card(root: &Path) -> String {
    let mut stack = std::collections::BTreeSet::new();
    let mut dirs = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .max_depth(Some(2))
        .build()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy();
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            if entry.depth() == 1 {
                dirs.push(format!("{name}/"));
            }
        } else if let Some(label) = stack_label(&name) {
            stack.insert(label);
        }
    }
    if stack.is_empty() && dirs.is_empty() {
        return String::new();
    }
    dirs.sort();
    let more = if dirs.len() > 12 { ", …" } else { "" };
    dirs.truncate(12);
    let stack = if stack.is_empty() {
        "stack not detected".to_owned()
    } else {
        stack.into_iter().collect::<Vec<_>>().join(", ")
    };
    format!(
        "PROJECT: {stack}. Top-level directories: {}{more}.",
        dirs.join(", ")
    )
}

#[derive(Debug)]
pub struct SeedReport {
    pub vault: PathBuf,
    pub nodes_created: usize,
    pub files_scanned: usize,
}

/// Прогресс-колбэк: (сколько файлов просмотрено) — для спиннера TUI.
pub fn seed(
    project_root: &Path,
    data_dir: &Path,
    mut progress: impl FnMut(usize),
) -> std::io::Result<SeedReport> {
    let vault = data_dir.join("wiki");
    std::fs::create_dir_all(&vault)?;

    // один проход: top-level модули (директории с кодом), манифесты, счётчик файлов
    let mut files_scanned = 0usize;
    let mut manifests: Vec<String> = Vec::new();
    let mut top_dirs: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for entry in ignore::WalkBuilder::new(project_root)
        .filter_entry(|e| e.file_name() != ".makedo" && e.file_name() != ".git")
        .build()
        .flatten()
    {
        let path = entry.path();
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        files_scanned += 1;
        if files_scanned.is_multiple_of(200) {
            progress(files_scanned);
        }
        let rel = path.strip_prefix(project_root).unwrap_or(path);
        if let Some(name) = rel.file_name().map(|n| n.to_string_lossy())
            && MANIFESTS.contains(&name.as_ref())
        {
            manifests.push(rel.display().to_string());
        }
        if let Some(first) = rel.components().next() {
            let first = first.as_os_str().to_string_lossy().into_owned();
            if rel.components().count() > 1 {
                *top_dirs.entry(first).or_insert(0) += 1;
            }
        }
    }
    progress(files_scanned);

    let mut nodes_created = 0usize;

    // schema.md — грамматика рёбер (дефолт, переопределяемая)
    let schema = vault.join("schema.md");
    if !schema.exists() {
        std::fs::write(
            &schema,
            "---\ntype: schema\ndescription: Edge grammar for this vault\n---\n\n\
             # Link grammar\n\n\
             Typed edges (Dataview-style inline fields):\n\n\
             - `satisfies:: [[FR-x]]` — implementation node serves a business goal\n\
             - `designed-in:: [[Design Doc]]`\n\
             - `implemented-by:: [[Module]]`\n\
             - `verified-by:: [[Test]]`\n\
             - `depends-on:: [[Module]]`\n\
             - `part-of:: [[Module]]`\n",
        )?;
        nodes_created += 1;
    }

    // нода на каждый top-level модуль
    for (dir, count) in &top_dirs {
        let node = vault.join(format!("{dir}.md"));
        if node.exists() {
            continue;
        }
        std::fs::write(
            &node,
            format!(
                "---\ntype: module\ndescription: Top-level directory `{dir}` ({count} files)\n---\n\n\
                 {SEED_MARKER} Enrich while working: what lives here, key entry points,\n\
                 `depends-on::`/`part-of::` edges.\n"
            ),
        )?;
        nodes_created += 1;
    }

    // index.md — карта проекта
    let mut index = String::from(
        "---\ntype: index\ndescription: Project map (seeded)\n---\n\n# Project map\n\n## Modules\n\n",
    );
    for (dir, count) in &top_dirs {
        let _ = writeln!(index, "- [[{dir}]] — {count} files");
    }
    index.push_str("\n## Manifests\n\n");
    for m in &manifests {
        let _ = writeln!(index, "- `{m}`");
    }
    std::fs::write(vault.join("index.md"), index)?;
    nodes_created += 1;

    ensure_makedo_gitignore(data_dir)?;
    Ok(SeedReport {
        vault,
        nodes_created,
        files_scanned,
    })
}

/// `.makedo/.gitignore`: worktrees/ и spill/ вне git (wiki — в git!). Пользовательский
/// файл уважаем — только дополняем отсутствующие записи.
pub fn ensure_makedo_gitignore(makedo_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(makedo_dir)?;
    let path = makedo_dir.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out = existing.clone();
    for entry in ["worktrees/", "spill/", "sessions/"] {
        if !existing.lines().any(|l| l.trim() == entry) {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(entry);
            out.push('\n');
        }
    }
    if out != existing {
        std::fs::write(&path, out)?;
    }
    Ok(())
}

/// Vault существует и непуст?
#[must_use]
pub fn exists(data_dir: &Path) -> bool {
    data_dir.join("wiki/index.md").is_file()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn project_card_names_the_stack_and_top_level_without_hidden_dirs() {
        let d = std::env::temp_dir().join(format!("makedo-card-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for dir in ["crates/core", "docs", ".git", "web"] {
            std::fs::create_dir_all(d.join(dir)).unwrap();
        }
        std::fs::write(d.join("Cargo.toml"), "").unwrap();
        std::fs::write(d.join("web/package.json"), "{}").unwrap();
        assert_eq!(
            project_card(&d),
            "PROJECT: JavaScript/TypeScript (npm), Rust (Cargo). \
             Top-level directories: crates/, docs/, web/."
        );
        assert_eq!(project_card(&d.join("docs")), "");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn seeds_modules_and_gitignore() {
        let d = std::env::temp_dir().join(format!("makedo-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::create_dir_all(d.join("docs")).unwrap();
        std::fs::write(d.join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(d.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(d.join("docs/a.md"), "x").unwrap();

        assert!(!exists(&d.join(".makedo")));
        let report = seed(&d, &d.join(".makedo"), |_| {}).unwrap();
        assert!(exists(&d.join(".makedo")));
        assert!(report.nodes_created >= 4); // schema + index + 2 модуля
        let index = std::fs::read_to_string(d.join(".makedo/wiki/index.md")).unwrap();
        assert!(index.contains("[[src]]"));
        assert!(index.contains("Cargo.toml"));
        let gi = std::fs::read_to_string(d.join(".makedo/.gitignore")).unwrap();
        assert!(gi.contains("worktrees/"));
        // повторный вызов не дублирует записи
        seed(&d, &d.join(".makedo"), |_| {}).unwrap();
        let gi2 = std::fs::read_to_string(d.join(".makedo/.gitignore")).unwrap();
        assert_eq!(gi, gi2);
        // сид-ноды парсятся и проходят check без dead links
        let g = crate::Graph::load(&d.join(".makedo/wiki")).unwrap();
        assert!(g.check().iter().all(|i| !i.is_error()));
    }
}
