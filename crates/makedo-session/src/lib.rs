//! Session-актор: один task владеет историей и состоянием — никаких shared-локов
//! на горячем пути (PLAN §3.5). Вход — bounded-канал команд, выход — bounded-канал
//! событий для TUI. Loop: stream LLM → permissions → батч тулов → результаты → повтор.

pub mod permission;

mod context;
pub mod history;

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use makedo_providers::{AssistantPart, CompletionRequest, LlmEvent, Message, Provider, StopReason};
use makedo_tools::ToolCtx;
use makedo_tools::executor::{self, ToolCall};
use makedo_tools::registry::Registry;
use makedo_types::event::{SessionEvent, TodoEntry, TodoState, TurnUsage};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use permission::{Action, DoomLoopGuard, Ruleset, subject_of};

/// Ёмкости каналов: команды редки, события — плотный поток дельт.
const CMD_CAPACITY: usize = 16;
const EVENT_CAPACITY: usize = 256;
const MAX_OUTPUT_TOKENS: u32 = 8_192;
const MAX_OUTPUT_RECOVERIES: u32 = 2;
/// Ретраи обрыва стрима до первого события контента (дропы коннекта провайдера).
const MAX_STREAM_RETRIES: u32 = 2;
/// После overflow повторяем только изменённый запрос, не более двух раз.
const MAX_CONTEXT_RETRIES: u32 = 2;

#[derive(Debug)]
pub enum SessionCommand {
    Prompt(String),
    /// Interrupt the active step, then deliver the inbox including this text.
    /// Empty text flushes already queued prompts without adding an empty message.
    Steer(String),
    /// Internal durable inbox item; callers normally send Prompt.
    QueuedPrompt {
        id: String,
        text: String,
    },
    Cancel,
    PermissionReply {
        request_id: u64,
        approve: bool,
        /// "always allow" — добавить сессионное правило под этот тул+субъект.
        always: bool,
    },
    /// Прямой вызов тула слэш-командой TUI — без LLM, ноль токенов.
    RunTool {
        name: String,
        args: serde_json::Value,
    },
    /// Переключить провайдера/модель на лету (ключ уже сохранён в credentials).
    SwitchModel {
        kind: String,
        model: String,
    },
    /// Выбрать уровень reasoning для следующих запросов.
    SetEffort(String),
    /// Включить режим «разрешить всё» (без permission-запросов) на сессию.
    SetAllowAll,
    /// Сменить доктрину проектирования на лету — пересобирает системный промпт.
    SetDoctrine(makedo_config::Doctrine),
    SetPonytail(makedo_config::PonytailLevel),
    /// Прогрев wiki (`/enrich [фокус]`): разрешает записи в `.makedo/wiki` на
    /// сессию и запускает турн-куратор с `makedo_wiki::enrich_prompt`.
    Enrich(Option<String>),
    /// Включить/обновить verification gate на лету (онбординг пишет конфиг
    /// проекта и активирует без рестарта).
    SetVerifyCmds(Vec<String>),
}

#[derive(Debug, Default)]
pub struct SessionConfig {
    pub provider: makedo_config::ProviderConfig,
    pub system_prompt: String,
    pub cwd: PathBuf,
    pub spill_dir: PathBuf,
    pub concurrency: usize,
    /// Контекстное окно модели (токены) из каталога; None = дефолтный триггер компакции.
    pub context_window: Option<u64>,
    /// Директория шимов rg/fd для bash-тула.
    pub shim_dir: Option<PathBuf>,
    /// Цены модели из каталога — для реальной стоимости турна в TUI.
    pub model_info: Option<makedo_providers::catalog::ModelInfo>,
    /// Потолок захвата вывода (из `Budget`).
    pub capture_bytes: Option<usize>,
    /// Файл append-only истории сессии (`.makedo/sessions/<ts>.jsonl`); None = не пишем.
    pub session_file: Option<PathBuf>,
    /// Shared project database and the identity of this agent's durable transcript.
    pub memory: Option<makedo_memory::Session>,
    /// Файл персистентных «always allow» масок (`.makedo/permissions.json`); None = не храним.
    pub permissions_file: Option<PathBuf>,
    /// Системный промпт БЕЗ секции скиллов — база для пересборки при смене доктрины.
    pub base_system_prompt: String,
    /// Уровень ponytail (для пересборки промпта при смене доктрины).
    pub ponytail: makedo_config::PonytailLevel,
    pub doctrine: makedo_config::Doctrine,
    /// Глобальная директория makedo (для загрузки скиллов при пересборке).
    pub global_dir: Option<PathBuf>,
    /// Verification gate: shell-команды самопроверки (`sh -c`), гоняются после
    /// турна с правками; провал возвращается модели (до 2 раундов). Пусто = выкл.
    pub verify_cmds: Vec<String>,
}

/// Хэндл для TUI: команды внутрь, события наружу.
#[derive(Debug)]
pub struct SessionHandle {
    pub commands: mpsc::Sender<SessionCommand>,
    pub events: mpsc::Receiver<SessionEvent>,
}

pub fn spawn(
    provider: Box<dyn Provider>,
    registry: Arc<Registry>,
    ruleset: Ruleset,
    cfg: SessionConfig,
) -> SessionHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
    let approved = permission::ApprovedStore::load(cfg.permissions_file.clone());
    tokio::spawn(async move {
        let restored = match cfg.memory.as_ref().map_or_else(
            || {
                cfg.session_file
                    .as_ref()
                    .map(|p| history::restore(p))
                    .transpose()
                    .map(Option::unwrap_or_default)
            },
            |session| {
                session
                    .memory
                    .events(&session.id)
                    .map_err(std::io::Error::other)
                    .and_then(history::restore_values)
            },
        ) {
            Ok(restored) => restored,
            Err(error) => {
                let _ = event_tx
                    .send(SessionEvent::Error(format!(
                        "Cannot restore session: {error}"
                    )))
                    .await;
                return;
            }
        };
        let mut actor = Actor {
            provider,
            registry,
            ruleset,
            approved,
            cfg,
            history: restored.messages,
            context: context::Budget::default(),
            effort: makedo_providers::effort::Settings::default(),
            events: event_tx,
            doom: DoomLoopGuard::default(),
            next_request_id: 0,
            turn_usage: TurnUsage::default(),
            turn_had_writes: false,
            steering: false,
            pending: restored
                .pending
                .into_iter()
                .map(|(id, text)| SessionCommand::QueuedPrompt { id, text })
                .collect(),
        };
        for call_id in restored.interrupted {
            actor.push_tool_result(&call_id, "Execution interrupted before its result was recorded. Inspect files and command logs before retrying; this call was NOT replayed.");
        }
        if !actor.history.is_empty() {
            let _ = actor
                .events
                .send(SessionEvent::Notice(format!(
                    "Restored {} messages; completed tools are not replayed.",
                    actor.history.len()
                )))
                .await;
            for message in actor
                .history
                .iter()
                .rev()
                .take(20)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
            {
                let (user, text) = match message {
                    Message::User { content } => (true, content.clone()),
                    Message::Assistant { content } => (
                        false,
                        content
                            .iter()
                            .filter_map(|p| match p {
                                AssistantPart::Text { text } => Some(text.as_str()),
                                AssistantPart::ToolCall { .. }
                                | AssistantPart::OpenAiOutput { .. } => None,
                            })
                            .collect::<String>(),
                    ),
                    _ => continue,
                };
                if !text.is_empty() {
                    let _ = actor
                        .events
                        .send(SessionEvent::HistoryMessage { user, text })
                        .await;
                }
            }
        }
        actor.run(cmd_rx).await;
    });
    SessionHandle {
        commands: cmd_tx,
        events: event_rx,
    }
}

struct Actor {
    provider: Box<dyn Provider>,
    registry: Arc<Registry>,
    ruleset: Ruleset,
    /// Персистентные bash-одобрения по маске (переживают сессии).
    approved: permission::ApprovedStore,
    cfg: SessionConfig,
    history: Vec<Message>,
    context: context::Budget,
    effort: makedo_providers::effort::Settings,
    events: mpsc::Sender<SessionEvent>,
    doom: DoomLoopGuard,
    next_request_id: u64,
    turn_usage: TurnUsage,
    /// Были ли в текущем турне не-read-only тул-коллы (триггер verification gate).
    turn_had_writes: bool,
    /// Step cancellation requested for steering, not for discarding the inbox.
    steering: bool,
    pending: VecDeque<SessionCommand>,
}

#[cfg(test)]
use makedo_tools::executor::summarize;

/// Решение permission-фазы по одному коллу.
enum Verdict {
    Approved,
    Denied,
}

impl Actor {
    async fn run(mut self, mut cmd_rx: mpsc::Receiver<SessionCommand>) {
        self.effort = makedo_providers::effort::Settings::load(
            &self.cfg.provider,
            self.cfg.global_dir.as_deref(),
        );
        self.publish_model().await;
        let _ = self
            .events
            .send(SessionEvent::PermissionsChanged {
                allow_all: self.ruleset.is_allow_all(),
            })
            .await;
        while let Some(cmd) = match self.pending.pop_front() {
            Some(cmd) => Some(cmd),
            None => cmd_rx.recv().await,
        } {
            match cmd {
                SessionCommand::Prompt(text) | SessionCommand::Steer(text) => {
                    if text.is_empty() {
                        continue;
                    }
                    if self.configure_prompt(&text).await {
                        let _ = self
                            .events
                            .send(SessionEvent::TurnFinished {
                                cancelled: false,
                                usage: TurnUsage::default(),
                            })
                            .await;
                    } else {
                        self.prompt_turn(text, None, &mut cmd_rx).await;
                    }
                }
                SessionCommand::QueuedPrompt { id, text } => {
                    self.prompt_turn(text, Some(id), &mut cmd_rx).await;
                }
                SessionCommand::Enrich(focus) => {
                    // куратору wiki можно писать в wiki без вопросов (safe scope)
                    for tool in ["write", "edit", "apply_patch"] {
                        if let Ok(rule) = permission::Rule::new(
                            tool,
                            "**/.makedo/wiki/**",
                            permission::Action::Allow,
                        ) {
                            self.ruleset.push(rule);
                        }
                    }
                    let _ = self
                        .events
                        .send(SessionEvent::Notice(
                            "wiki enrich: writes to .makedo/wiki auto-allowed".to_owned(),
                        ))
                        .await;
                    self.prompt_turn(
                        makedo_wiki::enrich_prompt(focus.as_deref()),
                        None,
                        &mut cmd_rx,
                    )
                    .await;
                }
                SessionCommand::SetVerifyCmds(cmds) => {
                    let _ = self
                        .events
                        .send(SessionEvent::Notice(if cmds.is_empty() {
                            "verify gate: off".to_owned()
                        } else {
                            format!("verify gate: {}", cmds.join(" && "))
                        }))
                        .await;
                    self.cfg.verify_cmds = cmds;
                }
                SessionCommand::RunTool { name, args } => {
                    let cancelled = self.run_tool_direct(name, args, &mut cmd_rx).await;
                    if cancelled {
                        self.discard_pending_prompts().await;
                    }
                    let _ = self
                        .events
                        .send(SessionEvent::TurnFinished {
                            cancelled,
                            usage: TurnUsage::default(),
                        })
                        .await;
                }
                SessionCommand::SwitchModel { kind, model } => {
                    self.switch_model(&kind, &model).await;
                }
                SessionCommand::SetEffort(value) => self.set_effort(&value).await,
                SessionCommand::SetAllowAll => self.enable_yolo().await,
                SessionCommand::SetPonytail(level) => {
                    self.cfg.ponytail = level;
                    self.rebuild_prompt(self.cfg.doctrine);
                    let _ = self
                        .events
                        .send(SessionEvent::Notice(format!(
                            "Ponytail → {level:?} (this session)"
                        )))
                        .await;
                }
                SessionCommand::SetDoctrine(doctrine) => {
                    self.rebuild_prompt(doctrine);
                    let _ = self
                        .events
                        .send(SessionEvent::Notice(format!(
                            "doctrine → {} (active now)",
                            doctrine.as_str()
                        )))
                        .await;
                }
                // Cancel/PermissionReply вне активного турна — игнорируются
                SessionCommand::Cancel | SessionCommand::PermissionReply { .. } => {}
            }
        }
    }

    fn request(&self, output_budget: u32) -> CompletionRequest {
        let output_limit = self
            .cfg
            .model_info
            .map(|m| m.output)
            .filter(|n| *n > 0)
            .unwrap_or(u64::from(output_budget));
        let window_limit = self
            .cfg
            .context_window
            .map_or(u64::from(output_budget), |ctx| (ctx / 4).max(1));
        let max_tokens = u32::try_from(output_limit.min(window_limit))
            .unwrap_or(output_budget)
            .min(output_budget);
        CompletionRequest {
            model: self.cfg.provider.model.clone(),
            system: if self.ruleset.is_allow_all() {
                format!(
                    "{}\n\nYOLO mode: all tool permissions are granted for this session. Execute the requested work without asking for tool approval.",
                    self.cfg.system_prompt
                )
            } else {
                self.cfg.system_prompt.clone()
            },
            messages: self.history.clone(),
            tools: self
                .registry
                .definitions()
                .into_iter()
                .map(|d| makedo_providers::ToolDef {
                    name: d.name,
                    description: d.description,
                    input_schema: d.input_schema,
                })
                .collect(),
            max_tokens,
            reasoning_effort: self.effort.selected.clone(),
        }
    }

    /// Один пользовательский турн; `true` = отменён.
    // шаг цикла линеен (stream → permissions → батч); дальнейшая нарезка скрыла бы поток
    #[allow(clippy::too_many_lines)]
    async fn turn(&mut self, cmd_rx: &mut mpsc::Receiver<SessionCommand>) -> bool {
        self.steering = false;
        let mut output_budget = MAX_OUTPUT_TOKENS;
        let mut output_recoveries = 0;
        loop {
            self.apply_pending().await;
            let _ = self
                .events
                .send(SessionEvent::PhaseChanged("Model".into()))
                .await;
            let cancel = CancellationToken::new();
            let mut req = self.request(output_budget);
            match self.context.fit(&mut req, self.cfg.context_window) {
                Ok(true) => {
                    self.history.clone_from(&req.messages);
                    let _ = self.events.send(SessionEvent::Notice(
                        "Context shortened to leave room for the response; the session log is unchanged.".to_owned()
                    )).await;
                }
                Ok(false) => {}
                Err(message) => {
                    let _ = self
                        .events
                        .send(SessionEvent::Error(message.to_owned()))
                        .await;
                    return false;
                }
            }
            // обрывы стрима ДО первого события контента ретраим с бэкоффом —
            // это дропы коннекта/декодинга (DeepSeek и др.), не ошибки запроса.
            // Обрыв с частичным выводом не ретраим: текст уже ушёл в UI.
            let mut attempts = 0u32;
            let mut context_attempts = 0u32;
            let (text, calls, finish, output) = 'attempt: loop {
                let mut stream = self.provider.stream(req.clone());
                let mut text = String::new();
                let mut calls: Vec<(String, String, String)> = Vec::new(); // (id, name, args_json)
                let mut finish: Option<StopReason> = None;
                let mut output = Vec::new();

                loop {
                    tokio::select! {
                        biased;
                        cmd = cmd_rx.recv() => match cmd {
                            Some(SessionCommand::Cancel) | None => {
                                drop(stream); // обрывает HTTP
                                self.push_assistant(&text, &calls, output);
                                self.push_interrupted_results(&calls);
                                return true;
                            }
                            Some(SessionCommand::SetEffort(value)) => self.set_effort(&value).await,
                            Some(cmd) => {
                                self.queue_command(cmd).await;
                                if self.steering {
                                    drop(stream);
                                    self.push_assistant(&text, &calls, output);
                                    self.push_interrupted_results(&calls);
                                    return true;
                                }
                            }
                        },
                        ev = stream.next() => match ev {
                            Some(Ok(LlmEvent::OpenAiOutput(items))) => output = items,
                            Some(Ok(LlmEvent::TextDelta(t))) => {
                                text.push_str(&t);
                                let _ = self.events.send(SessionEvent::TextDelta(t)).await;
                            }
                            Some(Ok(LlmEvent::ToolCallStart { call_id, name })) => {
                                calls.push((call_id, name, String::new()));
                            }
                            Some(Ok(LlmEvent::ToolCallDelta { call_id, args_delta })) => {
                                if let Some(c) = calls.iter_mut().find(|c| c.0 == call_id) {
                                    c.2.push_str(&args_delta);
                                }
                            }
                            Some(Ok(LlmEvent::Finished { reason, usage })) => {
                                if self.cfg.context_window.is_some() {
                                    self.context.observe(&req, usage.input_tokens);
                                }
                                self.turn_usage.input_tokens += usage.input_tokens;
                                self.turn_usage.output_tokens += usage.output_tokens;
                                self.turn_usage.cache_read_tokens += usage.cache_read_tokens;
                                self.turn_usage.cache_write_tokens += usage.cache_write_tokens;
                                // цена от провайдера (CLI-делегаты) — каталог её не перепишет
                                if let Some(c) = usage.cost_usd {
                                    *self.turn_usage.cost_usd.get_or_insert(0.0) += c;
                                }
                                finish = Some(reason);
                                break 'attempt (text, calls, finish, output);
                            }
                            Some(Err(e)) => {
                                if text.is_empty() && calls.is_empty() && e.is_context_overflow()
                                    && context_attempts < MAX_CONTEXT_RETRIES && context::Budget::recover(&mut req)
                                {
                                    drop(stream);
                                    context_attempts += 1;
                                    self.history.clone_from(&req.messages);
                                    let _ = self.events.send(SessionEvent::Notice(format!(
                                        "Context limit reached; shortened previous output, retry {context_attempts}/{MAX_CONTEXT_RETRIES}."
                                    ))).await;
                                    continue 'attempt;
                                }
                                if text.is_empty() && calls.is_empty() && e.is_stream_retryable() && attempts < MAX_STREAM_RETRIES {
                                    attempts += 1;
                                    let _ = self.events
                                        .send(SessionEvent::Notice(format!(
                                            "stream dropped, retry {attempts}/{MAX_STREAM_RETRIES}: {e}"
                                        )))
                                        .await;
                                    tokio::time::sleep(std::time::Duration::from_millis(
                                        500 << attempts,
                                    ))
                                    .await;
                                    continue 'attempt;
                                }
                                let _ = self.events.send(SessionEvent::Error(e.to_string())).await;
                                self.push_assistant(&text, &calls, output);
                                self.push_interrupted_results(&calls);
                                return false;
                            }
                            None => break 'attempt (text, calls, finish, output),
                        },
                    }
                }
            };

            self.push_assistant(&text, &calls, output);
            if finish == Some(StopReason::MaxTokens) {
                // Even valid JSON can describe a call from an unfinished batch.
                // None of these calls may run; preserve their IDs and raw replay.
                for (id, _, _) in &calls {
                    self.push_tool_result(id, "Tool was not executed: the response reached its output token limit. Reissue the call with complete arguments, splitting large writes into smaller calls.");
                }
                if output_recoveries >= MAX_OUTPUT_RECOVERIES {
                    let _ = self.events.send(SessionEvent::Error(format!(
                        "The model reached its output token limit after {MAX_OUTPUT_RECOVERIES} recovery attempts. Progress is saved; continue with smaller steps."
                    ))).await;
                    return false;
                }
                output_recoveries += 1;
                output_budget = req.max_tokens.saturating_mul(2);
                let message = Message::User {
                    content: "The previous response reached its output token limit. Continue the original task from the saved state without repeating completed work. No tool calls from the truncated response were executed. Reissue needed calls with complete arguments and split large writes into smaller calls.".into(),
                };
                self.record(&message);
                self.history.push(message);
                let _ = self.events.send(SessionEvent::Notice(format!(
                    "Output token limit reached; continuing from saved progress ({output_recoveries}/{MAX_OUTPUT_RECOVERIES})."
                ))).await;
                continue;
            }
            // толерантность к finish=stop при эмитированных коллах (PLAN §3.5)
            if calls.is_empty() {
                if self
                    .pending
                    .iter()
                    .any(|command| matches!(command, SessionCommand::QueuedPrompt { .. }))
                {
                    continue;
                }
                return false;
            }

            let mut tool_calls = Vec::with_capacity(calls.len());
            for (id, name, args_buf) in &calls {
                let args = serde_json::from_str(args_buf)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                tool_calls.push(ToolCall {
                    call_id: id.clone(),
                    name: name.clone(),
                    args,
                });
            }

            // permission-фаза — последовательная, до батча (модель grok-build):
            // отказ синтезирует результаты, пары tool_use/tool_result не рвутся
            let all_ids: Vec<String> = tool_calls.iter().map(|c| c.call_id.clone()).collect();
            let mut approved = Vec::new();
            let mut denied: Vec<ToolCall> = Vec::new();
            let mut rejected_rest = false;
            for call in tool_calls {
                if rejected_rest {
                    denied.push(call);
                    continue;
                }
                match self.check_permission(&call, cmd_rx).await {
                    Some(Verdict::Approved) => approved.push(call),
                    Some(Verdict::Denied) => {
                        rejected_rest = true;
                        denied.push(call);
                    }
                    None => {
                        // отмена посреди permission-запроса: КАЖДЫЙ tool_use из этого
                        // ассистент-сообщения обязан получить tool_result (approved +
                        // ещё не дошедшие + denied), иначе Anthropic отвергнет историю
                        for id in &all_ids {
                            self.push_tool_result(id, "Tool execution was cancelled");
                        }
                        return true;
                    }
                }
            }

            self.execute_approved(approved, &cancel, cmd_rx).await;
            for call in denied {
                self.push_tool_result(
                    &call.call_id,
                    "Permission denied by user. Do not retry this call; ask the user or adjust the approach.",
                );
            }
            if cancel.is_cancelled() {
                return true;
            }
            // цикл продолжается: результаты ушли в историю, следующий шаг LLM
        }
    }

    /// Полный цикл одного пользовательского промпта: турн → verify gate →
    /// цена → `TurnFinished` (общий путь `Prompt` и `Enrich`).
    async fn prompt_turn(
        &mut self,
        text: String,
        queued_id: Option<String>,
        cmd_rx: &mut mpsc::Receiver<SessionCommand>,
    ) {
        self.accept_prompt(text.clone(), queued_id).await;
        self.turn_usage = TurnUsage::default();
        self.turn_had_writes = false;
        let cancelled = loop {
            let mut cancelled = self.turn(cmd_rx).await;
            if self.steering {
                continue;
            }
            if !cancelled {
                cancelled = self.verify_gate(cmd_rx).await;
            }
            if !self.steering {
                break cancelled;
            }
        };
        if cancelled {
            self.discard_pending_prompts().await;
        } else {
            self.remember_turn(&text);
        }
        self.price_turn();
        let _ = self
            .events
            .send(SessionEvent::TurnFinished {
                cancelled,
                usage: self.turn_usage,
            })
            .await;
    }

    /// Память силами харнесса: турн, которому пришлось искать, оставляет заметку
    /// «о чём спросили → в каких файлах ответ». Следующий похожий промпт получает её
    /// подсказкой (`hints`). Запроса к модели нет: файлы берутся из тул-коллов турна.
    fn remember_turn(&self, prompt: &str) {
        // ponytail: fixed effort threshold, a 600-char answer head, findings made by grep
        // alone are not captured; distil with a model call only if these notes measurably
        // fail to shorten later turns
        const MIN_TOOL_CALLS: usize = 3;
        let Some(memory) = &self.cfg.memory else {
            return;
        };
        let Some(start) = self
            .history
            .iter()
            .rposition(|m| matches!(m, Message::User { content } if content.starts_with(prompt)))
        else {
            return;
        };
        let (mut calls, mut answer) = (0usize, String::new());
        let (mut read, mut files) = (Vec::new(), Vec::new());
        for message in &self.history[start..] {
            let Message::Assistant { content } = message else {
                continue;
            };
            answer.clear();
            for part in content {
                match part {
                    AssistantPart::Text { text } => answer.push_str(text),
                    AssistantPart::ToolCall { name, input, .. } => {
                        if name == "memory_write" {
                            return; // модель записала вывод сама
                        }
                        calls += 1;
                        let Some(tool) = self.registry.get(name) else {
                            continue;
                        };
                        let paths = tool.access_paths_json(input).into_iter().map(|p| {
                            p.strip_prefix(&self.cfg.cwd)
                                .unwrap_or(&p)
                                .display()
                                .to_string()
                        });
                        if tool.is_read_only() {
                            read.extend(paths);
                        } else {
                            files.extend(paths);
                        }
                    }
                    AssistantPart::OpenAiOutput { .. } => {}
                }
            }
        }
        // прочитанный файл — находка, только если ответ его называет
        files.extend(read.into_iter().filter(|p| {
            std::path::Path::new(p)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| answer.contains(n))
        }));
        files.retain(|p| !p.starts_with(".makedo/")); // wiki индексируется сама
        files.sort();
        files.dedup();
        files.truncate(8);
        let title: String = prompt
            .trim()
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(120)
            .collect();
        if calls < MIN_TOOL_CALLS
            || files.is_empty()
            || title.is_empty()
            || answer.trim().is_empty()
        {
            return;
        }
        // тот же вопрос повторно (модель не воспользовалась подсказкой) заметки не плодит
        if memory
            .memory
            .hints(prompt)
            .is_ok_and(|hints| hints.contains(&format!(" {title} — ")))
        {
            return;
        }
        let head: String = answer.trim().chars().take(600).collect();
        let event = match memory.write_note(&makedo_memory::tools::WriteArgs {
            title,
            text: format!("Files: {}\n{head}", files.join(", ")),
            sources: files,
            id: None,
            expected_revision: None,
            assessment: None,
            supersedes: None,
        }) {
            Ok(note) => SessionEvent::Notice(format!(
                "memory: noted where this was found (note #{})",
                note["id"]
            )),
            Err(error) => SessionEvent::Error(format!("Memory note failed: {error}")),
        };
        let _ = self.events.try_send(event);
    }

    /// Verification gate (CORE §достроить-2): после турна с правками гоняет
    /// команды самопроверки; провал возвращается модели новым user-сообщением
    /// (до 2 раундов). Возвращает `true`, если пользователь отменил.
    #[allow(clippy::too_many_lines)] // check, report and bounded repair form one linear gate
    async fn verify_gate(&mut self, cmd_rx: &mut mpsc::Receiver<SessionCommand>) -> bool {
        const MAX_ROUNDS: u32 = 2;
        const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(10);
        // гейт входит один раз по факту правок; дальше перепроверяет каждый
        // раунд независимо от новых правок (модель может «починить» и словами)
        if !self.turn_had_writes || self.cfg.verify_cmds.is_empty() {
            return false;
        }
        for round in 0..=MAX_ROUNDS {
            let _ = self
                .events
                .send(SessionEvent::PhaseChanged(format!(
                    "Verification · round {}",
                    round + 1
                )))
                .await;
            let mut failures = String::new();
            for cmd in self.cfg.verify_cmds.clone() {
                let _ = self
                    .events
                    .send(SessionEvent::Notice(format!("verify: {cmd}")))
                    .await;
                let _ = self
                    .events
                    .send(SessionEvent::PhaseChanged(format!("Verification · {cmd}")))
                    .await;
                let cancel = CancellationToken::new();
                let ctx = ToolCtx {
                    progress: Some(makedo_tools::Progress {
                        events: self.events.clone(),
                        call_id: String::new(),
                    }),
                    cancel: cancel.clone(),
                    cwd: self.cfg.cwd.clone(),
                    spill_dir: self.cfg.spill_dir.clone(),
                    shim_dir: self.cfg.shim_dir.clone(),
                    capture_bytes: self.cfg.capture_bytes,
                };
                let run = makedo_tools::builtin::shell::run_command(&ctx, &cmd, CMD_TIMEOUT, true);
                tokio::pin!(run);
                let result = loop {
                    tokio::select! {
                        result = &mut run => break result,
                        command = cmd_rx.recv(), if !cancel.is_cancelled() => match command {
                            Some(SessionCommand::Cancel) | None => cancel.cancel(),
                            Some(SessionCommand::SetEffort(value)) => self.set_effort(&value).await,
                            Some(command) => {
                                self.queue_command(command).await;
                                if self.steering { cancel.cancel(); }
                            }
                        }
                    }
                };
                if let Ok(result) = &result {
                    if !cancel.is_cancelled()
                        && let Some(memory) = &self.cfg.memory
                        && let Err(error) = memory.record_check(
                            &cmd,
                            result.success(),
                            &result.log.display().to_string(),
                        )
                    {
                        let _ = self
                            .events
                            .send(SessionEvent::Error(error.to_string()))
                            .await;
                    }
                    let id = format!("verify-{}", self.next_request_id);
                    self.next_request_id += 1;
                    let _ = self
                        .events
                        .send(SessionEvent::ToolLog {
                            call_id: id,
                            name: format!("verify: {cmd}"),
                            path: result.log.display().to_string(),
                        })
                        .await;
                }
                if cancel.is_cancelled() {
                    return true;
                }
                match result {
                    Ok(result) if result.success() => {
                        let _ = self
                            .events
                            .send(SessionEvent::Notice(format!(
                                "verify: {}",
                                result.summary()
                            )))
                            .await;
                    }
                    Ok(result) => {
                        let _ = writeln!(failures, "$ {cmd}\n{}", result.summary());
                    }
                    Err(error) => {
                        let _ = writeln!(failures, "$ {cmd}\n{error}");
                    }
                }
            }
            if failures.is_empty() {
                let _ = self
                    .events
                    .send(SessionEvent::Notice("verify gate: green".to_owned()))
                    .await;
                return false;
            }
            if round == MAX_ROUNDS {
                let _ = self
                    .events
                    .send(SessionEvent::Error(format!(
                        "verify gate: still red after {MAX_ROUNDS} fix rounds:\n{failures}"
                    )))
                    .await;
                return false;
            }
            let _ = self
                .events
                .send(SessionEvent::Notice(format!(
                    "verify gate: red, round {}/{MAX_ROUNDS}",
                    round + 1
                )))
                .await;
            let m = Message::User {
                content: format!(
                    "[verification gate] These project checks fail after your \
                     changes. Fix the code (or the tests you wrote) until they \
                     pass. Search the command logs with grep and read only relevant lines. Results:\n{failures}"
                ),
            };
            self.record(&m);
            self.history.push(m);
            self.turn_had_writes = false;
            if self.turn(cmd_rx).await {
                return true; // отменён пользователем
            }
        }
        false
    }

    /// Реальная стоимость турна по ценам каталога (экономика кэша — PLAN M3.5).
    fn price_turn(&mut self) {
        const M: f64 = 1_000_000.0;
        // провайдер сообщил реальную цену — она точнее каталога; подписка ChatGPT
        // оплачена планом, долларовая цена по каталогу была бы выдумкой
        if self.turn_usage.cost_usd.is_some() || self.cfg.provider.kind == "chatgpt" {
            return;
        }
        let Some(m) = &self.cfg.model_info else {
            return;
        };
        let u = &mut self.turn_usage;
        #[allow(clippy::cast_precision_loss)] // токены << 2^52
        let (noncached, cached, written, out) = (
            u.input_tokens.saturating_sub(u.cache_read_tokens) as f64,
            u.cache_read_tokens as f64,
            u.cache_write_tokens as f64,
            u.output_tokens as f64,
        );
        u.cost_usd = Some(
            (noncached * m.cost_in
                + cached * m.cost_cache_read
                + written * m.cost_cache_write
                + out * m.cost_out)
                / M,
        );
        u.saved_usd = Some((cached * (m.cost_in - m.cost_cache_read)).max(0.0) / M);
    }

    /// Переключает провайдера/модель. Ключ ожидается в env/credentials (TUI
    /// сохраняет его до отправки команды). Обновляет цены/лимиты из каталога
    /// и персистит выбор в `~/.makedo/config.toml`.
    /// Пересобирает системный промпт под новую доктрину: база + скиллы (ponytail +
    /// доктрина). Действует со следующего турна (промпт шлётся в каждом запросе).
    fn rebuild_prompt(&mut self, doctrine: makedo_config::Doctrine) {
        self.cfg.doctrine = doctrine;
        let skills = makedo_skills::load(
            self.cfg.global_dir.as_deref(),
            &self.cfg.cwd,
            self.cfg.ponytail,
            doctrine,
        );
        self.cfg.system_prompt = format!(
            "{}{}",
            self.cfg.base_system_prompt,
            makedo_skills::render(&skills)
        );
    }

    async fn publish_model(&self) {
        let _ = self
            .events
            .send(SessionEvent::ModelChanged {
                model: self.cfg.provider.model.clone(),
                effort_levels: self.effort.levels.clone(),
                effort: self.effort.selected.clone(),
            })
            .await;
    }

    async fn set_effort(&mut self, value: &str) {
        match self.effort.set(value, self.cfg.global_dir.as_deref()) {
            Ok(()) => {
                self.publish_model().await;
                let _ = self
                    .events
                    .send(SessionEvent::Notice(format!(
                        "effort → {value} (next request)"
                    )))
                    .await;
            }
            Err(error) => {
                let _ = self.events.send(SessionEvent::Error(error)).await;
            }
        }
    }

    async fn switch_model(&mut self, kind: &str, model: &str) {
        let mut pc = if self.cfg.provider.kind == kind {
            self.cfg.provider.clone()
        } else {
            makedo_config::ProviderConfig {
                kind: kind.to_owned(),
                ..makedo_config::ProviderConfig::default()
            }
        };
        pc.model = model.to_owned();
        match makedo_providers::from_config(&pc) {
            Ok(provider) => {
                self.provider = provider;
                self.context = context::Budget::default();
                self.effort =
                    makedo_providers::effort::Settings::load(&pc, self.cfg.global_dir.as_deref());
                self.cfg.provider = pc;
                self.publish_model().await;
                let catalog =
                    makedo_providers::catalog::load(makedo_config::cache_dir().as_deref());
                let info = makedo_providers::catalog::find_model(&catalog, Some(kind), model)
                    .map(|(_, m)| m);
                self.cfg.model_info = info;
                self.cfg.context_window = info.map(|m| m.context).filter(|c| *c > 0);
                let _ = makedo_config::persist_provider(kind, model);
                let _ = self
                    .events
                    .send(SessionEvent::Notice(format!("switched to {kind}/{model}")))
                    .await;
            }
            Err(e) => {
                let _ = self
                    .events
                    .send(SessionEvent::Error(format!("cannot switch: {e}")))
                    .await;
            }
        }
    }

    /// Прямой вызов тула (слэш-команда). Недоступный `image_gen` объясняется явно.
    async fn run_tool_direct(
        &mut self,
        name: String,
        args: serde_json::Value,
        cmd_rx: &mut mpsc::Receiver<SessionCommand>,
    ) -> bool {
        if self.registry.get(&name).is_none() {
            let msg = if name == "image_gen" {
                "image_gen недоступен: OpenRouter не подключён. Задай OPENROUTER_API_KEY \
                 в окружении и перезапусти makedo."
                    .to_owned()
            } else {
                format!("tool `{name}` is not available")
            };
            let _ = self.events.send(SessionEvent::Error(msg)).await;
            return false;
        }
        let call_id = format!("direct-{}", self.next_request_id);
        self.next_request_id += 1;
        let ctx = ToolCtx {
            progress: Some(makedo_tools::Progress {
                events: self.events.clone(),
                call_id: String::new(),
            }),
            cancel: CancellationToken::new(),
            cwd: self.cfg.cwd.clone(),
            spill_dir: self.cfg.spill_dir.clone(),
            shim_dir: self.cfg.shim_dir.clone(),
            capture_bytes: self.cfg.capture_bytes,
        };
        let batch = executor::run_batch(
            Arc::clone(&self.registry),
            &ctx,
            vec![ToolCall {
                call_id: call_id.clone(),
                name: name.clone(),
                args,
            }],
            1,
        );
        tokio::pin!(batch);
        loop {
            tokio::select! {
                _ = &mut batch => break,
                command = cmd_rx.recv(), if !ctx.cancel.is_cancelled() => match command {
                    Some(SessionCommand::Cancel) | None => ctx.cancel.cancel(),
                    Some(command) => {
                        self.queue_command(command).await;
                        if self.steering { ctx.cancel.cancel(); }
                    }
                },
            }
        }
        ctx.cancel.is_cancelled() && !self.steering
    }

    /// Парсит аргументы `todo_write` в записи панели (для события TUI).
    fn parse_todos(args: &serde_json::Value) -> Vec<TodoEntry> {
        args["todos"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|t| {
                        let content = t["content"].as_str()?.to_owned();
                        let status = match t["status"].as_str()? {
                            "in_progress" => TodoState::InProgress,
                            "completed" => TodoState::Completed,
                            _ => TodoState::Pending,
                        };
                        Some(TodoEntry { content, status })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Исполняет одобренные коллы батчем и заносит результаты в историю.
    /// Поллит `cmd_rx` во время батча: `Cancel` → файрит `cancel` (иначе Esc во
    /// время исполнения тулов не работает). Пары `tool_use`/`tool_result` целы:
    /// `run_batch` возвращает результат на каждый колл даже при отмене.
    async fn execute_approved(
        &mut self,
        approved: Vec<ToolCall>,
        cancel: &CancellationToken,
        cmd_rx: &mut mpsc::Receiver<SessionCommand>,
    ) {
        let ctx = ToolCtx {
            progress: Some(makedo_tools::Progress {
                events: self.events.clone(),
                call_id: String::new(),
            }),
            cancel: cancel.clone(),
            cwd: self.cfg.cwd.clone(),
            spill_dir: self.cfg.spill_dir.clone(),
            shim_dir: self.cfg.shim_dir.clone(),
            capture_bytes: self.cfg.capture_bytes,
        };
        // не-read-only колл в турне → триггер verification gate после турна
        if approved.iter().any(|c| {
            c.name != "memory_write"
                && self
                    .registry
                    .get(&c.name)
                    .is_some_and(|t| !t.is_read_only())
        }) {
            self.turn_had_writes = true;
        }
        // пути пишущих коллов: wiki-файлы из них после батча попадают в индекс поиска
        let written: Vec<(String, Vec<PathBuf>)> = approved
            .iter()
            .filter_map(|c| {
                let tool = self.registry.get(&c.name)?;
                (!tool.is_read_only()).then(|| (c.call_id.clone(), tool.access_paths_json(&c.args)))
            })
            .collect();
        // todo_write перехватывается ДО исполнения: args → записи панели
        let todo_updates: std::collections::HashMap<String, Vec<TodoEntry>> = approved
            .iter()
            .filter(|c| c.name == makedo_tools::builtin::todo::TODO_TOOL_NAME)
            .map(|c| (c.call_id.clone(), Self::parse_todos(&c.args)))
            .collect();
        let batch = executor::run_batch(
            Arc::clone(&self.registry),
            &ctx,
            approved,
            self.cfg.concurrency,
        );
        tokio::pin!(batch);
        let results = loop {
            tokio::select! {
                r = &mut batch => break r,
                cmd = cmd_rx.recv(), if !cancel.is_cancelled() => match cmd {
                    Some(SessionCommand::Cancel) | None => cancel.cancel(),
                    Some(SessionCommand::SetEffort(value)) => self.set_effort(&value).await,
                    Some(cmd) => {
                        self.queue_command(cmd).await;
                        if self.steering { cancel.cancel(); }
                    }
                },
            }
        };
        if let Some(memory) = &self.cfg.memory {
            let paths: Vec<PathBuf> = written
                .into_iter()
                .filter(|(id, _)| results.iter().any(|r| &r.call_id == id && !r.is_error))
                .flat_map(|(_, paths)| paths)
                .map(|p| self.cfg.cwd.join(p))
                .collect();
            if let Err(error) = memory.memory.import_wiki_files(&paths) {
                let _ = self.events.try_send(SessionEvent::Error(format!(
                    "Wiki index update failed: {error}"
                )));
            }
        }
        for res in &results {
            self.push_tool_result(&res.call_id, &res.output);
            if !res.is_error
                && let Some(todos) = todo_updates.get(&res.call_id)
            {
                let _ = self
                    .events
                    .send(SessionEvent::TodoUpdated(todos.clone()))
                    .await;
            }
        }
    }

    async fn check_permission(
        &mut self,
        call: &ToolCall,
        cmd_rx: &mut mpsc::Receiver<SessionCommand>,
    ) -> Option<Verdict> {
        if self.ruleset.is_allow_all() {
            return Some(Verdict::Approved);
        }
        let doom = self.doom.observe(&call.name, &call.args);
        let subjects = if call.name == "apply_patch" {
            let paths = self.registry.get(&call.name)?.access_paths_json(&call.args);
            // A patch may touch several files: every resolved path needs approval.
            // Canonical paths prevent wiki-scoped approval escaping through ../ or symlinks.
            if paths.is_empty() {
                // Invalid patches cannot mutate anything; let the tool return its
                // parser/schema diagnostic instead of a misleading permission denial.
                return Some(Verdict::Approved);
            }
            paths
                .iter()
                .map(|path| {
                    makedo_tools::executor::lock_path(&self.cfg.cwd, path)
                        .to_string_lossy()
                        .into_owned()
                })
                .collect::<Vec<_>>()
        } else {
            vec![subject_of(&call.name, &call.args)]
        };
        for subject in &subjects {
            match self
                .check_subject_permission(call, subject, doom, cmd_rx)
                .await
            {
                Some(Verdict::Approved) => {}
                other => return other,
            }
        }
        Some(Verdict::Approved)
    }

    async fn check_subject_permission(
        &mut self,
        call: &ToolCall,
        subject: &str,
        doom: bool,
        cmd_rx: &mut mpsc::Receiver<SessionCommand>,
    ) -> Option<Verdict> {
        if self.ruleset.is_allow_all() {
            return Some(Verdict::Approved);
        }
        let action = if doom {
            Action::Ask // порог одинаковых коллов — принудительный вопрос
        } else if call.name == "bash" && self.approved.bash_allowed(subject) {
            Action::Allow // персистентная маска: программа уже одобрена ранее
        } else {
            self.ruleset
                .evaluate(&call.name, subject)
                .unwrap_or_else(|| {
                    // без правила: read-only тулы (wiki_*, search_tool, …) безопасны
                    let read_only = self
                        .registry
                        .get(&call.name)
                        .is_some_and(|t| t.is_read_only());
                    if read_only {
                        Action::Allow
                    } else {
                        Action::Ask
                    }
                })
        };
        match action {
            Action::Allow => Some(Verdict::Approved),
            Action::Deny => Some(Verdict::Denied),
            Action::Ask => {
                self.next_request_id += 1;
                let id = self.next_request_id;
                let looped = if doom { " [repeated call]" } else { "" };
                let _ = self
                    .events
                    .send(SessionEvent::PermissionAsk {
                        request_id: id,
                        description: format!("{} {subject}{looped}", call.name),
                    })
                    .await;
                loop {
                    match cmd_rx.recv().await? {
                        SessionCommand::PermissionReply {
                            request_id,
                            approve,
                            always,
                        } if request_id == id => {
                            if always && approve {
                                if call.name == "bash" {
                                    // персистентно: одобряем программы команды по маске,
                                    // пишем в .makedo/permissions.json — переживёт сессию
                                    self.approved.approve_bash(subject);
                                } else {
                                    // прочие тулы — сессионное правило тул+субъект
                                    self.ruleset.allow_always(&call.name, subject);
                                }
                            }
                            return Some(if approve {
                                Verdict::Approved
                            } else {
                                Verdict::Denied
                            });
                        }
                        // allow-all прямо из запроса: включаем режим и одобряем
                        // текущий колл (иначе SetAllowAll во время турна терялся)
                        SessionCommand::SetAllowAll => {
                            self.enable_yolo().await;
                            return Some(Verdict::Approved);
                        }
                        SessionCommand::SetEffort(value) => self.set_effort(&value).await,
                        SessionCommand::Cancel => return None,
                        cmd => {
                            self.queue_command(cmd).await;
                            if self.steering {
                                return None;
                            }
                            if self.ruleset.is_allow_all() {
                                return Some(Verdict::Approved);
                            }
                        }
                    }
                }
            }
        }
    }

    fn push_assistant(
        &mut self,
        text: &str,
        calls: &[(String, String, String)],
        output: Vec<serde_json::Value>,
    ) {
        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(AssistantPart::Text {
                text: text.to_owned(),
            });
        }
        for (id, name, args) in calls {
            parts.push(AssistantPart::ToolCall {
                call_id: id.clone(),
                name: name.clone(),
                input: serde_json::from_str(args)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new())),
            });
        }
        if !output.is_empty() {
            parts.push(AssistantPart::OpenAiOutput { items: output });
        }
        if !parts.is_empty() {
            let m = Message::Assistant { content: parts };
            self.record(&m);
            self.history.push(m);
        }
    }

    /// Синтетические interrupted-результаты: пары `tool_use`/`tool_result` не рвутся
    /// (Anthropic отвергает историю с висячим `tool_use` — инвариант обоих движков).
    fn push_interrupted_results(&mut self, calls: &[(String, String, String)]) {
        for (id, _, _) in calls {
            self.push_tool_result(id, "[Tool execution was interrupted]");
        }
    }

    fn push_tool_result(&mut self, call_id: &str, content: &str) {
        let m = Message::ToolResult {
            call_id: call_id.to_owned(),
            content: content.to_owned(),
        };
        self.record(&m);
        self.history.push(m);
    }

    /// Append-only запись сообщения в `.makedo/sessions/<ts>.jsonl`. Файл только
    /// дополняется — сессии нельзя перетереть или удалить из кода (требование).
    fn record(&self, m: &Message) {
        self.record_value(m);
    }

    fn record_value(&self, value: &impl serde::Serialize) {
        if let Some(memory) = &self.cfg.memory
            && let Err(error) = memory.record(value)
        {
            let _ = self.events.try_send(SessionEvent::Error(format!(
                "Memory journal write failed: {error}"
            )));
        }
        if let Some(path) = &self.cfg.session_file
            && let Err(error) = history::append(path, value)
        {
            let _ = self.events.try_send(SessionEvent::Error(format!(
                "Session journal write failed: {error}"
            )));
        }
    }

    async fn discard_pending_prompts(&mut self) {
        let mut keep = VecDeque::new();
        while let Some(command) = self.pending.pop_front() {
            if let SessionCommand::QueuedPrompt { id, .. } = command {
                self.record_value(&serde_json::json!({"cancelled": id}));
                let _ = self.events.send(SessionEvent::PromptCancelled { id }).await;
            } else {
                keep.push_back(command);
            }
        }
        self.pending = keep;
    }

    async fn apply_pending(&mut self) {
        while let Some(command) = self.pending.pop_front() {
            match command {
                SessionCommand::QueuedPrompt { id, text } => {
                    self.accept_prompt(text, Some(id)).await;
                }
                SessionCommand::SetEffort(value) => self.set_effort(&value).await,
                SessionCommand::SwitchModel { kind, model } => {
                    self.switch_model(&kind, &model).await;
                }
                SessionCommand::SetDoctrine(doctrine) => self.rebuild_prompt(doctrine),
                SessionCommand::SetPonytail(level) => {
                    self.cfg.ponytail = level;
                    self.rebuild_prompt(self.cfg.doctrine);
                }
                SessionCommand::SetVerifyCmds(cmds) => self.cfg.verify_cmds = cmds,
                SessionCommand::SetAllowAll => self.enable_yolo().await,
                SessionCommand::PermissionReply { .. } => {}
                other => {
                    self.pending.push_front(other);
                    break;
                }
            }
        }
    }

    async fn enable_yolo(&mut self) {
        self.ruleset = Ruleset::allow_all();
        let _ = self
            .events
            .send(SessionEvent::PermissionsChanged { allow_all: true })
            .await;
        let _ = self
            .events
            .send(SessionEvent::Notice(
                "YOLO enabled: all tool permissions granted for this session.".into(),
            ))
            .await;
    }

    async fn configure_prompt(&mut self, text: &str) -> bool {
        if permission::requests_yolo(text) {
            self.enable_yolo().await;
            return permission::only_yolo(text);
        }
        false
    }

    async fn queue_command(&mut self, command: SessionCommand) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if matches!(command, SessionCommand::SetAllowAll) {
            self.enable_yolo().await;
            return;
        }
        let (text, force) = match command {
            SessionCommand::Prompt(text) => (text, false),
            SessionCommand::Steer(text) => (text, true),
            other => {
                self.pending.push_back(other);
                return;
            }
        };
        if !text.is_empty() && !self.configure_prompt(&text).await {
            let id = format!(
                "{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos()),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            self.record_value(&serde_json::json!({"queued": {"id": id, "text": text}}));
            self.pending.push_back(SessionCommand::QueuedPrompt {
                id: id.clone(),
                text: text.clone(),
            });
            let _ = self
                .events
                .send(SessionEvent::PromptQueued { id, text })
                .await;
        }
        if force {
            self.steering = self
                .pending
                .iter()
                .any(|c| matches!(c, SessionCommand::QueuedPrompt { .. }));
        }
    }

    async fn accept_prompt(&mut self, text: String, id: Option<String>) {
        // Поиск по памяти делает харнесс, а не модель: дешёвый поиск по ключевым словам
        // дописывается к сообщению пользователя. Системный промпт и кэш префикса не меняются.
        let hints = self
            .cfg
            .memory
            .as_ref()
            .and_then(|m| m.memory.hints(&text).ok())
            .filter(|hints| !hints.is_empty());
        let content = match hints {
            Some(hints) => format!(
                "{text}\n\n[makedo: project memory on this task. Start from the files a line names. \
                 Open a hit (memory_read(id), wiki_node(title)) only when it is clearly about \
                 the task and its line is not enough]\n{hints}"
            ),
            None => text.clone(),
        };
        let message = Message::User { content };
        if let Some(id) = &id {
            self.record_value(&serde_json::json!({"delivered": id, "message": message}));
        } else {
            self.record(&message);
        }
        self.history.push(message);
        let _ = self
            .events
            .send(SessionEvent::PromptAccepted {
                text,
                queued_id: id,
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn prune_clears_old_tool_results_keeps_tail() {
        let (event_tx, _event_rx) = mpsc::channel(8);
        let mut actor = Actor {
            provider: Box::new(NoopProvider),
            registry: Arc::new(makedo_tools::registry::Registry::default()),
            ruleset: Ruleset::default_policy(),
            approved: permission::ApprovedStore::default(),
            cfg: SessionConfig {
                cwd: std::env::temp_dir(),
                spill_dir: std::env::temp_dir(),
                concurrency: 1,
                ponytail: makedo_config::PonytailLevel::Off,
                ..SessionConfig::default()
            },
            history: Vec::new(),
            context: context::Budget::default(),
            effort: makedo_providers::effort::Settings::default(),
            events: event_tx,
            doom: DoomLoopGuard::default(),
            next_request_id: 0,
            turn_usage: TurnUsage::default(),
            turn_had_writes: false,
            steering: false,
            pending: VecDeque::new(),
        };
        // старый большой результат + свежий хвост. 2 новейших сообщения защищены
        // безусловно (параллельные tool_result'ы, которые модель ещё не читала),
        // поэтому для проверки прунинга старого нужен ≥3-й элемент.
        actor.history.push(Message::ToolResult {
            call_id: "old".into(),
            content: "x".repeat(300_000),
        });
        actor.history.push(Message::ToolResult {
            call_id: "mid".into(),
            content: "m".repeat(150_000),
        });
        actor.history.push(Message::ToolResult {
            call_id: "recent".into(),
            content: "y".repeat(150_000),
        });
        let mut req = actor.request(MAX_OUTPUT_TOKENS);
        actor.context.fit(&mut req, None).unwrap();
        actor.history = req.messages;
        let old = &actor.history[0];
        let recent = &actor.history[2];
        // старый (вне защищённого хвоста и вне 2 новейших) — очищен
        assert!(
            matches!(old, Message::ToolResult { content, .. } if content == "[Old tool result content cleared]")
        );
        // новейший — нетронут (защищён)
        assert!(matches!(recent, Message::ToolResult { content, .. } if content.len() == 150_000));
    }

    fn test_actor(pf: Option<PathBuf>, event_tx: mpsc::Sender<SessionEvent>) -> Actor {
        Actor {
            provider: Box::new(NoopProvider),
            registry: Arc::new(makedo_tools::registry::Registry::default()),
            ruleset: Ruleset::default_policy(),
            approved: permission::ApprovedStore::load(pf.clone()),
            cfg: SessionConfig {
                cwd: std::env::temp_dir(),
                spill_dir: std::env::temp_dir(),
                concurrency: 1,
                permissions_file: pf,
                ponytail: makedo_config::PonytailLevel::Off,
                ..SessionConfig::default()
            },
            history: Vec::new(),
            context: context::Budget::default(),
            effort: makedo_providers::effort::Settings::default(),
            events: event_tx,
            doom: DoomLoopGuard::default(),
            next_request_id: 0,
            turn_usage: TurnUsage::default(),
            turn_had_writes: false,
            steering: false,
            pending: VecDeque::new(),
        }
    }

    /// git-репозиторий во временной папке, память и актор со встроенными тулами
    fn memory_actor(
        tag: &str,
    ) -> (
        PathBuf,
        makedo_memory::Memory,
        Actor,
        mpsc::Receiver<SessionEvent>,
    ) {
        let root = std::env::temp_dir().join(format!("makedo-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        makedo_memory::git(&root, &["init", "-b", "main"]).unwrap();
        let memory = makedo_memory::Memory::open(
            &root,
            makedo_config::MemoryConfig {
                embedding_url: "http://127.0.0.1:1/api/embed".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let (events, kept_open) = mpsc::channel(64);
        let mut actor = test_actor(None, events);
        actor.cfg.cwd = root.clone();
        actor.cfg.spill_dir = root.join("spill");
        actor.cfg.memory = Some(memory.start_session("main", &[]).unwrap());
        let mut registry = makedo_tools::registry::Registry::default();
        makedo_tools::builtin::register_all(&mut registry).unwrap();
        actor.registry = Arc::new(registry);
        (root, memory, actor, kept_open)
    }

    #[tokio::test]
    async fn wiki_file_written_by_a_tool_is_searchable_without_a_restart() {
        let (root, memory, mut actor, _kept_open) = memory_actor("wiki-hook");
        let (_commands, mut incoming) = mpsc::channel(1);
        let write = |call_id: &str, path: &str, content: &str| ToolCall {
            call_id: call_id.into(),
            name: "write".into(),
            args: serde_json::json!({"file_path": path, "content": content}),
        };
        actor
            .execute_approved(
                vec![
                    write(
                        "wiki",
                        ".makedo/wiki/Feed Service.md",
                        "Ingests the rubberband feed.\n",
                    ),
                    write("code", "notes.md", "rubberband outside the vault\n"),
                ],
                &CancellationToken::new(),
                &mut incoming,
            )
            .await;
        let hits = memory
            .search(makedo_memory::SearchArgs {
                task: "rubberband".into(),
                context: None,
                hypothesis: None,
                kind: Some("wiki".into()),
                limit: None,
                all_branches: None,
            })
            .await
            .unwrap()
            .results;
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0]["title"], "Feed Service");

        // и следующий промпт получает подсказку от харнесса, без вызова тула моделью
        actor
            .accept_prompt("how is the rubberband feed ingested?".into(), None)
            .await;
        let content = serde_json::to_string(actor.history.last().unwrap()).unwrap();
        assert!(content.contains("how is the rubberband feed ingested?\\n\\n[makedo: "));
        assert!(content.contains("Feed Service"), "{content}");
        std::fs::remove_dir_all(root).unwrap();
    }

    // турн, которому пришлось искать, оставляет заметку — без участия модели
    #[tokio::test]
    async fn turn_that_had_to_search_leaves_a_note_for_the_next_prompt() {
        let (root, _memory, mut actor, mut events) = memory_actor("turn-note");
        let call = |name: &str, args: serde_json::Value| Message::Assistant {
            content: vec![AssistantPart::ToolCall {
                call_id: name.into(),
                name: name.into(),
                input: args,
            }],
        };
        let says = |text: &str| Message::Assistant {
            content: vec![AssistantPart::Text { text: text.into() }],
        };
        let asked = "where does the gizmo tool pick its output mode?";
        actor.history = vec![
            Message::User {
                content: format!("{asked}\n\n[makedo: hints]"),
            },
            call("grep", serde_json::json!({"pattern": "output"})),
            call("read", serde_json::json!({"file_path": "src/shell.rs"})),
            call(
                "read",
                serde_json::json!({"file_path": root.join("src/unrelated.rs")}),
            ),
            says("It is chosen in `shell.rs`; the default is Status."),
        ];
        actor.remember_turn(asked);
        assert!(
            matches!(events.try_recv(), Ok(SessionEvent::Notice(n)) if n.starts_with("memory: noted"))
        );
        actor.remember_turn(asked); // повтор того же вопроса заметку не дублирует
        // один read — не поиск: такой турн ничего не оставляет
        actor.history = vec![
            Message::User {
                content: "gizmo output mode in short".into(),
            },
            call("read", serde_json::json!({"file_path": "src/shell.rs"})),
            says("See shell.rs."),
        ];
        actor.remember_turn("gizmo output mode in short");
        actor
            .accept_prompt("change the gizmo output mode default".into(), None)
            .await;
        let content = serde_json::to_string(actor.history.last().unwrap()).unwrap();
        assert!(
            content.contains(&format!(
                "{asked} — Files: src/shell.rs It is chosen in `shell.rs`"
            )),
            "{content}"
        );
        assert_eq!(content.matches("- note #").count(), 1, "{content}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn patch_permission_checks_every_resolved_path() {
        let root =
            std::env::temp_dir().join(format!("makedo-patch-permission-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".makedo/wiki")).unwrap();
        std::os::unix::fs::symlink(&root, root.join(".makedo/wiki/escape")).unwrap();
        let (events, _) = mpsc::channel(8);
        let mut actor = test_actor(None, events);
        actor.cfg.cwd = root.clone();
        let mut registry = makedo_tools::registry::Registry::default();
        makedo_tools::builtin::register_all(&mut registry).unwrap();
        actor.registry = Arc::new(registry);
        actor
            .ruleset
            .push(permission::Rule::new("apply_patch", "*", Action::Deny).unwrap());
        actor.ruleset.push(
            permission::Rule::new("apply_patch", "**/.makedo/wiki/**", Action::Allow).unwrap(),
        );
        let (_commands, mut incoming) = mpsc::channel(1);
        for (suffix, allowed) in [
            ("", true),
            ("*** Add File: outside\n+bad\n", false),
            ("*** Add File: .makedo/wiki/../../outside\n+bad\n", false),
            ("*** Add File: .makedo/wiki/escape/outside\n+bad\n", false),
        ] {
            let call = ToolCall {
                call_id: "patch".into(),
                name: "apply_patch".into(),
                args: serde_json::json!({"patch": format!("*** Begin Patch\n*** Add File: .makedo/wiki/ok\n+ok\n{suffix}*** End Patch")}),
            };
            let verdict = actor.check_permission(&call, &mut incoming).await;
            assert_eq!(matches!(verdict, Some(Verdict::Approved)), allowed);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn always_allow_bash_stops_reasking_same_program() {
        let dir = std::env::temp_dir().join(format!("makedo-perm-it-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let pf = dir.join("permissions.json");
        let _ = std::fs::remove_file(&pf);

        let (event_tx, mut event_rx) = mpsc::channel(64);
        let mut actor = test_actor(Some(pf.clone()), event_tx);

        // первый bash: спросит → отвечаем «always allow» (заранее кладём reply)
        let (cmd_tx, mut cmd_rx) = mpsc::channel(8);
        cmd_tx
            .send(SessionCommand::PermissionReply {
                request_id: 1,
                approve: true,
                always: true,
            })
            .await
            .unwrap();
        let call1 = ToolCall {
            call_id: "c1".into(),
            name: "bash".into(),
            args: serde_json::json!({ "command": "cargo test" }),
        };
        let v1 = actor.check_permission(&call1, &mut cmd_rx).await;
        assert!(matches!(v1, Some(Verdict::Approved)), "первый — одобрен");
        // ушёл PermissionAsk
        assert!(matches!(
            event_rx.try_recv(),
            Ok(SessionEvent::PermissionAsk { .. })
        ));

        // вторая команда той же программы: канал reply ПУСТ → если бы спрашивала,
        // check_permission завис бы. Значит должна авто-одобриться сразу.
        let call2 = ToolCall {
            call_id: "c2".into(),
            name: "bash".into(),
            args: serde_json::json!({ "command": "cargo build --release" }),
        };
        let v2 = actor.check_permission(&call2, &mut cmd_rx).await;
        assert!(
            matches!(v2, Some(Verdict::Approved)),
            "одобренная программа не должна переспрашиваться"
        );
        // и никакого нового PermissionAsk
        assert!(
            event_rx.try_recv().is_err(),
            "не должно быть повторного запроса"
        );

        // персистентность: свежий актор из того же файла помнит
        let (etx2, _erx2) = mpsc::channel(8);
        let actor2 = test_actor(Some(pf.clone()), etx2);
        assert!(
            actor2.approved.bash_allowed("cargo run"),
            "не переехало на диск"
        );
        let _ = std::fs::remove_file(&pf);
    }

    #[test]
    fn summarize_is_compact_grok_style() {
        // длинный read → счётчик строк, а не сырой текст
        let long = (0..220)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(super::summarize("read", &long, false), "220 lines");
        // grep → matches
        assert_eq!(super::summarize("grep", "a\nb\nc", false), "3 matches");
        // короткий одностроковый вывод — как есть
        assert_eq!(
            super::summarize("wiki_node", "PokerDomain [module]", false),
            "PokerDomain [module]"
        );
        // ошибка — само сообщение
        assert_eq!(
            super::summarize("bash", "boom: no such file", true),
            "boom: no such file"
        );
        // пусто
        assert_eq!(super::summarize("bash", "", false), "ok");
    }

    #[test]
    fn set_doctrine_rebuilds_prompt_with_skill() {
        let (tx, _rx) = mpsc::channel(8);
        let mut actor = test_actor(None, tx);
        actor.cfg.base_system_prompt = "BASE.".into();
        actor.rebuild_prompt(makedo_config::Doctrine::Ddd);
        assert!(
            actor.cfg.system_prompt.starts_with("BASE."),
            "база потеряна"
        );
        assert!(
            actor.cfg.system_prompt.contains("doctrine-ddd"),
            "нет DDD-скилла"
        );
        actor.rebuild_prompt(makedo_config::Doctrine::None);
        assert!(
            !actor.cfg.system_prompt.contains("doctrine-ddd"),
            "none не убрал доктрину"
        );
    }

    struct NoopProvider;
    impl makedo_providers::Provider for NoopProvider {
        fn stream(&self, _req: CompletionRequest) -> makedo_providers::EventStream {
            Box::pin(futures_util::stream::empty())
        }
    }

    /// Первые `fails` вызовов стрима падают сразу (обрыв коннекта), дальше — текст+финиш.
    struct FlakyProvider {
        fails: std::sync::atomic::AtomicU32,
    }
    impl makedo_providers::Provider for FlakyProvider {
        fn stream(&self, _req: CompletionRequest) -> makedo_providers::EventStream {
            use std::sync::atomic::Ordering;
            if self
                .fails
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                return Box::pin(futures_util::stream::once(async {
                    Err(makedo_providers::ProviderError::Protocol(
                        "error decoding response body".to_owned(),
                    ))
                }));
            }
            Box::pin(futures_util::stream::iter(vec![
                Ok(LlmEvent::TextDelta("ok".to_owned())),
                Ok(LlmEvent::Finished {
                    reason: StopReason::Stop,
                    usage: makedo_providers::Usage::default(),
                }),
            ]))
        }
    }

    #[tokio::test]
    async fn stream_drop_before_content_is_retried() {
        let (tx, mut rx) = mpsc::channel(64);
        let mut actor = test_actor(None, tx);
        actor.provider = Box::new(FlakyProvider {
            fails: std::sync::atomic::AtomicU32::new(MAX_STREAM_RETRIES),
        });
        actor.history.push(Message::User {
            content: "hi".into(),
        });
        let (_ctx, mut cmd_rx) = mpsc::channel::<SessionCommand>(4);
        let cancelled = actor.turn(&mut cmd_rx).await;
        assert!(!cancelled);
        // турн выжил: в истории есть ответ ассистента с текстом
        assert!(matches!(
            actor.history.last(),
            Some(Message::Assistant { content })
                if matches!(content.first(), Some(makedo_providers::AssistantPart::Text { text }) if text == "ok")
        ));
        // среди событий были Notice о ретраях, но не Error
        let mut notices = 0;
        let mut errors = 0;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                SessionEvent::Notice(n) if n.contains("retry") => notices += 1,
                SessionEvent::Error(_) => errors += 1,
                _ => {}
            }
        }
        assert_eq!(notices, MAX_STREAM_RETRIES as usize);
        assert_eq!(errors, 0, "после успешного ретрая ошибок быть не должно");
    }

    #[tokio::test]
    async fn stream_drop_beyond_retries_errors_out() {
        let (tx, mut rx) = mpsc::channel(64);
        let mut actor = test_actor(None, tx);
        actor.provider = Box::new(FlakyProvider {
            fails: std::sync::atomic::AtomicU32::new(MAX_STREAM_RETRIES + 1),
        });
        actor.history.push(Message::User {
            content: "hi".into(),
        });
        let (_ctx, mut cmd_rx) = mpsc::channel::<SessionCommand>(4);
        let cancelled = actor.turn(&mut cmd_rx).await;
        assert!(!cancelled);
        let mut errors = 0;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, SessionEvent::Error(_)) {
                errors += 1;
            }
        }
        assert_eq!(errors, 1, "исчерпав ретраи, турн отдаёт ошибку");
    }

    struct ErrorScriptProvider {
        script: std::sync::Mutex<
            std::collections::VecDeque<Vec<Result<LlmEvent, makedo_providers::ProviderError>>>,
        >,
        requests: Arc<std::sync::Mutex<Vec<CompletionRequest>>>,
    }

    impl Provider for ErrorScriptProvider {
        fn stream(&self, req: CompletionRequest) -> makedo_providers::EventStream {
            self.requests.lock().unwrap().push(req);
            Box::pin(futures_util::stream::iter(
                self.script.lock().unwrap().pop_front().unwrap(),
            ))
        }
    }

    fn overflow() -> makedo_providers::ProviderError {
        makedo_providers::ProviderError::Api {
            status: 400,
            body: r#"{"error":{"message":"This model's maximum context length is 65536 tokens. However, you requested 8192 output tokens and your prompt contains at least 57345 input tokens, for a total of at least 65537 tokens.","type":"BadRequestError","param":"input_tokens","code":400}}"#.into(),
        }
    }

    #[tokio::test]
    async fn context_400_changes_request_and_continues_without_replaying_tools() {
        let (tx, mut rx) = mpsc::channel(64);
        let mut actor = test_actor(None, tx);
        actor.cfg.context_window = Some(65_536);
        actor.history.push(Message::User {
            content: "Continue the task".into(),
        });
        actor.push_assistant(
            "",
            &[("c1".into(), "bash".into(), r#"{"command":"ls"}"#.into())],
            Vec::new(),
        );
        actor.push_tool_result("c1", &"result".repeat(8_000));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        actor.provider = Box::new(ErrorScriptProvider {
            script: std::sync::Mutex::new(std::collections::VecDeque::from([
                vec![Err(overflow())],
                vec![
                    Ok(LlmEvent::TextDelta("recovered".into())),
                    Ok(LlmEvent::Finished {
                        reason: StopReason::Stop,
                        usage: makedo_providers::Usage::default(),
                    }),
                ],
            ])),
            requests: Arc::clone(&requests),
        });
        let (_cmd_tx, mut cmd_rx) = mpsc::channel(4);
        assert!(!actor.turn(&mut cmd_rx).await);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            matches!(&requests[1].messages[2], Message::ToolResult { call_id, content } if call_id == "c1" && content.len() < 48_000)
        );
        assert_eq!(requests[0].messages.len(), requests[1].messages.len());
        assert!(
            matches!(&requests[1].messages[1], Message::Assistant { content } if matches!(&content[0], AssistantPart::ToolCall { call_id, .. } if call_id == "c1"))
        );
        drop(requests);
        let mut recovered = false;
        while let Ok(event) = rx.try_recv() {
            assert!(!matches!(event, SessionEvent::Error(_)));
            if let SessionEvent::TextDelta(text) = event {
                recovered |= text == "recovered";
            }
        }
        assert!(recovered);
    }

    #[tokio::test]
    async fn permanent_api_errors_and_partial_output_are_not_retried() {
        for (error, partial) in [
            (
                makedo_providers::ProviderError::Api {
                    status: 400,
                    body: "invalid parameter".into(),
                },
                false,
            ),
            (
                makedo_providers::ProviderError::Api {
                    status: 401,
                    body: "invalid token".into(),
                },
                false,
            ),
            (
                makedo_providers::ProviderError::MissingKey("TEST_KEY".into()),
                false,
            ),
            (overflow(), true),
        ] {
            let (tx, mut rx) = mpsc::channel(64);
            let mut actor = test_actor(None, tx);
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut events = Vec::new();
            if partial {
                events.push(Ok(LlmEvent::TextDelta("partial".into())));
            }
            events.push(Err(error));
            actor.provider = Box::new(ErrorScriptProvider {
                script: std::sync::Mutex::new(std::collections::VecDeque::from([events])),
                requests: Arc::clone(&requests),
            });
            let (_cmd_tx, mut cmd_rx) = mpsc::channel(4);
            assert!(!actor.turn(&mut cmd_rx).await);
            assert_eq!(requests.lock().unwrap().len(), 1);
            let mut errors = 0;
            while let Ok(event) = rx.try_recv() {
                if matches!(event, SessionEvent::Error(_)) {
                    errors += 1;
                }
            }
            assert_eq!(errors, 1);
        }
    }

    #[tokio::test]
    async fn context_recovery_stops_after_bounded_attempts() {
        let (tx, mut rx) = mpsc::channel(64);
        let mut actor = test_actor(None, tx);
        actor.history.push(Message::User {
            content: "task".into(),
        });
        for i in 0..20 {
            let id = format!("c{i}");
            actor.push_assistant("", &[(id.clone(), "read".into(), "{}".into())], Vec::new());
            actor.push_tool_result(&id, &"x".repeat(10_000));
        }
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        actor.provider = Box::new(ErrorScriptProvider {
            script: std::sync::Mutex::new(
                (0..=MAX_CONTEXT_RETRIES)
                    .map(|_| vec![Err(overflow())])
                    .collect(),
            ),
            requests: Arc::clone(&requests),
        });
        let (_cmd_tx, mut cmd_rx) = mpsc::channel(4);
        assert!(!actor.turn(&mut cmd_rx).await);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), MAX_CONTEXT_RETRIES as usize + 1);
        let sizes: Vec<_> = requests
            .iter()
            .map(|r| serde_json::to_vec(&r.messages).unwrap().len())
            .collect();
        drop(requests);
        assert!(sizes.windows(2).all(|w| w[1] < w[0]));
        assert!(
            std::iter::from_fn(|| rx.try_recv().ok()).any(|e| matches!(e, SessionEvent::Error(_)))
        );
    }

    #[test]
    fn output_reserve_respects_model_limit_and_small_context() {
        let (tx, _rx) = mpsc::channel(4);
        let mut actor = test_actor(None, tx);
        actor.cfg.context_window = Some(65_536);
        actor.cfg.model_info = Some(makedo_providers::catalog::ModelInfo {
            context: 65_536,
            output: 1024,
            cost_in: 0.0,
            cost_out: 0.0,
            cost_cache_read: 0.0,
            cost_cache_write: 0.0,
            tools: true,
        });
        assert_eq!(actor.request(MAX_OUTPUT_TOKENS).max_tokens, 1024);
        assert_eq!(actor.request(MAX_OUTPUT_TOKENS * 4).max_tokens, 1024);
        actor.cfg.model_info = None;
        actor.cfg.context_window = Some(4096);
        assert_eq!(actor.request(MAX_OUTPUT_TOKENS * 4).max_tokens, 1024);
    }

    #[tokio::test]
    async fn repeated_output_limits_stop_and_account_for_every_attempt() {
        let (tx, mut rx) = mpsc::channel(64);
        let mut actor = test_actor(None, tx);
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        actor.provider = Box::new(ErrorScriptProvider {
            script: std::sync::Mutex::new(
                (0..=MAX_OUTPUT_RECOVERIES)
                    .map(|_| {
                        vec![Ok(LlmEvent::Finished {
                            reason: StopReason::MaxTokens,
                            usage: makedo_providers::Usage {
                                output_tokens: 8,
                                ..makedo_providers::Usage::default()
                            },
                        })]
                    })
                    .collect(),
            ),
            requests: Arc::clone(&requests),
        });
        let (_cmd_tx, mut cmd_rx) = mpsc::channel(4);
        assert!(!actor.turn(&mut cmd_rx).await);
        assert_eq!(
            requests.lock().unwrap().len(),
            MAX_OUTPUT_RECOVERIES as usize + 1
        );
        assert_eq!(
            actor.turn_usage.output_tokens,
            8 * u64::from(MAX_OUTPUT_RECOVERIES + 1)
        );
        let errors = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                SessionEvent::Error(message) => Some(message),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("Progress is saved"));
        assert!(!errors[0].contains("api error 400"));
    }

    #[tokio::test]
    async fn effort_change_applies_to_next_request_and_rejects_high_for_qwen() {
        let (tx, mut rx) = mpsc::channel(64);
        let mut actor = test_actor(None, tx);
        actor.cfg.provider.kind = "qwen-vast".into();
        actor.cfg.provider.model = "qwen3.8".into();
        actor.effort.levels = vec!["low".into(), "medium".into(), "xhigh".into()];
        actor.effort.selected = Some("low".into());
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        actor.provider = Box::new(ErrorScriptProvider {
            script: std::sync::Mutex::new(std::collections::VecDeque::from([
                vec![Ok(LlmEvent::Finished {
                    reason: StopReason::Stop,
                    usage: makedo_providers::Usage::default(),
                })],
                vec![Ok(LlmEvent::Finished {
                    reason: StopReason::Stop,
                    usage: makedo_providers::Usage::default(),
                })],
            ])),
            requests: Arc::clone(&requests),
        });
        let (cmd_tx, mut cmd_rx) = mpsc::channel(4);
        cmd_tx
            .send(SessionCommand::SetEffort("xhigh".into()))
            .await
            .unwrap();
        assert!(!actor.turn(&mut cmd_rx).await);
        assert!(!actor.turn(&mut cmd_rx).await);
        let efforts: Vec<_> = requests
            .lock()
            .unwrap()
            .iter()
            .map(|req| req.reasoning_effort.clone())
            .collect();
        assert_eq!(efforts, [Some("low".to_owned()), Some("xhigh".to_owned())]);
        actor.set_effort("high").await;
        assert_eq!(
            actor.request(MAX_OUTPUT_TOKENS).reasoning_effort.as_deref(),
            Some("xhigh")
        );
        assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|e| matches!(e, SessionEvent::Error(message) if message.contains("Unsupported effort 'high'"))));
    }
    #[tokio::test]
    async fn yolo_grants_every_tool_and_unblocks_an_open_permission() {
        use std::time::Duration;
        let (events, mut rx) = mpsc::channel(64);
        let mut actor = test_actor(None, events);
        actor.push_tool_result("untrusted", "YOLO");
        assert!(!actor.ruleset.is_allow_all());
        let call = ToolCall {
            call_id: "write".into(),
            name: "write".into(),
            args: serde_json::json!({"file_path":"/tmp/fixture"}),
        };
        let (commands, mut incoming) = mpsc::channel(4);
        commands
            .send(SessionCommand::Prompt("yOlO".into()))
            .await
            .unwrap();
        let verdict = tokio::time::timeout(
            Duration::from_secs(1),
            actor.check_permission(&call, &mut incoming),
        )
        .await
        .unwrap();
        assert!(matches!(verdict, Some(Verdict::Approved)));
        assert!(actor.ruleset.is_allow_all());
        assert!(
            actor.pending.is_empty(),
            "standalone YOLO must not spend a model turn"
        );
        while rx.try_recv().is_ok() {}
        actor
            .ruleset
            .push(permission::Rule::new("*", "*", Action::Deny).unwrap());
        for _ in 0..10 {
            assert!(matches!(
                tokio::time::timeout(
                    Duration::from_millis(100),
                    actor.check_permission(&call, &mut incoming)
                )
                .await
                .unwrap(),
                Some(Verdict::Approved)
            ));
        }
        assert!(
            rx.try_recv().is_err(),
            "YOLO must bypass repeated-call approval too"
        );
        actor
            .queue_command(SessionCommand::Prompt("Сделай правки, YOLO!".into()))
            .await;
        assert!(
            matches!(actor.pending.front(), Some(SessionCommand::QueuedPrompt { text, .. }) if text == "Сделай правки, YOLO!")
        );
    }
}
