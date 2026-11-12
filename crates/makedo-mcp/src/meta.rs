//! Meta-tools: `search_tool` + `use_tool` — MCP-схемы не попадают в манифест
//! модели (стабильный KV-кэш, экономия контекста — механика grok-build).
//! Чиним оба их бага: BM25 кэшируется по generation; `total_hidden_tools` —
//! действительно скрытые, не все.

use std::fmt::Write as _;
use std::sync::Arc;

use makedo_tools::registry::{Registry, RegistryError};
use makedo_tools::{Tool, ToolCtx, ToolError};
use makedo_types::QualifiedToolName;
use makedo_types::bm25::Bm25Index;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::{CatalogTool, McpManager};

/// Регистрирует meta-tools. Если MCP-серверов нет — не регистрирует ничего
/// (пустой каталог не заслуживает двух тулов в манифесте).
pub fn register_all(r: &mut Registry, manager: Arc<McpManager>) -> Result<(), RegistryError> {
    if manager.servers.is_empty() {
        return Ok(());
    }
    let index = Arc::new(Mutex::new(None));
    r.register(SearchTool {
        manager: Arc::clone(&manager),
        index,
    })?;
    r.register(UseTool { manager })?;
    Ok(())
}

/// Кэш BM25 по generation каталога.
type IndexCache = Arc<Mutex<Option<(Arc<Vec<CatalogTool>>, Arc<Bm25Index>)>>>;

// ---------------------------------------------------------------- search_tool

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// What capability you need (e.g. "create github issue", "query database")
    pub query: String,
    /// Max results (default 5)
    pub limit: Option<usize>,
}

pub struct SearchTool {
    manager: Arc<McpManager>,
    index: IndexCache,
}

impl std::fmt::Debug for SearchTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchTool").finish_non_exhaustive()
    }
}

impl Tool for SearchTool {
    type Args = SearchArgs;

    fn name(&self) -> &'static str {
        "search_tool"
    }

    fn description(&self) -> &'static str {
        "Search the catalog of connected MCP server tools by capability. \
         Returns tool names, descriptions and input schemas. \
         Call the found tool with use_tool."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn run(&self, _ctx: &ToolCtx, args: SearchArgs) -> Result<String, ToolError> {
        let catalog = self.manager.catalog().await;
        let status = if self.manager.any_connecting() {
            "partial (some servers still connecting)"
        } else {
            "ready"
        };
        if catalog.is_empty() {
            return Ok(format!("No MCP tools available. Status: {status}."));
        }
        // BM25 с кэшем: пересборка только при смене каталога (Arc::ptr_eq)
        let bm25 = {
            let mut cache = self.index.lock().await;
            match cache.as_ref() {
                Some((cat, idx)) if Arc::ptr_eq(cat, &catalog) => Arc::clone(idx),
                _ => {
                    let docs: Vec<String> = catalog
                        .iter()
                        .map(|t| format!("{} {}", t.qualified.as_str(), t.description))
                        .collect();
                    let idx = Arc::new(Bm25Index::build(docs.iter().map(String::as_str)));
                    *cache = Some((Arc::clone(&catalog), Arc::clone(&idx)));
                    idx
                }
            }
        };
        let limit = args.limit.unwrap_or(5).min(20);
        // exact-match fast path: модель часто уже знает имя
        let exact = catalog.iter().find(|t| t.qualified.as_str() == args.query);
        let hits: Vec<&CatalogTool> = if let Some(t) = exact {
            vec![t]
        } else {
            bm25.query(&args.query, limit)
                .into_iter()
                .map(|(doc, _)| &catalog[doc as usize])
                .collect()
        };
        if hits.is_empty() {
            return Ok(format!(
                "No MCP tools matched. {} tools exist. Status: {status}.",
                catalog.len()
            ));
        }
        let hidden = catalog.len() - hits.len(); // именно скрытые (фикс off-by-everything grok-build)
        let mut out = String::new();
        for t in &hits {
            let _ = writeln!(
                out,
                "## {}\n{}\ninput schema: {}",
                t.qualified.as_str(),
                t.description,
                serde_json::to_string(&t.input_schema).unwrap_or_default()
            );
        }
        let _ = writeln!(out, "\nStatus: {status}. {hidden} more tools not shown.");
        Ok(out)
    }
}

// ---------------------------------------------------------------- use_tool

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UseArgs {
    /// Qualified tool name from `search_tool` (format: `server__tool`)
    pub tool_name: String,
    /// Arguments matching the tool's input schema
    pub tool_input: serde_json::Value,
}

pub struct UseTool {
    manager: Arc<McpManager>,
}

impl std::fmt::Debug for UseTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UseTool").finish_non_exhaustive()
    }
}

impl Tool for UseTool {
    type Args = UseArgs;

    fn name(&self) -> &'static str {
        "use_tool"
    }

    fn description(&self) -> &'static str {
        "Invoke an MCP server tool found via search_tool. \
         tool_name is the qualified `server__tool` name."
    }

    async fn run(&self, ctx: &ToolCtx, args: UseArgs) -> Result<String, ToolError> {
        let qualified = QualifiedToolName::parse(&args.tool_name).map_err(|_| {
            // native-tool correction (grok-build): неквалифицированное имя = встроенный тул
            ToolError::InvalidArgs(format!(
                "`{}` is not a qualified MCP tool name (server__tool). \
                 If it is a built-in tool, call it directly instead of via use_tool.",
                args.tool_name
            ))
        })?;
        self.manager
            .call_with_cancel(&qualified, args.tool_input, &ctx.cancel)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))
    }
}
