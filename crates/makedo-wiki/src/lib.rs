//! LLM Wiki: Obsidian-совместимый vault `.makedo/wiki/` — markdown-ноды,
//! `[[wikilinks]]`, типизированные рёбра `key:: [[Target]]` (грамматика
//! traceability-graph). Самописный парсер: плоский frontmatter, никакого YAML
//! (PLAN §3.3). Граф с кэшем по fingerprint; поиск по нодам — `memory_search`.

pub mod seed;
pub mod tools;

/// Текст задачи «куратора wiki» — общий для CLI `makedo wiki enrich`, `/enrich`
/// в TUI и онбординга.
#[must_use]
pub fn enrich_prompt(focus: Option<&str>) -> String {
    let scope = focus.map_or_else(
        || {
            "весь проект: структуру, ключевые модули и их ответственность, связи, паттерны тестов"
                .to_owned()
        },
        |f| format!("область «{f}» и её окружение"),
    );
    format!(
        "Ты — куратор wiki проекта (.makedo/wiki). Изучи {scope} читающими тулами \
         (memory_search перед grep). Создай/обнови ноды: caveman-стиль — телеграфно, \
         факты, пути файлов, имена функций; typed edges по .makedo/wiki/schema.md; \
         обнови index.md. КОД НЕ МЕНЯТЬ — писать только в .makedo/wiki/*. \
         Команды запускать нельзя."
    )
}

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum WikiError {
    #[error("wiki not found at {0} — run `makedo` in the project to initialize it")]
    NotFound(PathBuf),
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub path: PathBuf,
    /// frontmatter `type:` (module, concept, fr, …); пусто = untyped (warning).
    pub node_type: String,
    /// frontmatter `description:` либо первый абзац тела.
    pub description: String,
    pub body: String,
    /// Типизированные рёбра `key:: [[Target]]` — (kind, target).
    pub edges: Vec<(String, String)>,
    /// Нетипизированные `[[wikilinks]]` из тела (вне `key::`-строк).
    pub links: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Graph {
    pub nodes: BTreeMap<String, Node>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Issue {
    /// Ссылка на несуществующую ноду — ошибка.
    DeadLink { from: String, to: String },
    /// Нода без `type:` — warning.
    Untyped { node: String },
}

impl Issue {
    #[must_use]
    pub fn is_error(&self) -> bool {
        matches!(self, Self::DeadLink { .. })
    }
}

// ---------------------------------------------------------------- парсер

/// Все `[[Target]]` в строке. Текст в `backticks` — цитируемый синтаксис,
/// не ссылки (schema.md документирует грамматику примерами).
fn wikilinks(line: &str) -> Vec<String> {
    let stripped: String = if line.contains('`') {
        line.split('`')
            .enumerate()
            .filter_map(|(i, part)| (i % 2 == 0).then_some(part))
            .collect()
    } else {
        line.to_owned()
    };
    let mut out = Vec::new();
    let mut rest = stripped.as_str();
    while let Some(start) = rest.find("[[") {
        let Some(len) = rest[start + 2..].find("]]") else {
            break;
        };
        let target = &rest[start + 2..start + 2 + len];
        // `[[Target|alias]]` — целевая часть до `|`
        let target = target.split('|').next().unwrap_or(target).trim();
        if !target.is_empty() {
            out.push(target.to_owned());
        }
        rest = &rest[start + 2 + len + 2..];
    }
    out
}

fn parse_node(name: String, path: PathBuf, text: &str) -> Node {
    let (fm, body) = makedo_types::frontmatter(text).unwrap_or_else(|| (BTreeMap::new(), text));
    let mut edges = Vec::new();
    let mut links = Vec::new();
    for line in body.lines() {
        // типизированное ребро: `key:: [[A]], [[B]]`
        if let Some((key, rhs)) = line.split_once("::") {
            let key = key.trim();
            if !key.is_empty() && !key.contains(' ') && rhs.contains("[[") {
                for target in wikilinks(rhs) {
                    edges.push((key.to_owned(), target));
                }
                continue;
            }
        }
        links.extend(wikilinks(line));
    }
    let description = fm.get("description").cloned().unwrap_or_else(|| {
        body.lines()
            .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .unwrap_or("")
            .to_owned()
    });
    Node {
        name,
        path,
        node_type: fm.get("type").cloned().unwrap_or_default(),
        description,
        body: body.to_owned(),
        edges,
        links,
    }
}

/// Читает одну ноду. Имя ноды = имя файла без расширения.
pub fn load_node(path: &Path) -> Result<Node, WikiError> {
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let text = std::fs::read_to_string(path).map_err(|e| WikiError::Io {
        path: path.to_owned(),
        source: e,
    })?;
    Ok(parse_node(name, path.to_owned(), &text))
}

// ---------------------------------------------------------------- граф

impl Graph {
    /// Загружает все `*.md` из vault. Имя ноды = имя файла без расширения.
    pub fn load(vault: &Path) -> Result<Self, WikiError> {
        if !vault.is_dir() {
            return Err(WikiError::NotFound(vault.to_owned()));
        }
        let mut nodes = BTreeMap::new();
        for path in md_files(vault) {
            let node = load_node(&path)?;
            nodes.insert(node.name.clone(), node);
        }
        Ok(Self { nodes })
    }

    /// Инварианты trace-check: мёртвые ссылки = ошибки, ноды без типа = warnings.
    #[must_use]
    pub fn check(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        for node in self.nodes.values() {
            for (_, target) in &node.edges {
                if !self.nodes.contains_key(target) {
                    issues.push(Issue::DeadLink {
                        from: node.name.clone(),
                        to: target.clone(),
                    });
                }
            }
            for target in &node.links {
                if !self.nodes.contains_key(target) {
                    issues.push(Issue::DeadLink {
                        from: node.name.clone(),
                        to: target.clone(),
                    });
                }
            }
            if node.node_type.is_empty() {
                issues.push(Issue::Untyped {
                    node: node.name.clone(),
                });
            }
        }
        issues
    }

    /// Входящие рёбра ноды: (kind, from).
    #[must_use]
    pub fn edges_in(&self, name: &str) -> Vec<(String, String)> {
        self.nodes
            .values()
            .flat_map(|n| {
                n.edges
                    .iter()
                    .filter(|(_, t)| t == name)
                    .map(|(k, _)| (k.clone(), n.name.clone()))
            })
            .collect()
    }

    /// BFS по рёбрам вида `edge` от `from`, до `depth` шагов.
    #[must_use]
    pub fn trace(&self, from: &str, edge: &str, depth: usize) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        let mut frontier = vec![from.to_owned()];
        let mut seen: std::collections::BTreeSet<String> = frontier.iter().cloned().collect();
        for level in 1..=depth {
            let mut next = Vec::new();
            for name in &frontier {
                if let Some(node) = self.nodes.get(name) {
                    for (kind, target) in &node.edges {
                        if kind == edge && seen.insert(target.clone()) {
                            out.push((level, target.clone()));
                            next.push(target.clone());
                        }
                    }
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        out
    }
}

// ---------------------------------------------------------------- кэш индекса

/// Обход .md в vault БЕЗ учёта .gitignore/скрытости: вики — внутренние данные
/// makedo в `.makedo/` (скрытая, часто gitignored папка), их нужно читать всегда.
pub fn md_files(vault: &Path) -> impl Iterator<Item = PathBuf> {
    ignore::WalkBuilder::new(vault)
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .parents(false)
        .build()
        .flatten()
        .map(ignore::DirEntry::into_path)
        .filter(|path| path.extension().is_some_and(|e| e == "md"))
}

/// Дешёвый fingerprint vault: сумма (mtime, len) всех .md.
fn fingerprint(vault: &Path) -> u64 {
    let mut h: u64 = 0;
    for path in md_files(vault) {
        if let Ok(meta) = path.metadata() {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            h = h
                .wrapping_mul(31)
                .wrapping_add(mtime)
                .wrapping_mul(31)
                .wrapping_add(meta.len());
        }
    }
    h
}

/// Граф, пересборка только при изменении vault (PLAN: никаких
/// пересборок на запрос — анти-паттерн grok-build).
#[derive(Debug)]
pub struct WikiIndex {
    pub graph: Graph,
    fp: u64,
}

impl WikiIndex {
    pub fn build(vault: &Path) -> Result<Self, WikiError> {
        Ok(Self {
            graph: Graph::load(vault)?,
            fp: fingerprint(vault),
        })
    }

    /// `true` = vault изменился, индекс устарел.
    #[must_use]
    pub fn stale(&self, vault: &Path) -> bool {
        fingerprint(vault) != self.fp
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn vault() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "makedo-wiki-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("FR-1.md"),
            "---\ntype: fr\ndescription: Odds ingestion requirement\n---\n\ndesigned-in:: [[Feed Service]]\n",
        )
        .unwrap();
        std::fs::write(
            d.join("Feed Service.md"),
            "---\ntype: module\n---\n\nIngests odds feed.\n\nsatisfies:: [[FR-1]]\nimplemented-by:: [[Missing Node]]\nSee also [[FR-1]].\n",
        )
        .unwrap();
        std::fs::write(d.join("untyped.md"), "no frontmatter here\n").unwrap();
        d
    }

    #[test]
    fn parses_frontmatter_edges_links() {
        let g = Graph::load(&vault()).unwrap();
        let fs = &g.nodes["Feed Service"];
        assert_eq!(fs.node_type, "module");
        assert_eq!(fs.description, "Ingests odds feed.");
        assert!(fs.edges.contains(&("satisfies".into(), "FR-1".into())));
        assert!(fs.links.contains(&"FR-1".to_owned()));
        let fr = &g.nodes["FR-1"];
        assert_eq!(fr.description, "Odds ingestion requirement");
    }

    #[test]
    fn check_finds_dead_links_and_untyped() {
        let g = Graph::load(&vault()).unwrap();
        let issues = g.check();
        assert!(issues.contains(&Issue::DeadLink {
            from: "Feed Service".into(),
            to: "Missing Node".into()
        }));
        assert!(issues.contains(&Issue::Untyped {
            node: "untyped".into()
        }));
        // валидные ссылки не флагаются
        assert!(
            !issues
                .iter()
                .any(|i| matches!(i, Issue::DeadLink { to, .. } if to == "FR-1"))
        );
    }

    #[test]
    fn trace_and_edges_in() {
        let g = Graph::load(&vault()).unwrap();
        let hits = g.trace("FR-1", "designed-in", 2);
        assert_eq!(hits, vec![(1, "Feed Service".to_owned())]);
        let incoming = g.edges_in("FR-1");
        assert!(incoming.contains(&("satisfies".into(), "Feed Service".into())));
    }

    #[test]
    fn index_caches_by_fingerprint() {
        let v = vault();
        let idx = WikiIndex::build(&v).unwrap();
        assert!(!idx.stale(&v));
        // sync-тест, диск-mtime секундной точности; disallowed-methods метит только async-код
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(v.join("new.md"), "---\ntype: concept\n---\nfresh\n").unwrap();
        assert!(idx.stale(&v));
    }
}

#[cfg(test)]
mod repro {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn loads_wiki_under_gitignored_hidden_makedo_dir() {
        // воспроизводим реальную раскладку: проект гитигнорит .makedo, вики внутри.
        let root = std::env::temp_dir().join(format!("makedo-wiki-ignore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let vault = root.join(".makedo").join("wiki");
        std::fs::create_dir_all(&vault).unwrap();
        // РЕАЛЬНАЯ раскладка: это git-репо, которое гитигнорит .makedo/
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), ".makedo/\n").unwrap();
        // и сам .makedo/.gitignore (как у нас)
        std::fs::write(
            root.join(".makedo").join(".gitignore"),
            "worktrees/\nspill/\nsessions/\n",
        )
        .unwrap();
        std::fs::write(
            vault.join("index.md"),
            "---\ntype: index\ndescription: Project map\n---\n\n# Map\n",
        )
        .unwrap();
        std::fs::write(
            vault.join("PokerDomain.md"),
            "---\ntype: module\ndescription: core rules\n---\n\nSwift SPM.\n",
        )
        .unwrap();

        let g = Graph::load(&vault).unwrap();
        assert!(
            g.nodes.contains_key("index") && g.nodes.contains_key("PokerDomain"),
            "wiki-ноды под .gitignored .makedo не загрузились: {:?}",
            g.nodes.keys().collect::<Vec<_>>()
        );
    }
}
