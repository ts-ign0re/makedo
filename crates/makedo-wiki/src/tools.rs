//! Wiki-тулы модели: `wiki_node`, `wiki_trace` (поиск — `memory_search`). Индекс общий,
//! пересборка — только при изменении vault (fingerprint), в `spawn_blocking`.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use makedo_tools::registry::{Registry, RegistryError};
use makedo_tools::{Tool, ToolCtx, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::WikiIndex;

/// Общий кэш индекса между тулами.
#[derive(Debug, Clone)]
pub struct SharedWiki {
    vault: PathBuf,
    index: Arc<Mutex<Option<Arc<WikiIndex>>>>,
}

impl SharedWiki {
    #[must_use]
    pub fn new(vault: PathBuf) -> Self {
        Self {
            vault,
            index: Arc::new(Mutex::new(None)),
        }
    }

    // guard намеренно живёт до записи свежего индекса — это single-flight пересборки
    #[allow(clippy::significant_drop_tightening)]
    async fn current(&self) -> Result<Arc<WikiIndex>, ToolError> {
        let mut guard = self.index.lock().await;
        let fresh = match guard.as_ref() {
            Some(idx) if !idx.stale(&self.vault) => return Ok(Arc::clone(idx)),
            _ => {
                let vault = self.vault.clone();
                tokio::task::spawn_blocking(move || WikiIndex::build(&vault))
                    .await
                    .map_err(|e| ToolError::Failed(format!("wiki index task: {e}")))?
                    .map_err(|e| ToolError::Failed(e.to_string()))?
            }
        };
        let fresh = Arc::new(fresh);
        *guard = Some(Arc::clone(&fresh));
        Ok(fresh)
    }
}

/// Регистрирует все wiki-тулы поверх общего кэша.
pub fn register_all(r: &mut Registry, vault: PathBuf) -> Result<(), RegistryError> {
    let shared = SharedWiki::new(vault);
    r.register(NodeTool {
        wiki: shared.clone(),
    })?;
    r.register(TraceTool { wiki: shared })?;
    Ok(())
}

// ---------------------------------------------------------------- wiki_node

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NodeArgs {
    /// Exact node name (the title of a `memory_search` wiki hit)
    pub name: String,
}

#[derive(Debug)]
pub struct NodeTool {
    wiki: SharedWiki,
}

impl Tool for NodeTool {
    type Args = NodeArgs;

    fn name(&self) -> &'static str {
        "wiki_node"
    }

    fn description(&self) -> &'static str {
        "Read a wiki node: full content plus its typed edges (outgoing and incoming)."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn run(&self, _ctx: &ToolCtx, args: NodeArgs) -> Result<String, ToolError> {
        let idx = self.wiki.current().await?;
        let Some(node) = idx.graph.nodes.get(&args.name) else {
            return Err(ToolError::Failed(format!(
                "No wiki node named `{}`. Use memory_search (kind=wiki) to find the right name.",
                args.name
            )));
        };
        let mut out = format!(
            "# {} [{}]\npath: {}\n\n{}\n",
            node.name,
            node.node_type,
            node.path.display(),
            node.body.trim()
        );
        let incoming = idx.graph.edges_in(&node.name);
        if !incoming.is_empty() {
            out.push_str("\nIncoming edges:\n");
            for (kind, from) in incoming {
                let _ = writeln!(out, "- {from} —{kind}→ this");
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- wiki_trace

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TraceArgs {
    /// Start node name
    pub from: String,
    /// Edge kind to follow (e.g. "implemented-by", "depends-on", "satisfies")
    pub edge: String,
    /// Max BFS depth (default 3)
    pub depth: Option<usize>,
}

#[derive(Debug)]
pub struct TraceTool {
    wiki: SharedWiki,
}

impl Tool for TraceTool {
    type Args = TraceArgs;

    fn name(&self) -> &'static str {
        "wiki_trace"
    }

    fn description(&self) -> &'static str {
        "Follow typed edges through the wiki graph from a node (BFS). \
         E.g. trace `implemented-by` from a requirement to find its code."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn run(&self, _ctx: &ToolCtx, args: TraceArgs) -> Result<String, ToolError> {
        let idx = self.wiki.current().await?;
        if !idx.graph.nodes.contains_key(&args.from) {
            return Err(ToolError::Failed(format!(
                "No wiki node named `{}`. Use memory_search (kind=wiki) first.",
                args.from
            )));
        }
        let depth = args.depth.unwrap_or(3).min(10);
        let hits = idx.graph.trace(&args.from, &args.edge, depth);
        if hits.is_empty() {
            return Ok(format!(
                "No `{}` edges from `{}` (checked {depth} levels).",
                args.edge, args.from
            ));
        }
        let mut out = String::new();
        for (level, name) in hits {
            let desc = idx
                .graph
                .nodes
                .get(&name)
                .map_or("", |n| n.description.as_str());
            let _ = writeln!(out, "{}{name} — {desc}", "  ".repeat(level - 1));
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
            "makedo-wikitools-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        let vault = d.join(".makedo/wiki");
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::write(
            vault.join("FR-1.md"),
            "---\ntype: fr\ndescription: Odds ingestion\n---\nimplemented-by:: [[Feed Service]]\n",
        )
        .unwrap();
        std::fs::write(
            vault.join("Feed Service.md"),
            "---\ntype: module\ndescription: Ingests the odds feed\n---\nimplemented-by:: [[Parser]]\n",
        )
        .unwrap();
        std::fs::write(
            vault.join("Parser.md"),
            "---\ntype: module\ndescription: V3 XML parser\n---\nbody\n",
        )
        .unwrap();
        let ctx = ToolCtx::new(d.clone(), d.join("spill"));
        (vault, ctx)
    }

    #[tokio::test]
    async fn node_trace_roundtrip() {
        let (vault, ctx) = setup();
        let shared = SharedWiki::new(vault);
        let node = NodeTool {
            wiki: shared.clone(),
        };
        let out = node
            .run(
                &ctx,
                NodeArgs {
                    name: "Feed Service".into(),
                },
            )
            .await
            .unwrap();
        assert!(out.contains("FR-1 —implemented-by→ this"));

        let trace = TraceTool { wiki: shared };
        let out = trace
            .run(
                &ctx,
                TraceArgs {
                    from: "FR-1".into(),
                    edge: "implemented-by".into(),
                    depth: None,
                },
            )
            .await
            .unwrap();
        assert!(out.contains("Feed Service"));
        assert!(out.contains("  Parser")); // второй уровень с отступом
    }

    #[tokio::test]
    async fn unknown_node_is_model_facing_error() {
        let (vault, ctx) = setup();
        let node = NodeTool {
            wiki: SharedWiki::new(vault),
        };
        let err = node
            .run(
                &ctx,
                NodeArgs {
                    name: "nope".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("memory_search"));
    }
}
