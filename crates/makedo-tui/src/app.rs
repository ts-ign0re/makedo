//! Состояние TUI: чистый reducer над `SessionEvent` — тестируется без терминала.

use makedo_types::event::{SessionEvent, TodoEntry};

use crate::spinner::ProcessKind;

#[derive(Debug, Clone)]
pub enum Entry {
    User(String),
    QueuedUser {
        id: String,
        text: String,
        cancelled: bool,
    },
    Assistant(String),
    Tool {
        call_id: String,
        name: String,
        status: ToolStatus,
        preview: String,
    },
    Notice(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Queued,
    Running,
    Ok,
    Failed,
}

#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)] // busy/should_quit/allow_all/unfocused — плоские флаги
pub struct App {
    pub agents: Vec<crate::pipeline::Agent>,
    pub agent_selected: Option<usize>,
    pub agents_hidden: bool,
    pub phase: String,
    pub turn_started: Option<std::time::Instant>,
    pub turn_failed: bool,
    pub log: Vec<Entry>,
    pub tool_logs: Vec<(String, String, String)>,
    /// Текст поля ввода (может быть многострочным — `\n` от Shift+Enter/вставки).
    pub input: String,
    /// Позиция курсора в `input` — индекс СИМВОЛА (0..=len). Для readline-правки.
    pub cursor: usize,
    /// Active streamed reply, which may precede newly queued user bubbles.
    pub assistant_stream: Option<usize>,
    pub busy: bool,
    pub running_tools: usize,
    /// Активный permission-запрос: (id, описание).
    pub permission: Option<(u64, String)>,
    pub permission_input: String,
    pub should_quit: bool,
    /// Usage последнего турна (для статус-строки: cache-hit = деньги).
    pub last_usage: Option<makedo_types::event::TurnUsage>,
    /// План сессии (ведёт модель через `todo_write`).
    pub todos: Vec<TodoEntry>,
    /// Палитра команд (активна, когда ввод начинается с `/`).
    pub palette: crate::commands::Palette,
    /// Модальный выбор модели (`/models`).
    pub modal: Modal,
    /// Стабильный список провайдеров на время выбора модели.
    pub providers: Vec<crate::commands::ProviderChoice<String>>,
    /// Скролл вверх от низа (число экранных строк). 0 = прижато к низу (авто).
    pub scroll: usize,
    /// Шапка: ветка git + путь (заполняется при старте).
    pub header: String,
    /// Локально установленные модели Ollama (пинг при старте / открытии `/models`).
    /// Пусто = демон не запущен. Даёт увидеть, что подключено, без «connect».
    pub ollama: Vec<String>,
    /// Первый запуск (не было модели): после выбора провайдера — онбординг.
    pub first_run: bool,
    /// Текущая модель (для метки в правом-нижнем углу инпута, как в grok).
    pub model: String,
    pub effort_levels: Vec<String>,
    pub effort: Option<String>,
    /// Текущая доктрина проектирования (`none`/`ddd`/`hexagonal`) — для `/doctrine`.
    pub doctrine: String,
    /// Режим авто-одобрения (--yolo / /allow-all) — показываем в метке инпута.
    pub allow_all: bool,
    /// Окно терминала НЕ в фокусе (default = false = в фокусе) — на нём гасим тики
    /// спиннера, чтобы не жечь CPU/батарею фоновой вкладки.
    pub unfocused: bool,
    /// Хост — наше окно: курсор рисуем сами инверсией клетки, значок ветки — powerline.
    pub own_window: bool,
}

/// Многошаговый выбор модели: провайдер → модель → ключ.
#[derive(Debug, Default)]
pub enum Modal {
    #[default]
    None,
    /// Выбор провайдера.
    Providers {
        sel: usize,
    },
    /// Выбор модели у провайдера (индекс в `App::providers`).
    Models {
        provider: usize,
        models: Vec<String>,
        sel: usize,
    },
    /// Ввод API-ключа (скрытый).
    ApiKey {
        provider: usize,
        model: String,
        buf: String,
    },
    /// Выбор темы с живым предпросмотром: стрелки применяют тему сразу, Esc
    /// откатывает к `original`, Enter сохраняет. `sel`/`original` — индексы в
    /// `theme::NAMES`.
    Themes {
        sel: usize,
        original: usize,
    },
    /// Выбор доктрины проектирования (`Doctrine::choices()`). Enter сохраняет и
    /// применяет на лету, Esc отменяет.
    Doctrine {
        sel: usize,
    },
    Effort {
        sel: usize,
    },
    /// Онбординг первого запуска после выбора модели: шаг 0 — прогреть wiki,
    /// шаг 1 — включить verify gate (`cmds` — автодетект по файлам проекта).
    Onboard {
        step: usize,
        cmds: Vec<String>,
    },
}

impl Modal {
    #[must_use]
    pub fn is_open(&self) -> bool {
        !matches!(self, Modal::None)
    }
}

impl App {
    /// Палитра команд открыта: ввод — одиночный `/`-токен без пробела.
    #[must_use]
    pub fn palette_open(&self) -> bool {
        self.input.starts_with('/') && !self.input.contains(' ')
    }

    /// Текущий фильтр палитры (часть после `/`).
    #[must_use]
    pub fn palette_filter(&self) -> &str {
        self.input.strip_prefix('/').unwrap_or("")
    }
}

impl App {
    // ---- редактирование поля ввода (readline-минимум, char-индексы) ----

    /// Байтовое смещение символа `char_idx` (или конец строки).
    fn byte_of(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map_or(self.input.len(), |(b, _)| b)
    }

    fn char_len(&self) -> usize {
        self.input.chars().count()
    }

    pub fn insert_str(&mut self, s: &str) {
        let at = self.byte_of(self.cursor);
        self.input.insert_str(at, s);
        self.cursor += s.chars().count();
    }

    pub fn insert_char(&mut self, c: char) {
        let at = self.byte_of(self.cursor);
        self.input.insert(at, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let (a, b) = (self.byte_of(self.cursor - 1), self.byte_of(self.cursor));
        self.input.replace_range(a..b, "");
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.char_len() {
            return;
        }
        let (a, b) = (self.byte_of(self.cursor), self.byte_of(self.cursor + 1));
        self.input.replace_range(a..b, "");
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.char_len() {
            self.cursor += 1;
        }
    }

    /// Начало текущей строки (после предыдущего `\n` или 0).
    fn line_start(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1] != '\n' {
            i -= 1;
        }
        i
    }

    /// Конец текущей строки (до следующего `\n` или конец).
    fn line_end(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        while i < chars.len() && chars[i] != '\n' {
            i += 1;
        }
        i
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    /// Ctrl+K — убить от курсора до конца строки.
    pub fn kill_to_line_end(&mut self) {
        let e = self.line_end();
        let (a, b) = (self.byte_of(self.cursor), self.byte_of(e));
        self.input.replace_range(a..b, "");
    }

    /// Ctrl+U — убить от начала строки до курсора.
    pub fn kill_to_line_start(&mut self) {
        let s = self.line_start();
        let (a, b) = (self.byte_of(s), self.byte_of(self.cursor));
        self.input.replace_range(a..b, "");
        self.cursor = s;
    }

    /// Индекс начала слова слева от курсора (пропустив пробелы, затем слово).
    fn word_left_idx(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    /// Индекс конца слова справа от курсора.
    fn word_right_idx(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_left_idx();
    }

    pub fn word_right(&mut self) {
        self.cursor = self.word_right_idx();
    }

    /// Ctrl+W — удалить слово слева от курсора.
    pub fn delete_word_back(&mut self) {
        let s = self.word_left_idx();
        let (a, b) = (self.byte_of(s), self.byte_of(self.cursor));
        self.input.replace_range(a..b, "");
        self.cursor = s;
    }

    pub fn newline(&mut self) {
        self.insert_char('\n');
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    /// Заголовок для таба терминала: задача (последний вопрос) + маркер работы,
    /// чтобы среди вкладок было видно, где сессия занята (как в opencode).
    #[must_use]
    pub fn title(&self) -> String {
        let task = self.log.iter().rev().find_map(|e| match e {
            Entry::User(t) => {
                let first = t.lines().next().unwrap_or(t).trim();
                (!first.is_empty()).then(|| {
                    if first.chars().count() > 40 {
                        let mut s: String = first.chars().take(39).collect();
                        s.push('…');
                        s
                    } else {
                        first.to_owned()
                    }
                })
            }
            _ => None,
        });
        match (self.busy, task) {
            (true, Some(t)) => format!("● makedo · {t}"), // занята
            (false, Some(t)) => format!("makedo · {t}"),  // ждёт
            (true, None) => "● makedo".to_owned(),
            (false, None) => "makedo".to_owned(),
        }
    }

    pub fn user_prompt(&mut self, text: String) {
        self.prompt_started();
        self.log.push(Entry::User(text));
    }

    fn prompt_started(&mut self) {
        self.assistant_stream = None;
        self.permission = None;
        self.permission_input.clear();
        if !self.busy {
            self.turn_started = Some(std::time::Instant::now());
            self.turn_failed = false;
        }
        // предыдущий план полностью закрыт → прячем его (не висит над инпутом)
        if !self.todos.is_empty()
            && self
                .todos
                .iter()
                .all(|t| t.status == makedo_types::event::TodoState::Completed)
        {
            self.todos.clear();
        }
        self.busy = true;
        self.scroll = 0; // новый вопрос — прыгаем к низу
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_add(n);
    }

    pub fn scroll_down(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_sub(n);
    }

    /// Текущий вид процесса для выбора спиннера.
    #[must_use]
    pub fn process_kind(&self) -> ProcessKind {
        if !self.busy {
            ProcessKind::Idle
        } else if self.running_tools > 1 {
            ProcessKind::ToolBatch
        } else if self.running_tools == 1 {
            ProcessKind::ToolRun
        } else {
            ProcessKind::LlmStream
        }
    }

    #[allow(clippy::too_many_lines)] // Flat event reducer keeps UI state transitions together.
    pub fn apply(&mut self, ev: SessionEvent) {
        match ev {
            SessionEvent::PermissionsChanged { allow_all } => {
                self.allow_all = allow_all;
                if allow_all {
                    self.permission = None;
                    self.permission_input.clear();
                }
            }
            SessionEvent::PhaseChanged(phase) => {
                if phase == "Model" {
                    self.assistant_stream = None;
                }
                self.turn_started
                    .get_or_insert_with(std::time::Instant::now);
                self.phase = phase;
            }
            SessionEvent::AgentStarted { id, name, worktree } => {
                self.agents
                    .push(crate::pipeline::Agent::new(id, name, worktree));
            }
            SessionEvent::AgentEvent { id, event } => {
                if let SessionEvent::ToolLog {
                    call_id,
                    name,
                    path,
                } = event.as_ref()
                {
                    self.tool_logs
                        .push((format!("{id}/{call_id}"), name.clone(), path.clone()));
                }
                if let Some(agent) = self.agents.iter_mut().find(|agent| agent.id == id) {
                    agent.apply(*event);
                }
            }
            SessionEvent::ToolQueued { call_id, name } => self.log.push(Entry::Tool {
                call_id,
                name,
                status: ToolStatus::Queued,
                preview: "queued".into(),
            }),
            SessionEvent::ToolLog {
                call_id,
                name,
                path,
            } => self.tool_logs.push((call_id, name, path)),
            SessionEvent::PromptQueued { id, text } => {
                self.log.push(Entry::QueuedUser {
                    id,
                    text,
                    cancelled: false,
                });
                self.scroll = 0;
            }
            SessionEvent::PromptAccepted { text, queued_id } => {
                let queued = queued_id.and_then(|id| {
                    self.log.iter().position(
                        |e| matches!(e, Entry::QueuedUser { id: queued, .. } if *queued == id),
                    )
                });
                if let Some(index) = queued {
                    self.log[index] = Entry::User(text);
                    self.prompt_started();
                } else {
                    self.user_prompt(text);
                }
            }
            SessionEvent::PromptCancelled { id } => {
                if let Some(Entry::QueuedUser { cancelled, .. }) = self
                    .log
                    .iter_mut()
                    .find(|e| matches!(e, Entry::QueuedUser { id: queued, .. } if *queued == id))
                {
                    *cancelled = true;
                }
            }
            SessionEvent::HistoryMessage { user, text } => self.log.push(if user {
                Entry::User(text)
            } else {
                Entry::Assistant(text)
            }),
            SessionEvent::ModelChanged {
                model,
                effort_levels,
                effort,
            } => {
                self.model = model;
                self.effort_levels = effort_levels;
                self.effort = effort;
                if matches!(self.modal, Modal::Effort { .. }) {
                    self.modal = Modal::None;
                }
            }
            SessionEvent::TextDelta(t) => {
                if let Some(Entry::Assistant(buf)) =
                    self.assistant_stream.and_then(|i| self.log.get_mut(i))
                {
                    buf.push_str(&t);
                } else {
                    self.assistant_stream = Some(self.log.len());
                    self.log.push(Entry::Assistant(t));
                }
            }
            SessionEvent::ToolStarted { call_id, name } => {
                self.turn_started
                    .get_or_insert_with(std::time::Instant::now);
                self.running_tools += 1;
                self.phase.clone_from(&name);
                let entry = Entry::Tool {
                    call_id,
                    name,
                    status: ToolStatus::Running,
                    preview: String::new(),
                };
                if let Some(old) = self.log.iter_mut().rev().find(|old| matches!((&**old, &entry), (Entry::Tool { call_id: a, status: ToolStatus::Queued, .. }, Entry::Tool { call_id: b, .. }) if a == b)) { *old = entry; }
                else { self.log.push(entry); }
            }
            SessionEvent::ToolFinished {
                call_id,
                name,
                is_error,
                preview,
                ..
            } => {
                if self.log.iter().any(|e| matches!(e, Entry::Tool { call_id: id, status: ToolStatus::Running, .. } if *id == call_id)) {
                    self.running_tools = self.running_tools.saturating_sub(1);
                }
                if let Some(agent) = self.agents.iter_mut().find(|agent| agent.id == call_id) {
                    agent.finish(if is_error && preview.contains("cancelled") {
                        crate::pipeline::State::Cancelled
                    } else if is_error {
                        crate::pipeline::State::Failed
                    } else {
                        crate::pipeline::State::Done
                    });
                    if is_error && agent.error.is_none() {
                        agent.error = Some(preview.clone());
                    }
                }
                let status = if is_error {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Ok
                };
                // Match the exact invocation, including concurrent tools with the same name.
                if let Some(e) = self
                    .log
                    .iter_mut()
                    .rev()
                    .find(|e| matches!(e, Entry::Tool { call_id: id, .. } if *id == call_id))
                {
                    *e = Entry::Tool {
                        call_id,
                        name,
                        status,
                        preview,
                    };
                } else {
                    self.log.push(Entry::Tool {
                        call_id,
                        name,
                        status,
                        preview,
                    });
                }
            }
            SessionEvent::PermissionAsk {
                request_id,
                description,
            } => {
                self.permission_input.clear();
                self.permission = Some((request_id, description));
            }
            SessionEvent::TurnFinished { cancelled, usage } => {
                self.assistant_stream = None;
                self.turn_started = None;
                for agent in &mut self.agents {
                    if agent.state == crate::pipeline::State::Running {
                        agent.finish(if cancelled {
                            crate::pipeline::State::Cancelled
                        } else {
                            crate::pipeline::State::Failed
                        });
                    }
                }
                self.busy = false;
                self.running_tools = 0;
                self.permission = None;
                self.last_usage = Some(usage);
                if cancelled {
                    self.turn_failed = true;
                    self.log.push(Entry::Error("(cancelled)".to_owned()));
                }
            }
            SessionEvent::TodoUpdated(todos) => self.todos = todos,
            SessionEvent::Notice(m) => self.log.push(Entry::Notice(m)),
            SessionEvent::Error(e) => {
                self.turn_failed = true;
                self.log.push(Entry::Error(e));
            }
            _ => {} // non_exhaustive: будущие события не ломают рендер
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reducer_tracks_stream_and_tools() {
        let mut app = App::default();
        app.user_prompt("hi".into());
        assert!(app.busy);
        assert_eq!(app.process_kind(), ProcessKind::LlmStream);

        app.apply(SessionEvent::TextDelta("he".into()));
        app.apply(SessionEvent::TextDelta("llo".into()));
        assert!(matches!(app.log.last(), Some(Entry::Assistant(t)) if t == "hello"));

        app.apply(SessionEvent::ToolStarted {
            call_id: "c1".into(),
            name: "grep".into(),
        });
        assert_eq!(app.process_kind(), ProcessKind::ToolRun);
        app.apply(SessionEvent::ToolStarted {
            call_id: "c2".into(),
            name: "read".into(),
        });
        assert_eq!(app.process_kind(), ProcessKind::ToolBatch);
        app.apply(SessionEvent::ToolFinished {
            call_id: "c1".into(),
            name: "grep".into(),
            is_error: false,
            preview: "3 matches".into(),
        });
        assert!(app.log.iter().any(|e| matches!(
            e,
            Entry::Tool { name, status: ToolStatus::Ok, preview, .. } if name == "grep" && preview == "3 matches"
        )));

        app.apply(SessionEvent::TurnFinished {
            cancelled: false,
            usage: makedo_types::event::TurnUsage::default(),
        });
        assert!(!app.busy);
        assert_eq!(app.process_kind(), ProcessKind::Idle);
    }

    #[test]
    fn title_reflects_task_and_activity() {
        let mut app = App::default();
        assert_eq!(app.title(), "makedo");
        app.user_prompt("сделай аудит движка покера".into());
        assert_eq!(app.title(), "● makedo · сделай аудит движка покера"); // busy
        app.apply(SessionEvent::TurnFinished {
            cancelled: false,
            usage: makedo_types::event::TurnUsage::default(),
        });
        assert_eq!(app.title(), "makedo · сделай аудит движка покера"); // idle
        // длинный вопрос усекается
        app.user_prompt("a".repeat(80));
        assert!(app.title().chars().count() < 60 && app.title().ends_with('…'));
    }

    #[test]
    fn input_editing_readline_and_multiline() {
        let mut a = App::default();
        for c in "hello".chars() {
            a.insert_char(c);
        }
        assert_eq!((a.input.as_str(), a.cursor), ("hello", 5));
        a.move_left();
        a.move_left(); // hel|lo
        a.insert_char('X'); // helX|lo
        assert_eq!((a.input.as_str(), a.cursor), ("helXlo", 4));
        a.home();
        assert_eq!(a.cursor, 0);
        a.end();
        assert_eq!(a.cursor, 6);
        a.backspace();
        assert_eq!(a.input, "helXl");
        a.delete_word_back();
        assert_eq!(a.input, "");
        // многострочный: home/end относительно строки, Ctrl+U по строке
        a.clear_input();
        a.insert_str("ab\ncd"); // cursor в конце
        assert_eq!(a.cursor, 5);
        a.home();
        assert_eq!(a.cursor, 3); // начало строки "cd"
        a.end();
        assert_eq!(a.cursor, 5); // конец строки "cd"
        a.kill_to_line_start();
        assert_eq!(a.input, "ab\n"); // убил "cd"
        // вставка идёт в позицию курсора
        a.clear_input();
        a.insert_char('x');
        a.move_left();
        a.insert_str("YZ");
        assert_eq!((a.input.as_str(), a.cursor), ("YZx", 2));
        // Ctrl+K — до конца строки
        a.clear_input();
        a.insert_str("hello");
        a.home();
        a.move_right();
        a.move_right();
        a.kill_to_line_end();
        assert_eq!(a.input, "he");
    }

    #[test]
    fn permission_prompt_lifecycle() {
        let mut app = App::default();
        app.user_prompt("x".into());
        app.apply(SessionEvent::PermissionAsk {
            request_id: 7,
            description: "bash rm -rf".into(),
        });
        assert_eq!(app.permission.as_ref().map(|p| p.0), Some(7));
        app.apply(SessionEvent::TurnFinished {
            cancelled: true,
            usage: makedo_types::event::TurnUsage::default(),
        });
        assert!(app.permission.is_none());
    }
}
