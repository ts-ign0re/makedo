//! Субагенты и worktree-тулы (PLAN §6, точка возврата наступила):
//! - `task` — дочерняя сессия: `research` (read-only тулы, тот же cwd) или
//!   `edit` (полный набор, изолированный git worktree `.makedo/worktrees/<name>`);
//! - `worktree_create` / `worktree_finish` — управление фиче-ветками из агента
//!   (то же, что CLI `makedo finish`, но как тулы).
//!
//! Ребёнок не получает `task`/worktree-тулов — вложенности нет. Ruleset ребёнка
//! `allow_all`: у research-набора нечего одобрять (read-only), edit изолирован
//! worktree-веткой, merge — только явный `worktree_finish` родителя.
// ponytail: per-task провайдер (research дёшево, edit дорого) — при реальном спросе

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use makedo_session::{SessionCommand, SessionConfig};
use makedo_tools::registry::Registry;
use makedo_tools::{Tool, ToolCtx, ToolError};
use makedo_types::event::SessionEvent;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::worktree;

/// Потолок одного субагента — защита от зависшего ребёнка.
const TASK_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(30);

/// Всё, что нужно для сборки дочерней сессии (снимок родительской конфигурации).
#[derive(Debug, Clone)]
pub struct AgentCtx {
    pub root: PathBuf,
    /// Parent wiki for read-only research; edit children use their own checkout's wiki.
    pub wiki: PathBuf,
    pub spill_dir: PathBuf,
    pub provider_cfg: makedo_config::ProviderConfig,
    pub memory_config: makedo_config::MemoryConfig,
    pub system_prompt: String,
    pub model_info: Option<makedo_providers::catalog::ModelInfo>,
    pub context_window: Option<u64>,
    pub capture_bytes: Option<usize>,
    pub shim_dir: Option<PathBuf>,
    pub concurrency: usize,
    /// Verification gate проекта — edit-субагент проверяет себя в своём worktree.
    pub verify_cmds: Vec<String>,
}

pub fn register(
    r: &mut Registry,
    ctx: AgentCtx,
) -> Result<(), makedo_tools::registry::RegistryError> {
    let ctx = Arc::new(ctx);
    r.register(TaskTool {
        ctx: Arc::clone(&ctx),
    })?;
    r.register(WorktreeCreateTool {
        root: ctx.root.clone(),
    })?;
    r.register(WorktreeFinishTool {
        root: ctx.root.clone(),
    })?;
    Ok(())
}

// ------------------------------------------------------------------- task

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskArgs {
    /// Short display name for the agent panel, e.g. "code-review" or "UI-аудит".
    pub name: Option<String>,
    /// "research" — read-only investigation in the current tree;
    /// "edit" — full toolset in an isolated git worktree.
    pub kind: String,
    /// Full task for the subagent: context, goal, expected report.
    pub prompt: String,
    /// Worktree/branch name (required for kind="edit", e.g. "fix-betslip").
    pub worktree: Option<String>,
}

#[derive(Debug)]
pub struct TaskTool {
    ctx: Arc<AgentCtx>,
}

impl Tool for TaskTool {
    type Args = TaskArgs;

    fn name(&self) -> &'static str {
        "task"
    }

    fn description(&self) -> &'static str {
        "Spawn a subagent. kind=research: read-only tools (read/grep/glob/wiki), \
         investigates and returns a report without touching files — use to gather \
         information without polluting your context. kind=edit: full toolset in an \
         isolated git worktree (requires `worktree` name); its changes land on \
         branch makedo/<name> — merge them later with worktree_finish. \
         The subagent sees only your prompt: include all needed context. \
         Before edit tasks load the worktrees skill. One active owner per worktree; \
         the parent reviews the diff and checks before an explicitly authorized finish."
    }

    fn is_read_only(&self) -> bool {
        false // спавн ребёнка = токены и (для edit) правки — пусть спрашивает
    }

    // линейный сценарий: валидация → реестр → сессия → сбор событий
    #[allow(clippy::too_many_lines)]
    async fn run(&self, ctx: &ToolCtx, args: TaskArgs) -> Result<String, ToolError> {
        let agent_id = ctx
            .progress
            .as_ref()
            .map_or_else(String::new, |p| p.call_id.clone());
        ctx.emit(SessionEvent::AgentStarted {
            id: agent_id.clone(),
            name: args
                .name
                .clone()
                .or_else(|| args.worktree.clone())
                .unwrap_or_else(|| args.kind.clone()),
            worktree: args.worktree.clone(),
        })
        .await;
        let ctx_a = &self.ctx;
        let (cwd, role) = match args.kind.as_str() {
            "research" => (
                ctx_a.root.clone(),
                "You are a research subagent: investigate and report. You have \
                 read-only tools; do not attempt to modify anything. End with a \
                 dense, factual report — it is your only output channel.",
            ),
            "edit" => {
                let name = args
                    .worktree
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .ok_or_else(|| {
                        ToolError::InvalidArgs("kind=edit requires `worktree` name".into())
                    })?;
                let root = ctx_a.root.clone();
                let name_owned = name.to_owned();
                let dir = tokio::task::spawn_blocking(move || worktree::start(&root, &name_owned))
                    .await
                    .map_err(|e| ToolError::Failed(format!("worktree task: {e}")))?
                    .map_err(|e| ToolError::Failed(format!("worktree: {e}")))?;
                (
                    dir,
                    "You are an edit subagent working in an isolated git worktree. \
                     Implement the task; changes stay on this feature branch until \
                     the parent merges them. End with a summary of what you changed.",
                )
            }
            other => {
                return Err(ToolError::InvalidArgs(format!(
                    "unknown kind `{other}` (use research | edit)"
                )));
            }
        };

        let mut worktree_lease = if args.kind == "edit" {
            Some(
                worktree::lease(&ctx_a.root, args.worktree.as_deref().unwrap_or_default())
                    .map_err(|e| ToolError::Failed(e.to_string()))?,
            )
        } else {
            None
        };

        // дочерний реестр: без task/worktree — вложенных субагентов нет
        let mut registry = Registry::default();
        let reg = if args.kind == "research" {
            registry
                .register(makedo_tools::builtin::fs::ReadTool)
                .and_then(|()| registry.register(makedo_tools::builtin::search::GrepTool))
                .and_then(|()| registry.register(makedo_tools::builtin::search::GlobTool))
                .and_then(|()| registry.register(makedo_tools::builtin::todo::TodoTool))
        } else {
            makedo_tools::builtin::register_all(&mut registry)
        };
        reg.map_err(|e| ToolError::Failed(format!("child registry: {e}")))?;
        let child_wiki = if args.kind == "edit" {
            let data = cwd.join(".makedo");
            if !makedo_wiki::seed::exists(&data) {
                makedo_wiki::seed::seed(&cwd, &data, |_| {})
                    .map_err(|e| ToolError::Failed(format!("child wiki seed: {e}")))?;
            }
            cwd.join(".makedo/wiki")
        } else {
            ctx_a.wiki.clone()
        };
        makedo_wiki::tools::register_all(&mut registry, child_wiki.clone())
            .map_err(|e| ToolError::Failed(format!("child wiki tools: {e}")))?;
        let memory = makedo_memory::Memory::open(&cwd, ctx_a.memory_config.clone())
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        memory
            .import_wiki()
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        let memory_brief = memory
            .brief()
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        let memory_session = memory
            .start_session("subagent", &[])
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        makedo_memory::tools::register(&mut registry, memory_session.clone())
            .map_err(|e| ToolError::Failed(e.to_string()))?;

        makedo_skills::tools::register(
            &mut registry,
            makedo_skills::load(
                makedo_config::global_dir().as_deref(),
                &cwd,
                makedo_config::PonytailLevel::Full,
                makedo_config::Doctrine::None,
                false,
            ),
        )
        .map_err(|e| ToolError::Failed(format!("child skill tools: {e}")))?;

        let provider = makedo_providers::from_config(&ctx_a.provider_cfg)
            .map_err(|e| ToolError::Failed(format!("child provider: {e}")))?;
        let cfg = SessionConfig {
            provider: ctx_a.provider_cfg.clone(),
            system_prompt: format!(
                "{}{memory_brief}\n\n# Subagent role\n{role}\nYour working directory is {}. Edit only this checkout. For multi-step work use todo_write with short step labels and keep the current step updated; the user sees this plan in the agent panel. Report changed files, verification outcomes and full log paths. Do not merge or push; the parent owns integration.",
                ctx_a.system_prompt,
                cwd.display()
            ),
            base_system_prompt: ctx_a.system_prompt.clone(),
            ponytail: makedo_config::PonytailLevel::Off, // промпт уже собран родителем
            doctrine: makedo_config::Doctrine::None,
            triz: false, // промпт уже собран родителем
            global_dir: makedo_config::global_dir(),
            cwd,
            spill_dir: ctx_a.spill_dir.clone(),
            concurrency: ctx_a.concurrency,
            shim_dir: ctx_a.shim_dir.clone(),
            context_window: ctx_a.context_window,
            model_info: ctx_a.model_info,
            capture_bytes: ctx_a.capture_bytes,
            session_file: None,
            memory: Some(memory_session),
            permissions_file: None, // и одобрений не персистят
            verify_cmds: ctx_a.verify_cmds.clone(),
        };
        let mut handle = makedo_session::spawn(
            provider,
            Arc::new(registry),
            makedo_session::permission::Ruleset::allow_all(),
            cfg,
        );
        handle
            .commands
            .send(SessionCommand::Prompt(args.prompt))
            .await
            .map_err(|e| ToolError::Failed(format!("child prompt: {e}")))?;

        let mut out = String::new();
        let mut child_failed = false;
        let mut child_finished = false;
        let collect = async {
            while let Some(ev) = handle.events.recv().await {
                if matches!(
                    &ev,
                    SessionEvent::PhaseChanged(_)
                        | SessionEvent::ToolQueued { .. }
                        | SessionEvent::ToolStarted { .. }
                        | SessionEvent::ToolFinished { .. }
                        | SessionEvent::ToolLog { .. }
                        | SessionEvent::TodoUpdated(_)
                        | SessionEvent::Error(_)
                        | SessionEvent::TurnFinished { .. }
                ) {
                    ctx.emit(SessionEvent::AgentEvent {
                        id: agent_id.clone(),
                        event: Box::new(ev.clone()),
                    })
                    .await;
                }
                match ev {
                    SessionEvent::TextDelta(t) => out.push_str(&t),
                    SessionEvent::Error(e) => {
                        let _ = write!(out, "\n[subagent error] {e}");
                        child_failed = true;
                    }
                    SessionEvent::ToolLog { call_id, path, .. } => {
                        let _ = write!(out, "\n[subagent log] {call_id}: {path}\n");
                    }
                    SessionEvent::TurnFinished { usage, cancelled } => {
                        child_finished = true;
                        child_failed |= cancelled;
                        let _ = write!(
                            out,
                            "\n\n[subagent usage] in={} out={} cache_read={}",
                            usage.input_tokens, usage.output_tokens, usage.cache_read_tokens
                        );
                        break;
                    }
                    _ => {}
                }
            }
        };
        let stopped = tokio::select! {
            () = collect => None,
            () = ctx.cancel.cancelled() => Some("subagent cancelled"),
            () = tokio::time::sleep(TASK_TIMEOUT) => Some("subagent timed out"),
        };
        if stopped.is_some() {
            let _ = handle.commands.send(SessionCommand::Cancel).await;
        }
        // Keep ownership until the child actor and its tool cleanup have actually stopped.
        drop(handle.commands);
        let cleanup = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while handle.events.recv().await.is_some() {}
        })
        .await;
        if cleanup.is_err() {
            let lease = worktree_lease.take();
            tokio::spawn(async move {
                let _lease = lease;
                while handle.events.recv().await.is_some() {}
            });
            return Err(ToolError::Failed(
                "child is still stopping; worktree remains locked".into(),
            ));
        }
        if let Some(reason) = stopped {
            return Err(ToolError::Failed(format!(
                "{reason}; partial report:\n{out}"
            )));
        }
        if !child_finished {
            return Err(ToolError::Failed(format!(
                "child session ended without a completion event; partial report:\n{out}"
            )));
        }
        if child_failed {
            return Err(ToolError::Failed(out));
        }
        if args.kind == "edit"
            && let Some(name) = &args.worktree
        {
            let _ = write!(
                out,
                "\n[edit subagent done] changes are on branch makedo/{name} \
                 (worktree .makedo/worktrees/{name}); merge with worktree_finish."
            );
        }
        Ok(out)
    }
}

// ------------------------------------------------------- worktree_create

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorktreeCreateArgs {
    /// Feature name; becomes branch makedo/<name> and dir .makedo/worktrees/<name>.
    pub name: String,
}

#[derive(Debug)]
pub struct WorktreeCreateTool {
    root: PathBuf,
}

impl Tool for WorktreeCreateTool {
    type Args = WorktreeCreateArgs;

    fn name(&self) -> &'static str {
        "worktree_create"
    }

    fn description(&self) -> &'static str {
        "Prepare an isolated git worktree at committed HEAD: branch makedo/<name>, \
         dir .makedo/worktrees/<name>. Name uses ASCII letters/digits/-/_. Returns the path; \
         edit through task(kind=edit) or makedo --worktree to acquire ownership. \
         Never switch the main checkout. Load the worktrees skill first."
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn run(&self, _ctx: &ToolCtx, args: WorktreeCreateArgs) -> Result<String, ToolError> {
        let root = self.root.clone();
        let dir = tokio::task::spawn_blocking(move || worktree::start(&root, &args.name))
            .await
            .map_err(|e| ToolError::Failed(format!("worktree task: {e}")))?
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        Ok(dir.display().to_string())
    }
}

// ------------------------------------------------------- worktree_finish

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorktreeFinishArgs {
    /// Feature name previously created via `worktree_create` / `task(kind=edit)`.
    pub name: String,
}

#[derive(Debug)]
pub struct WorktreeFinishTool {
    root: PathBuf,
}

impl Tool for WorktreeFinishTool {
    type Args = WorktreeFinishArgs;

    fn name(&self) -> &'static str {
        "worktree_finish"
    }

    fn description(&self) -> &'static str {
        "Merge the feature worktree back: commits leftover changes on makedo/<name>, \
         merges into the main branch (--no-ff) and removes the worktree. On merge \
         conflict the integration preview fails without changing the main checkout. \
         Requires a clean main checkout on the recorded base branch and no active owner. \
         Logs/history keep the worktree from being deleted. Load the worktrees skill first."
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn run(&self, _ctx: &ToolCtx, args: WorktreeFinishArgs) -> Result<String, ToolError> {
        let root = self.root.clone();
        let name = args.name.clone();
        let removed = tokio::task::spawn_blocking(move || worktree::finish(&root, &name))
            .await
            .map_err(|e| ToolError::Failed(format!("worktree task: {e}")))?
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        Ok(format!(
            "merged makedo/{}; {}",
            args.name,
            if removed {
                "worktree removed"
            } else {
                "worktree retained to preserve logs/history"
            }
        ))
    }
}
