mod agent_tools;
mod image_tool;
mod mcp_wiki;
mod memory_cmd;
mod rgfd;
#[cfg(feature = "gui")]
mod window;
mod worktree;

/// println!, не паникующий на закрытом pipe (`makedo doctor | head`).
/// SIGPIPE-хендлинг без unsafe: просто игнорируем ошибку записи.
macro_rules! outln {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};

#[allow(clippy::too_many_lines)] // линейная диспетчеризация подкоманд
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("memory") => runtime()?.block_on(memory_cmd::run(&args[1..])),
        Some("demo") => {
            runtime()?.block_on(async { makedo_tui::pipeline::demo().await.map_err(Into::into) })
        }
        Some("skills") => {
            let skills = makedo_skills::load(
                None,
                &std::env::current_dir()?,
                makedo_config::PonytailLevel::Full,
                makedo_config::Doctrine::None,
                false,
            );
            if let Some(name) = args.get(1) {
                let skill = skills
                    .iter()
                    .find(|s| &s.name == name)
                    .context("unknown skill")?;
                outln!("{}", skill.body);
            } else {
                for skill in skills {
                    outln!(
                        "{}{} — {}",
                        skill.name,
                        if skill.active { " (active)" } else { "" },
                        skill.description
                    );
                }
            }
            Ok(())
        }
        Some("--version" | "-V") => {
            outln!("makedo {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        // обогащение wiki дешёвой моделью (CORE §достроить-1): headless-сессия,
        // запись разрешена ТОЛЬКО в .makedo/wiki, bash запрещён
        Some("wiki") if args.get(1).map(String::as_str) == Some("enrich") => {
            let focus = args.get(2).cloned();
            runtime()?.block_on(async move {
                let cwd = std::env::current_dir()?;
                let mut ruleset = makedo_session::permission::Ruleset::default_policy();
                for tool in ["write", "edit", "apply_patch"] {
                    if let Ok(rule) = makedo_session::permission::Rule::new(
                        tool,
                        "**/.makedo/wiki/**",
                        makedo_session::permission::Action::Allow,
                    ) {
                        ruleset.push(rule);
                    }
                }
                let (handle, _cfg, needs_setup, _global) = build_session_with(&cwd, Some(ruleset))?;
                if needs_setup {
                    eprintln!("no model configured: run `makedo` and pick /models");
                    std::process::exit(1);
                }
                let code =
                    makedo_tui::run_headless(handle, makedo_wiki::enrich_prompt(focus.as_deref()))
                        .await;
                std::process::exit(code);
            })
        }
        Some("wiki") => wiki_cmd(args.get(1).map(String::as_str)),
        Some("models") => {
            let cache = makedo_config::cache_dir();
            if args.get(1).map(String::as_str) == Some("--refresh") {
                let dir = cache.clone().context("no ~/.makedo dir")?;
                let n = runtime()?
                    .block_on(makedo_providers::catalog::refresh(&dir))
                    .context("refreshing models.dev")?;
                outln!("refreshed: {n} models cached");
            }
            let catalog = makedo_providers::catalog::load(cache.as_deref());
            let cfg = makedo_config::load(&std::env::current_dir()?)?;
            let current = makedo_providers::catalog::find_model(
                &catalog,
                Some(&cfg.provider.kind),
                &cfg.provider.model,
            )
            .map(|(pid, _)| pid.to_owned());
            for (pid, p) in catalog.iter() {
                // только провайдеры с настроенным env-ключом + канонический для текущей модели
                let has_key = p.env.iter().any(|e| {
                    std::env::var(e).is_ok() || makedo_config::credentials::get(e).is_some()
                });
                if !has_key && current.as_deref() != Some(pid) {
                    continue;
                }
                for (mid, m) in &p.models {
                    let mark = if *mid == cfg.provider.model { "*" } else { " " };
                    outln!(
                        "{mark} {pid}/{mid}  ctx={}k out={}k  ${}/{}M cache=${}",
                        m.context / 1000,
                        m.output / 1000,
                        m.cost_in,
                        m.cost_out,
                        m.cost_cache_read
                    );
                }
            }
            Ok(())
        }
        Some("login") => runtime()?.block_on(login_cmd(&args[1..])),
        Some("init") => {
            let cwd = std::env::current_dir()?;
            init_project(&cwd)?;
            Ok(())
        }
        Some("doctor") => doctor(),
        // stdio MCP-сервер wiki-тулов для CLI-делегатов (claude -p --mcp-config)
        Some("mcp-serve") => {
            let cwd = std::env::current_dir()?;
            runtime()?.block_on(mcp_wiki::serve(&cwd))
        }
        Some("rg") => {
            let pat = args.get(1).context("usage: makedo rg <pattern> [path]")?;
            std::process::exit(rgfd::rg(pat, args.get(2).map(String::as_str))?);
        }
        Some("fd") => {
            let pat = args.get(1).context("usage: makedo fd <pattern> [path]")?;
            std::process::exit(rgfd::fd(pat, args.get(2).map(String::as_str)));
        }
        Some("finish") => {
            let name = args.get(1).context("usage: makedo finish <name>")?;
            let root = std::env::current_dir()?;
            let removed = worktree::finish(&root, name)?;
            outln!(
                "merged makedo/{name}; {}",
                if removed {
                    "worktree removed"
                } else {
                    "worktree retained to preserve logs/history"
                }
            );
            Ok(())
        }
        // тот же TUI в собственном окне; только в сборке с `--features gui`
        #[cfg(feature = "gui")]
        Some("app") => window::run(&args),
        Some("-p") => {
            let prompt = args
                .get(1)
                .context("usage: makedo -p \"<prompt>\"")?
                .clone();
            runtime()?.block_on(async move {
                let (dir, _worktree_lease) = work_dir(&args)?;
                let (handle, _cfg, needs_setup, _global) = build_session(&dir)?;
                if needs_setup {
                    eprintln!(
                        "no model configured: set an API key env var (e.g. DEEPSEEK_API_KEY) \
                         or run `makedo` interactively and pick /models"
                    );
                    std::process::exit(1);
                }
                let code = makedo_tui::run_headless(handle, prompt).await;
                std::process::exit(code);
            })
        }
        _ => {
            let (dir, _worktree_lease) = work_dir(&args)?;
            runtime()?.block_on(async move {
                let (handle, cfg, needs_setup, global_data) = build_session(&dir)?;
                makedo_tui::run(
                    handle,
                    &cfg.tui,
                    needs_setup,
                    global_data,
                    cfg.provider.model.clone(),
                    cfg.skills.doctrine.as_str().to_owned(),
                )
                .await?;
                Ok(())
            })
        }
    }
}

/// `makedo login chatgpt [model]`: вход по подписке `ChatGPT` в браузере; с моделью —
/// сразу делает её текущей в глобальном конфиге.
async fn login_cmd(args: &[String]) -> anyhow::Result<()> {
    use makedo_providers::chatgpt;
    if args.first().map(String::as_str) != Some("chatgpt") {
        bail!("usage: makedo login chatgpt [model]");
    }
    // действующий вход не трогаем: браузер нужен только если токен не обновить
    let models = if let Ok(models) = chatgpt::models().await {
        outln!(
            "already signed in as {}",
            chatgpt::account().unwrap_or_default()
        );
        models
    } else {
        let creds = chatgpt::login(|url| {
            eprintln!(
                "Opening the ChatGPT sign-in page. If the browser does not open, visit:\n{url}"
            );
            let opener = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            let _ = std::process::Command::new(opener)
                .arg(url)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        })
        .await?;
        outln!("signed in as {}", creds.email);
        chatgpt::models().await?
    };
    match args.get(1) {
        Some(model) if models.contains(model) => {
            makedo_config::persist_provider("chatgpt", model)?;
            outln!("provider set: chatgpt / {model}");
        }
        Some(model) => bail!(
            "model `{model}` is not available on this plan; available: {}",
            models.join(", ")
        ),
        None => {
            outln!("models: {}", models.join(", "));
            outln!("pick one with /models in makedo, or: makedo login chatgpt <model>");
        }
    }
    Ok(())
}

/// `--worktree <name>` → директория worktree; иначе cwd.
fn work_dir(args: &[String]) -> anyhow::Result<(PathBuf, Option<worktree::Lease>)> {
    let cwd = makedo_config::project_root(&std::env::current_dir()?);
    if let Some(i) = args.iter().position(|a| a == "--worktree") {
        let name = args.get(i + 1).context("usage: makedo --worktree <name>")?;
        ensure_git(&cwd)?;
        let dir = worktree::start(&cwd, name)?;
        outln!(
            "worktree: {} (finish with `makedo finish {name}`)",
            dir.display()
        );
        let lease = worktree::lease(&cwd, name)?;
        std::env::set_current_dir(&dir)?;
        return Ok((dir, Some(lease)));
    }
    std::env::set_current_dir(&cwd)?;
    Ok((cwd, None))
}

fn doctor() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    outln!("makedo {}", env!("CARGO_PKG_VERSION"));
    let res = makedo_types::resources::Resources::detect();
    let budget = res.budget();
    outln!(
        "machine: {} cores, RAM {}, disk free {}",
        res.cores,
        res.ram_bytes
            .map_or("?".into(), |b| format!("{}GB", b / (1024 * 1024 * 1024))),
        makedo_types::resources::disk_free_bytes()
            .map_or("?".into(), |b| format!("{}GB", b / (1024 * 1024 * 1024))),
    );
    outln!(
        "budget: {} parallel tools, {}MB capture cap",
        budget.tool_concurrency,
        budget.max_capture_bytes / 1_000_000
    );
    outln!("global dir: {:?}", makedo_config::global_dir());
    outln!(
        "heavy: {}",
        makedo_tools::builtin::shell::heavy_binary()
            .map_or_else(|| "not installed".into(), |p| p.display().to_string())
    );
    match makedo_config::load(&cwd) {
        Ok(cfg) => {
            outln!(
                "config: ok — provider {} / {}",
                cfg.provider.kind,
                cfg.provider.model
            );
            let catalog = makedo_providers::catalog::load(makedo_config::cache_dir().as_deref());
            if let Some((pid, m)) = makedo_providers::catalog::find_model(
                &catalog,
                Some(&cfg.provider.kind),
                &cfg.provider.model,
            ) {
                outln!(
                    "model: known ({pid}, ctx {}k, tools: {})",
                    m.context / 1000,
                    m.tools
                );
            } else {
                outln!("model: NOT in catalog — check the name (makedo models)");
            }
            match makedo_providers::from_config(&cfg.provider) {
                Ok(_) => outln!("api key: present"),
                Err(e) => outln!("api key: MISSING — {e}"),
            }
            outln!("mcp servers configured: {}", cfg.mcp.len());
        }
        Err(e) => outln!("config: ERROR — {e}"),
    }
    outln!(
        "git: {}",
        if cwd.join(".git").exists() {
            "ok"
        } else {
            "MISSING"
        }
    );
    let data = makedo_config::data_dir(&cwd);
    if makedo_wiki::seed::exists(&data) {
        match makedo_wiki::Graph::load(&data.join("wiki")) {
            Ok(g) => {
                let errors = g.check().iter().filter(|i| i.is_error()).count();
                outln!("wiki: {} nodes, {} dead links", g.nodes.len(), errors);
            }
            Err(e) => outln!("wiki: ERROR — {e}"),
        }
    } else {
        outln!("wiki: not initialized (runs on first `makedo`)");
    }
    Ok(())
}

fn wiki_cmd(sub: Option<&str>) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    match sub {
        Some("check") => {
            let graph = makedo_wiki::Graph::load(&makedo_config::data_dir(&cwd).join("wiki"))?;
            let issues = graph.check();
            let mut errors = 0;
            for issue in &issues {
                match issue {
                    makedo_wiki::Issue::DeadLink { from, to } => {
                        errors += 1;
                        eprintln!("error: dead link [[{to}]] in `{from}`");
                    }
                    makedo_wiki::Issue::Untyped { node } => {
                        eprintln!("warning: node `{node}` has no `type:`");
                    }
                }
            }
            outln!(
                "{} nodes, {} errors, {} warnings",
                graph.nodes.len(),
                errors,
                issues.len() - errors
            );
            if errors > 0 {
                std::process::exit(1);
            }
            Ok(())
        }
        Some("seed") => run_seed(&cwd, &makedo_config::data_dir(&cwd)),
        _ => bail!("usage: makedo wiki <check|seed>"),
    }
}

/// Wiki живёт в git обязательно: без репозитория предлагаем git init, отказ = выход.
fn ensure_git(root: &Path) -> anyhow::Result<()> {
    if root.join(".git").exists() {
        return Ok(());
    }
    eprint!("makedo requires git (the wiki lives in your history). Run `git init` here? [y/N] ");
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !answer.trim().eq_ignore_ascii_case("y") {
        bail!("cannot continue without git — the project wiki must be versioned");
    }
    let status = std::process::Command::new("git")
        .args(["init"])
        .current_dir(root)
        .status()?;
    if !status.success() {
        bail!("git init failed");
    }
    Ok(())
}

/// `makedo init` / `/init`: делает проект git-репо (если ещё нет) и ПЕРЕНОСИТ данные
/// makedo (вики, историю сессий) из `~/.makedo/projects/<...>` в `<project>/.makedo`,
/// чтобы всё лежало рядом с кодом и версионировалось.
fn init_project(root: &Path) -> anyhow::Result<()> {
    let external = makedo_config::data_dir(root); // где данные сейчас (git → уже в проекте)
    if makedo_config::data_is_in_project(root) {
        // уже git-режим — данные на месте, просто убеждаемся что seeded
        let data = root.join(".makedo");
        if !makedo_wiki::seed::exists(&data) {
            run_seed(root, &data)?;
        }
        eprintln!(
            "already a git project — makedo data is at {}",
            data.display()
        );
        return Ok(());
    }
    // не git → git init
    let status = std::process::Command::new("git")
        .arg("init")
        .current_dir(root)
        .status()?;
    if !status.success() {
        bail!("git init failed");
    }
    let target = root.join(".makedo");
    std::fs::create_dir_all(&target)?;
    // переносим внешние данные (wiki + sessions) в проект, если они есть
    if external.exists() && external != target {
        for sub in ["wiki", "sessions", "spill"] {
            let from = external.join(sub);
            if from.exists() {
                let to = target.join(sub);
                if to.exists() {
                    eprintln!("skip {sub}: already exists in project");
                } else {
                    std::fs::rename(&from, &to)
                        .with_context(|| format!("moving {}", from.display()))?;
                    eprintln!("moved {} → {}", from.display(), to.display());
                }
            }
        }
        let _ = std::fs::remove_dir(&external); // пусто — убираем
    }
    // seed если вики ещё нет; gitignore + add
    if makedo_wiki::seed::exists(&target) {
        let _ = makedo_wiki::seed::ensure_makedo_gitignore(&target);
        let _ = std::process::Command::new("git")
            .args(["add", ".makedo/wiki", ".makedo/.gitignore"])
            .current_dir(root)
            .status();
    } else {
        run_seed(root, &target)?;
    }
    eprintln!("initialized: makedo data now lives in {}", target.display());
    Ok(())
}

fn run_seed(root: &Path, data: &Path) -> anyhow::Result<()> {
    let report = makedo_wiki::seed::seed(root, data, |n| {
        eprint!("\r⠿ indexing wiki — {n} files scanned");
    })?;
    eprintln!(
        "\rwiki seeded: {} nodes from {} files at {}",
        report.nodes_created,
        report.files_scanned,
        report.vault.display()
    );
    // в git-режиме wiki попадает в историю проекта
    if makedo_config::data_is_in_project(root) {
        let _ = std::process::Command::new("git")
            .args(["add", ".makedo/wiki", ".makedo/.gitignore"])
            .current_dir(root)
            .status();
    }
    Ok(())
}

/// Бюджеты из реальных ресурсов машины (принцип: makedo знает, на чём работает).
/// Один детект на процесс: он запускает `sysctl` на macOS.
static BUDGET: std::sync::LazyLock<makedo_types::resources::Budget> =
    std::sync::LazyLock::new(|| makedo_types::resources::Resources::detect().budget());

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    let budget = *BUDGET;
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(budget.tool_concurrency.max(2))
        .max_blocking_threads(budget.max_blocking_threads)
        .enable_all()
        .build()?)
}

fn build_session(
    dir: &Path,
) -> anyhow::Result<(
    makedo_session::SessionHandle,
    makedo_config::Config,
    bool,
    bool,
)> {
    build_session_with(dir, None)
}

// линейная сборка: конфиг → провайдер → реестр (+агент-тулы) → сессия
#[allow(clippy::too_many_lines)]
fn build_session_with(
    dir: &Path,
    ruleset_override: Option<makedo_session::permission::Ruleset>,
) -> anyhow::Result<(
    makedo_session::SessionHandle,
    makedo_config::Config,
    bool,
    bool,
)> {
    let cfg = makedo_config::load(dir)?;
    // провайдер нет ключа → НЕ принуждаем к терминальному вводу дефолтного ключа;
    // стартуем с заглушкой и откроем выбор /models внутри TUI (первый запуск)
    let (provider, needs_setup): (Box<dyn makedo_providers::Provider>, bool) =
        match makedo_providers::from_config(&cfg.provider) {
            Ok(p) => (p, false),
            Err(
                makedo_providers::ProviderError::MissingKey(_)
                | makedo_providers::ProviderError::Auth(_),
            ) => (Box::new(makedo_providers::Unconfigured), true),
            Err(e) => return Err(e).context("provider setup failed"),
        };

    // корень данных: git-репо → <project>/.makedo, иначе ~/.makedo/projects/<name-hash>
    let data = makedo_config::data_dir(dir);
    // логи команд живут в <data>/spill: в git-режиме каталог обязан быть игнорируемым
    // на каждом запуске, а не только после первого seed
    let global_data = !makedo_config::data_is_in_project(dir);
    if !global_data {
        let _ = makedo_wiki::seed::ensure_makedo_gitignore(&data);
    }
    // первый запуск без wiki = инициализация. git НЕ требуем: нет git → данные в
    // ~/.makedo (пользователь позже сделает /init для переноса в проект).
    if !makedo_wiki::seed::exists(&data) {
        run_seed(dir, &data)?;
    }

    let memory = makedo_memory::Memory::open(dir, cfg.memory.clone())?;
    memory.import_wiki()?;
    memory.refresh_delivery()?;
    let mut legacy: Vec<_> = std::fs::read_dir(data.join("sessions"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    legacy.sort();
    for path in legacy {
        if let Err(error) = memory.import_journal(&path) {
            eprintln!("memory: could not import {}: {error}", path.display());
        }
    }
    let args: Vec<_> = std::env::args().collect();
    let mut restored_values = Vec::new();
    if let Some(index) = args.iter().position(|a| a == "--resume") {
        let source = args
            .get(index + 1)
            .context("usage: makedo --resume <session-id|session-file|latest>")?;
        let restored = if Path::new(source).is_file() {
            makedo_session::history::restore(Path::new(source))?
        } else {
            let id = if source == "latest" {
                memory.latest_session()?.context("no saved sessions")?
            } else {
                source.clone()
            };
            let values = memory.events(&id)?;
            if values.is_empty() {
                bail!("session not found or empty: {id}");
            }
            makedo_session::history::restore_values(values)?
        };
        for message in restored.messages {
            restored_values.push(serde_json::to_value(message)?);
        }
        for (id, text) in restored.pending {
            restored_values.push(serde_json::json!({"queued":{"id":id,"text":text}}));
        }
    }
    let memory_session = memory.start_session("main", &restored_values)?;
    let memory_brief = memory.brief()?;

    let mut registry = makedo_tools::registry::Registry::default();
    makedo_tools::builtin::register_all(&mut registry).context("builtin tool registration")?;
    makedo_wiki::tools::register_all(&mut registry, data.join("wiki"))
        .context("wiki tool registration")?;
    makedo_memory::tools::register(&mut registry, memory_session.clone())?;
    // image_gen — только при наличии ключа OpenRouter
    if let Some(tool) = image_tool::ImageGenTool::from_env() {
        registry.register(tool).context("image tool registration")?;
    }

    // MCP: конфиг [mcp.*] + импорт .mcp.json (проектный файл побеждает по имени)
    let mut mcp_servers = cfg.mcp.clone();
    mcp_servers.extend(makedo_mcp::mcp_json::import(dir, &|k| {
        std::env::var(k).ok()
    }));
    let mcp = makedo_mcp::McpManager::new(&mcp_servers);
    makedo_mcp::meta::register_all(&mut registry, Arc::clone(&mcp))
        .context("mcp meta tool registration")?;
    // Progressive-подключение: первый промпт не ждёт серверы (search_tool честно скажет partial)
    let mcp_bg = Arc::clone(&mcp);
    tokio::spawn(async move { mcp_bg.connect_all().await });
    // цены/лимиты модели из каталога + фоновое обновление каталога (TTL 1ч, lock)
    let cache_dir = makedo_config::cache_dir();
    let model_info = {
        let catalog = makedo_providers::catalog::load(cache_dir.as_deref());
        makedo_providers::catalog::find_model(
            &catalog,
            Some(&cfg.provider.kind),
            &cfg.provider.model,
        )
        .map(|(_, m)| m)
    };
    if let Some(dir) = cache_dir {
        makedo_providers::catalog::spawn_background_refresh(dir);
    }
    let spill_dir = data.join("spill");
    let budget = *BUDGET;
    let shim_dir = makedo_config::global_dir().and_then(|d| rgfd::ensure_shims(&d));
    let context_window = model_info.map(|m| m.context).filter(|c| *c > 0);
    // база промпта отдельно — сессия пересобирает промпт при смене доктрины
    let base_prompt = system_prompt(dir);
    let skills = makedo_skills::load(
        makedo_config::global_dir().as_deref(),
        dir,
        cfg.skills.ponytail,
        cfg.skills.doctrine,
        cfg.skills.triz,
    );
    let skills_prompt = makedo_skills::render(&skills);
    makedo_skills::tools::register(&mut registry, skills).context("skill tools")?;
    // субагенты (task) + worktree-тулы — только у корневой сессии (дети их
    // не получают → нет вложенных субагентов). Промпт ребёнка = снимок текущего.
    agent_tools::register(
        &mut registry,
        agent_tools::AgentCtx {
            root: dir.to_owned(),
            wiki: data.join("wiki"),
            spill_dir: spill_dir.clone(),
            provider_cfg: cfg.provider.clone(),
            memory_config: cfg.memory.clone(),
            system_prompt: format!("{base_prompt}{skills_prompt}"),
            model_info,
            context_window,
            capture_bytes: Some(budget.max_capture_bytes),
            shim_dir: shim_dir.clone(),
            concurrency: budget.tool_concurrency,
            verify_cmds: cfg.verify.cmds.clone(),
        },
    )
    .context("agent tool registration")?;
    // --yolo / --allow-all: авто-одобрение всех тул-коллов (без permission-запросов)
    let yolo = std::env::args().any(|a| a == "--yolo" || a == "--allow-all");
    let mut ruleset = if yolo {
        makedo_session::permission::Ruleset::allow_all()
    } else {
        ruleset_override.unwrap_or_else(makedo_session::permission::Ruleset::default_policy)
    };
    ruleset.push(makedo_session::permission::Rule::new(
        "memory_write",
        "**",
        makedo_session::permission::Action::Allow,
    )?);
    let base_prompt = format!("{base_prompt}{memory_brief}");
    let handle = makedo_session::spawn(
        provider,
        Arc::new(registry),
        ruleset,
        makedo_session::SessionConfig {
            provider: cfg.provider.clone(),
            system_prompt: format!("{base_prompt}{skills_prompt}"),
            base_system_prompt: base_prompt,
            ponytail: cfg.skills.ponytail,
            doctrine: cfg.skills.doctrine,
            triz: cfg.skills.triz,
            global_dir: makedo_config::global_dir(),
            cwd: dir.to_owned(),
            spill_dir,
            concurrency: budget.tool_concurrency,
            shim_dir,
            context_window,
            model_info,
            capture_bytes: Some(budget.max_capture_bytes),
            session_file: None,
            memory: Some(memory_session),
            // персистентные bash-одобрения по маске: .makedo/permissions.json
            permissions_file: Some(data.join("permissions.json")),
            verify_cmds: cfg.verify.cmds.clone(),
        },
    );
    Ok((handle, cfg, needs_setup, global_data))
}

fn system_prompt(cwd: &Path) -> String {
    let mut card = makedo_wiki::seed::project_card(cwd);
    if !card.is_empty() {
        card.push_str("\n\n");
    }
    format!(
        "You are makedo, a terminal coding agent. Working directory: {}.\n\
         Use the available tools to read, search, and modify files and run commands. \
         Be concise. Prefer the simplest change that works.\n\n{card}\
         FILE EDITING: prefer edit for exact replacements and apply_patch for multi-hunk/file changes. \
         Read existing files first and pass the returned version as expected_version (edit/write) \
         or expected_versions[path] (apply_patch). Successful writes return the new version. \
         On conflict, read again and reconsider the change. write without a version only creates missing files. \
         Use these native tools for ordinary edits; shell scripts are for bulk or structural transformations.\n\n\
         COMMAND LOGS: run builds, tests and other commands with bash in its default status mode — the full \
         output goes to a git-ignored log file and you get only OK/FAIL with the log path. On OK do not read \
         the log. On FAIL grep the log for the error and read only the relevant lines. Logs stay on disk as \
         history; never pull a whole log into the conversation. Use output=content only when the output \
         itself is the answer (rg, git diff).\n\n\
         The project has a curated knowledge graph at .makedo/wiki (the wiki). \
         SEARCH DISCIPLINE: the harness has already searched project memory for the task — hits, if any, \
         are appended to the user message under [makedo: …]; start from the files they name instead of \
         grepping blind. No hits there: memory most likely holds nothing on it, go straight to the code. \
         Call memory_search only to look further (session history, another wording). A wiki hit's title \
         is its node name: use wiki_node and wiki_trace to follow typed edges. ENRICHMENT DUTY: whenever you learn something durable about \
         the codebase (what a module does, how components connect, a key decision), \
         record it with memory_write and sources; curated wiki nodes can also be edited under .makedo/wiki, \
         using `key:: [[Target]]` typed edges per .makedo/wiki/schema.md. The wiki is \
         version-controlled; keep it truthful. WIKI STYLE (ponytail/caveman): nodes are \
         for agents, not humans — telegraphic, dense, no prose. Facts and edges only. \
         'Parses V3 XML. Retries 3x.' — not 'This module is responsible for parsing…'. \
         Every saved token is money on every future read.\n\n\
         CREDENTIALS: assume the user is not a security expert — protect them. \
         Never print secret values (API keys, tokens, passwords, private keys) in \
         replies, logs, commits, or the wiki; refer to them by name and mask values \
         (sk-…redacted). Secrets live in env vars or a credential store — never \
         hardcode them in source or in files tracked by git. Before committing, check \
         the diff for secrets; keep .env and key files in .gitignore. If you find a \
         secret already in code or git history, warn the user, move it to an env var, \
         and tell them to rotate it — history rewrite alone is not enough. Never send \
         secrets to external services, put them in URLs, or paste them into commands \
         whose output gets logged.",
        cwd.display()
    )
}
