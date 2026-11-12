//! CLI-провайдеры: `claude -p` и `codex exec` как суб-агенты (без API-ключа,
//! авторизация — логин самого CLI: подписка Max / `ChatGPT`).
//!
//! Режим «делегат»: CLI крутит СВОЙ цикл тулов в текущем cwd, makedo получает
//! поток текста + usage. Тулы и system-промпт makedo ему не передаются — у
//! агента-CLI свои. Сессии: `--resume` / `exec resume` по session id, так что
//! после первого турна пересылается только новое user-сообщение.
// ponytail: тулы makedo через MCP-мост (`--mcp-config`) — если делегата не хватит

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};

use crate::{
    CompletionRequest, EventStream, LlmEvent, Message, Provider, ProviderError, StopReason, Usage,
};

/// Алиасы моделей `claude` CLI (пустая строка = дефолт CLI).
pub const CLAUDE_CLI_MODELS: &[&str] = &["default", "sonnet", "opus", "haiku"];
/// Модели `codex` CLI.
pub const CODEX_CLI_MODELS: &[&str] = &["default", "gpt-5-codex", "gpt-5"];
/// Модели `grok` CLI (Grok Build).
pub const GROK_CLI_MODELS: &[&str] = &["default", "grok-4.6"];

/// Списки моделей для `/models` у CLI-провайдеров (`None` = не CLI-провайдер).
#[must_use]
pub fn cli_models(kind: &str) -> Option<Vec<String>> {
    let list = match kind {
        "claude-cli" => CLAUDE_CLI_MODELS,
        "codex-cli" => CODEX_CLI_MODELS,
        "grok-cli" => GROK_CLI_MODELS,
        _ => return None,
    };
    Some(list.iter().map(|&s| s.to_owned()).collect())
}

/// Есть ли бинарь в `$PATH` (для подсказки в `/models`).
#[must_use]
pub fn cli_available(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Claude,
    Codex,
    Grok,
}

impl Kind {
    fn bin(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Grok => "grok",
        }
    }
}

/// `claude -p` / `codex exec` за трейтом `Provider`.
#[derive(Debug)]
pub struct AgentCli {
    kind: Kind,
    /// session id CLI после первого турна (`--resume` / `exec resume`).
    session: Arc<Mutex<Option<String>>>,
}

impl AgentCli {
    #[must_use]
    pub fn claude() -> Self {
        Self {
            kind: Kind::Claude,
            session: Arc::default(),
        }
    }

    #[must_use]
    pub fn codex() -> Self {
        Self {
            kind: Kind::Codex,
            session: Arc::default(),
        }
    }

    #[must_use]
    pub fn grok() -> Self {
        Self {
            kind: Kind::Grok,
            session: Arc::default(),
        }
    }
}

/// Промпт турна: при резюме — только последний user-ввод (история уже в сессии
/// CLI), иначе — плоская история (обычно это единственное user-сообщение).
fn build_prompt(messages: &[Message], resuming: bool) -> String {
    if resuming {
        return messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User { content } => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_default();
    }
    let mut out = String::new();
    for m in messages {
        let (prefix, text) = match m {
            Message::User { content } => ("", content.as_str()),
            Message::Assistant { content } => {
                let t: String = content
                    .iter()
                    .filter_map(|p| match p {
                        crate::AssistantPart::Text { text } => Some(text.as_str()),
                        crate::AssistantPart::ToolCall { .. }
                        | crate::AssistantPart::OpenAiOutput { .. } => None,
                    })
                    .collect();
                if t.is_empty() {
                    continue;
                }
                out.push_str("[previous assistant reply]\n");
                out.push_str(&t);
                out.push_str("\n\n");
                continue;
            }
            // system makedo'а описывает его собственные тулы — CLI не нужен
            Message::System { .. } | Message::ToolResult { .. } => continue,
        };
        out.push_str(prefix);
        out.push_str(text);
        out.push_str("\n\n");
    }
    out.trim_end().to_owned()
}

impl Provider for AgentCli {
    fn stream(&self, req: CompletionRequest) -> EventStream {
        // новый диалог в makedo (первый турн или /clear) = новая сессия CLI
        let fresh = !req
            .messages
            .iter()
            .any(|m| matches!(m, Message::Assistant { .. }));
        let sid = if fresh {
            self.session.lock().ok().and_then(|mut s| s.take())
        } else {
            self.session.lock().ok().and_then(|s| s.clone())
        };
        let prompt = build_prompt(&req.messages, sid.is_some());
        let model = (req.model != "default").then_some(req.model.as_str());

        let mut cmd = Command::new(self.kind.bin());
        match self.kind {
            Kind::Claude => {
                cmd.args(["-p", "--output-format", "stream-json", "--verbose"]);
                // acceptEdits: правки файлов без интерактива; Bash-команды CLI
                // подтверждать некому — они будут отклонены его же политикой
                cmd.args(["--permission-mode", "acceptEdits"]);
                // гибрид: системный промпт makedo (wiki-дисциплина, ponytail,
                // credentials) едет в делегат, wiki-тулы — через MCP-мост
                // `makedo mcp-serve` (см. src/mcp_wiki.rs)
                if !req.system.is_empty() {
                    cmd.args([
                        "--append-system-prompt",
                        &format!(
                            "{}\n\nThe knowledge tools (memory_search, memory_read, wiki_node, \
                             wiki_trace) are served by the `makedo` MCP server; other makedo tools \
                             are not available here — use your own equivalents.",
                            req.system
                        ),
                    ]);
                }
                if let Ok(exe) = std::env::current_exe() {
                    let mcp = serde_json::json!({
                        "mcpServers": {"makedo": {"command": exe, "args": ["mcp-serve"]}}
                    });
                    cmd.args(["--mcp-config", &mcp.to_string()]);
                    // read-only тулы знаний — преодобряем, чтобы не падали в -p
                    cmd.args([
                        "--allowedTools",
                        "mcp__makedo__memory_search,mcp__makedo__memory_read,\
                         mcp__makedo__wiki_node,mcp__makedo__wiki_trace",
                    ]);
                }
                if let Some(m) = model {
                    cmd.args(["--model", m]);
                }
                if let Some(id) = &sid {
                    cmd.args(["--resume", id]);
                }
                cmd.arg(&prompt);
            }
            Kind::Codex => {
                cmd.args([
                    "exec",
                    "--json",
                    "--skip-git-repo-check",
                    "-s",
                    "workspace-write",
                ]);
                if let Some(m) = model {
                    cmd.args(["-m", m]);
                }
                if let Some(id) = &sid {
                    cmd.args(["resume", id]);
                }
                cmd.arg(&prompt);
            }
            // Grok Build: headless-вывод — клон stream-json клода (system/init,
            // assistant, result), маппер общий. Гибрид (промпт+MCP) — позже.
            Kind::Grok => {
                cmd.args(["--output-format", "streaming-messages-json"]);
                cmd.args(["--permission-mode", "acceptEdits"]);
                if let Some(m) = model {
                    cmd.args(["-m", m]);
                }
                if let Some(id) = &sid {
                    cmd.args(["--resume", id]);
                }
                cmd.args(["-p", &prompt]);
            }
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true); // отмена = drop стрима → SIGKILL процессу

        let bin = self.kind.bin();
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("cannot start `{bin}` (is it installed and logged in?): {e}");
                return Box::pin(futures_util::stream::once(async move {
                    Err(ProviderError::Protocol(msg))
                }));
            }
        };
        let Some(stdout) = child.stdout.take() else {
            return Box::pin(futures_util::stream::once(async {
                Err(ProviderError::Protocol("no stdout pipe".to_owned()))
            }));
        };
        let mapper: Box<dyn LineMapper> = match self.kind {
            Kind::Claude | Kind::Grok => {
                Box::new(ClaudeMapper::new(bin, Arc::clone(&self.session)))
            }
            Kind::Codex => Box::new(CodexMapper::new(Arc::clone(&self.session))),
        };
        line_stream(bin, child, BufReader::new(stdout).lines(), mapper)
    }
}

/// Диалект-специфичный разбор одной JSONL-строки stdout.
trait LineMapper: Send {
    fn map(&mut self, line: &str) -> Result<Vec<LlmEvent>, ProviderError>;
    /// Был ли финальный event (result / turn.completed / ошибка).
    fn done(&self) -> bool;
}

struct Ctx {
    bin: &'static str,
    child: Option<Child>,
    lines: Lines<BufReader<ChildStdout>>,
    mapper: Box<dyn LineMapper>,
    queue: VecDeque<Result<LlmEvent, ProviderError>>,
    finished: bool,
}

fn line_stream(
    bin: &'static str,
    child: Child,
    lines: Lines<BufReader<ChildStdout>>,
    mapper: Box<dyn LineMapper>,
) -> EventStream {
    let ctx = Ctx {
        bin,
        child: Some(child),
        lines,
        mapper,
        queue: VecDeque::new(),
        finished: false,
    };
    Box::pin(futures_util::stream::unfold(ctx, |mut c| async move {
        loop {
            if let Some(item) = c.queue.pop_front() {
                return Some((item, c));
            }
            if c.finished {
                return None;
            }
            if let Ok(Some(line)) = c.lines.next_line().await {
                match c.mapper.map(&line) {
                    Ok(evs) => c.queue.extend(evs.into_iter().map(Ok)),
                    Err(e) => {
                        c.queue.push_back(Err(e));
                        c.finished = true;
                    }
                }
            } else {
                // EOF или обрыв пайпа: если финального события не было —
                // достаём stderr процесса для внятной ошибки (логин, лимиты)
                c.finished = true;
                if !c.mapper.done() {
                    let stderr = match c.child.take() {
                        Some(ch) => ch
                            .wait_with_output()
                            .await
                            .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_owned())
                            .unwrap_or_default(),
                        None => String::new(),
                    };
                    // хвост stderr, не целиком (бывают простыни логов)
                    let mut start = stderr.len().saturating_sub(500);
                    while !stderr.is_char_boundary(start) {
                        start += 1;
                    }
                    let tail = &stderr[start..];
                    c.queue.push_back(Err(ProviderError::Protocol(format!(
                        "{} exited without a result: {}",
                        c.bin,
                        if tail.is_empty() { "(no stderr)" } else { tail }
                    ))));
                }
            }
        }
    }))
}

/// Fold usage как в anthropic.rs: `input_tokens` включает кэш (инвариант Usage).
fn anthropic_usage(u: &Value) -> Usage {
    let cache_read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
    let cache_write = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    Usage {
        input_tokens: u["input_tokens"].as_u64().unwrap_or(0) + cache_read + cache_write,
        output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        cost_usd: None,
    }
}

/// Краткий аргумент тул-колла для маркера: первое осмысленное строковое поле
/// input (команда/путь/паттерн), одной строкой, до 80 символов.
fn tool_arg(input: &Value) -> String {
    const KEYS: &[&str] = &[
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "description",
        "prompt",
    ];
    let s = KEYS
        .iter()
        .find_map(|k| input[*k].as_str())
        .unwrap_or_default();
    let line = s.lines().next().unwrap_or_default();
    let mut out: String = line.chars().take(80).collect();
    if line.chars().count() > 80 {
        out.push('…');
    }
    out
}

/// stream-json `claude -p` (и его клон у `grok -p`): system/init →
/// `session_id`; assistant → текстовые блоки (`tool_use` — строкой-маркером,
/// исполняет их сам CLI); result → usage.
struct ClaudeMapper {
    bin: &'static str,
    session: Arc<Mutex<Option<String>>>,
    done: bool,
    emitted_text: bool,
}

impl ClaudeMapper {
    fn new(bin: &'static str, session: Arc<Mutex<Option<String>>>) -> Self {
        Self {
            bin,
            session,
            done: false,
            emitted_text: false,
        }
    }
}

impl LineMapper for ClaudeMapper {
    fn map(&mut self, line: &str) -> Result<Vec<LlmEvent>, ProviderError> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return Ok(vec![]); // не-JSON шум в stdout
        };
        if let Some(sid) = v["session_id"].as_str()
            && let Ok(mut s) = self.session.lock()
        {
            *s = Some(sid.to_owned());
        }
        let mut out = Vec::new();
        match v["type"].as_str() {
            Some("assistant") => {
                for block in v["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => {
                            let t = block["text"].as_str().unwrap_or_default();
                            if !t.is_empty() {
                                if self.emitted_text {
                                    out.push(LlmEvent::TextDelta("\n\n".to_owned()));
                                }
                                out.push(LlmEvent::TextDelta(t.to_owned()));
                                self.emitted_text = true;
                            }
                        }
                        Some("tool_use") => {
                            let name = block["name"].as_str().unwrap_or("?");
                            let arg = tool_arg(&block["input"]);
                            out.push(LlmEvent::TextDelta(format!(
                                "{}⏺ {name}{}{arg}",
                                if self.emitted_text { "\n\n" } else { "" },
                                if arg.is_empty() { "" } else { " · " },
                            )));
                            self.emitted_text = true;
                        }
                        _ => {} // thinking и прочее не показываем
                    }
                }
            }
            Some("result") => {
                self.done = true;
                if v["is_error"].as_bool() == Some(true) {
                    let msg = v["result"]
                        .as_str()
                        .or_else(|| v["subtype"].as_str())
                        .unwrap_or("unknown error");
                    return Err(ProviderError::Protocol(format!("{}: {msg}", self.bin)));
                }
                out.push(LlmEvent::Finished {
                    reason: match v["stop_reason"].as_str() {
                        Some("max_tokens") => StopReason::MaxTokens,
                        _ => StopReason::Stop,
                    },
                    usage: Usage {
                        // цена от самого CLI (result.total_cost_usd) — по
                        // подписке денег не списывает, но это API-эквивалент
                        cost_usd: v["total_cost_usd"].as_f64(),
                        ..anthropic_usage(&v["usage"])
                    },
                });
            }
            _ => {} // system/user/rate_limit_event — служебные
        }
        Ok(out)
    }

    fn done(&self) -> bool {
        self.done
    }
}

/// `codex exec --json`: thread.started → session; item.* → текст/маркеры
/// команд; turn.completed → usage; error / turn.failed → ошибка.
struct CodexMapper {
    session: Arc<Mutex<Option<String>>>,
    done: bool,
    emitted_text: bool,
}

impl CodexMapper {
    fn new(session: Arc<Mutex<Option<String>>>) -> Self {
        Self {
            session,
            done: false,
            emitted_text: false,
        }
    }

    fn sep(&mut self) -> &'static str {
        if std::mem::replace(&mut self.emitted_text, true) {
            "\n\n"
        } else {
            ""
        }
    }
}

impl LineMapper for CodexMapper {
    fn map(&mut self, line: &str) -> Result<Vec<LlmEvent>, ProviderError> {
        if self.done {
            return Ok(vec![]); // после turn.failed codex дублирует error
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        match v["type"].as_str() {
            Some("thread.started") => {
                if let Some(id) = v["thread_id"].as_str()
                    && let Ok(mut s) = self.session.lock()
                {
                    *s = Some(id.to_owned());
                }
            }
            Some("item.started" | "item.completed") => {
                let item = &v["item"];
                let item_type = item["item_type"].as_str().or_else(|| item["type"].as_str());
                match (v["type"].as_str(), item_type) {
                    (Some("item.completed"), Some("agent_message")) => {
                        let t = item["text"].as_str().unwrap_or_default();
                        if !t.is_empty() {
                            let sep = self.sep();
                            out.push(LlmEvent::TextDelta(format!("{sep}{t}")));
                        }
                    }
                    (Some("item.started"), Some("command_execution")) => {
                        let c = item["command"].as_str().unwrap_or("?");
                        let sep = self.sep();
                        out.push(LlmEvent::TextDelta(format!("{sep}⏺ {c}")));
                    }
                    _ => {}
                }
            }
            Some("turn.completed") => {
                self.done = true;
                let u = &v["usage"];
                out.push(LlmEvent::Finished {
                    reason: StopReason::Stop,
                    usage: Usage {
                        // у OpenAI input_tokens уже включает кэшированные
                        input_tokens: u["input_tokens"].as_u64().unwrap_or(0),
                        output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                        cache_read_tokens: u["cached_input_tokens"].as_u64().unwrap_or(0),
                        cache_write_tokens: 0,
                        cost_usd: None,
                    },
                });
            }
            Some("turn.failed" | "error") => {
                self.done = true;
                let msg = v["error"]["message"]
                    .as_str()
                    .or_else(|| v["message"].as_str())
                    .unwrap_or("unknown error");
                return Err(ProviderError::Protocol(format!("codex: {msg}")));
            }
            _ => {}
        }
        Ok(out)
    }

    fn done(&self) -> bool {
        self.done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> ClaudeMapper {
        ClaudeMapper::new("claude", Arc::default())
    }

    fn codex() -> CodexMapper {
        CodexMapper::new(Arc::default())
    }

    fn sid(s: &Arc<Mutex<Option<String>>>) -> Option<String> {
        s.lock().ok().and_then(|g| g.clone())
    }

    #[test]
    fn claude_maps_text_result_and_session() {
        let mut m = claude();
        let s = Arc::clone(&m.session);
        assert!(
            m.map(r#"{"type":"system","subtype":"init","session_id":"s-1"}"#)
                .is_ok_and(|v| v.is_empty())
        );
        assert_eq!(sid(&s).as_deref(), Some("s-1"));
        let out = m
            .map(r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"x"},{"type":"text","text":"hi"}]}}"#)
            .unwrap_or_default();
        assert_eq!(out, vec![LlmEvent::TextDelta("hi".to_owned())]);
        // tool_use → маркер с кратким аргументом, не ToolCallStart (исполняет CLI, не makedo)
        let out = m
            .map(
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls -la\npwd"}}]}}"#,
            )
            .unwrap_or_default();
        assert_eq!(
            out,
            vec![LlmEvent::TextDelta("\n\n⏺ Bash · ls -la".to_owned())]
        );
        let out = m
            .map(r#"{"type":"result","is_error":false,"stop_reason":"end_turn","session_id":"s-1","usage":{"input_tokens":10,"cache_read_input_tokens":100,"cache_creation_input_tokens":5,"output_tokens":7}}"#)
            .unwrap_or_default();
        assert!(m.done());
        assert_eq!(
            out,
            vec![LlmEvent::Finished {
                reason: StopReason::Stop,
                usage: Usage {
                    input_tokens: 115,
                    output_tokens: 7,
                    cache_read_tokens: 100,
                    cache_write_tokens: 5,
                    cost_usd: None,
                }
            }]
        );
    }

    #[test]
    fn claude_error_result_is_error() {
        let mut m = claude();
        assert!(
            m.map(r#"{"type":"result","is_error":true,"result":"quota"}"#)
                .is_err()
        );
        assert!(m.done());
    }

    #[test]
    fn codex_maps_thread_message_usage_and_error() {
        let mut m = codex();
        let s = Arc::clone(&m.session);
        assert!(
            m.map(r#"{"type":"thread.started","thread_id":"t-1"}"#)
                .is_ok_and(|v| v.is_empty())
        );
        assert_eq!(sid(&s).as_deref(), Some("t-1"));
        let out = m
            .map(r#"{"type":"item.completed","item":{"item_type":"agent_message","text":"ok"}}"#)
            .unwrap_or_default();
        assert_eq!(out, vec![LlmEvent::TextDelta("ok".to_owned())]);
        let out = m
            .map(r#"{"type":"turn.completed","usage":{"input_tokens":50,"cached_input_tokens":40,"output_tokens":3}}"#)
            .unwrap_or_default();
        assert!(m.done());
        assert!(matches!(
            out[0],
            LlmEvent::Finished {
                reason: StopReason::Stop,
                usage: Usage {
                    input_tokens: 50,
                    cache_read_tokens: 40,
                    output_tokens: 3,
                    ..
                }
            }
        ));

        let mut m = codex();
        assert!(
            m.map(r#"{"type":"turn.failed","error":{"message":"limit"}}"#)
                .is_err()
        );
        // повторный error после turn.failed молча глотается
        assert!(
            m.map(r#"{"type":"error","message":"limit"}"#)
                .is_ok_and(|v| v.is_empty())
        );
    }

    #[test]
    fn prompt_resume_takes_last_user_only() {
        let msgs = vec![
            Message::User {
                content: "first".to_owned(),
            },
            Message::Assistant {
                content: vec![crate::AssistantPart::Text {
                    text: "reply".to_owned(),
                }],
            },
            Message::User {
                content: "second".to_owned(),
            },
        ];
        assert_eq!(build_prompt(&msgs, true), "second");
        // fresh: плоская история с прежними ответами
        let flat = build_prompt(&msgs, false);
        assert!(flat.starts_with("first"));
        assert!(flat.contains("[previous assistant reply]\nreply"));
        assert!(flat.ends_with("second"));
    }
}
