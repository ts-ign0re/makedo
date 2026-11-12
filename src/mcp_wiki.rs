//! `makedo mcp-serve` — stdio MCP-сервер с тулами знаний (`memory_*` /
//! `wiki_node` / `wiki_trace`) текущего проекта. Нужен CLI-делегатам
//! (`claude -p --mcp-config`): гибрид «makedo-промпт + makedo-wiki» поверх чужого
//! агентского цикла. Протокол — JSON-RPC 2.0 построчно, подмножество MCP:
//! initialize / tools/list / tools/call / ping.

use std::io::{BufRead as _, Write as _};
use std::path::Path;

use makedo_tools::ToolCtx;
use serde_json::{Value, json};

/// Крутит сервер до EOF stdin (родитель-CLI закрывает пайп по завершении).
pub async fn serve(cwd: &Path) -> anyhow::Result<()> {
    let data = makedo_config::data_dir(cwd);
    let mut registry = makedo_tools::registry::Registry::default();
    makedo_wiki::tools::register_all(&mut registry, data.join("wiki"))?;
    let memory = makedo_memory::Memory::open(cwd, makedo_config::load(cwd)?.memory)?;
    memory.import_wiki()?;
    makedo_memory::tools::register(&mut registry, memory.start_session("mcp", &[])?)?;
    let ctx = ToolCtx::new(cwd.to_path_buf(), data.join("spill"));

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue; // мусор в пайпе — молча пропускаем
        };
        let id = req["id"].clone();
        let method = req["method"].as_str().unwrap_or_default();
        let Some(result) = handle(method, &req["params"], &registry, &ctx).await else {
            continue; // нотификация или незнакомый метод без id
        };
        if id.is_null() {
            continue; // нотификации не требуют ответа
        }
        let reply = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((code, msg)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
            }
        };
        writeln!(stdout, "{reply}")?;
        stdout.flush()?;
    }
    Ok(())
}

type RpcResult = Result<Value, (i64, String)>;

async fn handle(
    method: &str,
    params: &Value,
    registry: &makedo_tools::registry::Registry,
    ctx: &ToolCtx,
) -> Option<RpcResult> {
    match method {
        "initialize" => Some(Ok(json!({
            // эхо версии клиента: сервер не зависит от конкретной ревизии MCP
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2024-11-05"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "makedo-wiki", "version": env!("CARGO_PKG_VERSION")},
        }))),
        "ping" => Some(Ok(json!({}))),
        "tools/list" => {
            let tools: Vec<Value> = registry
                .definitions()
                .into_iter()
                .map(|d| {
                    json!({
                        "name": d.name,
                        "description": d.description,
                        "inputSchema": d.input_schema,
                    })
                })
                .collect();
            Some(Ok(json!({"tools": tools})))
        }
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default();
            let Some(tool) = registry.get(name) else {
                return Some(Err((-32602, format!("unknown tool `{name}`"))));
            };
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            // ошибки тулов — model-facing текст в isError-результате, не RPC-ошибка
            let (text, is_error) = match tool.run_json(ctx, args).await {
                Ok(out) => (out, false),
                Err(e) => (e.to_string(), true),
            };
            Some(Ok(json!({
                "content": [{"type": "text", "text": text}],
                "isError": is_error,
            })))
        }
        // notifications/* и незнакомые методы: нотификации игнорируем, а на
        // запросы с id отвечает вызывающая сторона (см. serve) — но метод нам
        // неизвестен, поэтому method-not-found
        m if m.starts_with("notifications/") => None,
        _ => Some(Err((-32601, format!("method not found: {method}")))),
    }
}
