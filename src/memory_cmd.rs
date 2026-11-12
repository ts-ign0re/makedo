//! Human-facing access to the same memory tools used by agents and MCP.
use anyhow::{Context, bail};
use makedo_tools::ToolCtx;

pub async fn run(args: &[String]) -> anyhow::Result<()> {
    let cwd = makedo_config::project_root(&std::env::current_dir()?);
    let memory = makedo_memory::Memory::open(&cwd, makedo_config::load(&cwd)?.memory)?;
    memory.import_wiki()?;
    memory.refresh_delivery()?;
    if args.is_empty() || args[0] == "status" {
        println!(
            "{}",
            serde_json::json!({"path":memory.path,"branch":memory.branch,"embedding_model":memory.config.embedding_model,"embedding_url":memory.config.embedding_url,"latest_session":memory.latest_session()?})
        );
        return Ok(());
    }
    if args[0] == "export" {
        let id = args
            .get(1)
            .context("usage: makedo memory export <note-id>")?
            .parse::<i64>()?;
        let note = memory.read(id)?;
        if note["entry"]["kind"] != "note" {
            bail!("only notes can be exported");
        }
        let vault = makedo_config::data_dir(&cwd).join("wiki");
        std::fs::create_dir_all(&vault)?;
        let uid = note["entry"]["uid"]
            .as_str()
            .context("note has no stable identity")?;
        let file = vault.join(format!("memory-{uid}.md"));
        let text = format!(
            "---\ntype: memory\n---\n\n# {}\n\n{}\n\nAssessment: {}\nSources: {}\n\nEvidence:\n```json\n{}\n```\n",
            note["entry"]["title"].as_str().unwrap_or(""),
            note["entry"]["text"].as_str().unwrap_or(""),
            note["entry"]["assessment"].as_str().unwrap_or(""),
            note["entry"]["source"].as_str().unwrap_or(""),
            serde_json::to_string_pretty(&note["evidence"])?
        );
        std::fs::write(&file, text)?;
        println!("{}", file.display());
        return Ok(());
    }
    let (name, value) = match args[0].as_str() {
        "search" => (
            "memory_search",
            serde_json::json!({"task":args.get(1).context("usage: makedo memory search <task>")?,"all_branches":args.iter().any(|a|a=="--all-branches")}),
        ),
        "read" => (
            "memory_read",
            serde_json::json!({"id":args.get(1).context("usage: makedo memory read <id>")?.parse::<i64>()?}),
        ),
        "write" => (
            "memory_write",
            serde_json::from_str(
                args.get(1)
                    .context("usage: makedo memory write '<JSON tool arguments>'")?,
            )?,
        ),
        _ => bail!("usage: makedo memory <status|search|read|write|export>"),
    };
    let mut registry = makedo_tools::registry::Registry::default();
    makedo_memory::tools::register(&mut registry, memory.start_session("cli", &[])?)?;
    let ctx = ToolCtx::new(cwd, std::env::temp_dir());
    let tool = registry.get(name).context("memory tool registration")?;
    println!("{}", tool.run_json(&ctx, value).await?);
    Ok(())
}
