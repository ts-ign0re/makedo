//! События сессии для TUI. Живут в types: TUI не знает про providers/mcp напрямую
//! (PLAN §2). `#[non_exhaustive]` — события будут расти.

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SessionEvent {
    PermissionsChanged {
        allow_all: bool,
    },
    /// Runtime activity, independent of the model-authored task plan.
    PhaseChanged(String),
    AgentStarted {
        id: String,
        name: String,
        worktree: Option<String>,
    },
    AgentEvent {
        id: String,
        event: Box<SessionEvent>,
    },
    ToolQueued {
        call_id: String,
        name: String,
    },
    /// Durable inbox receipt, visible immediately while the current step runs.
    PromptQueued {
        id: String,
        text: String,
    },
    /// Accepted at a safe model-step boundary. Update the queued bubble in place.
    PromptAccepted {
        text: String,
        queued_id: Option<String>,
    },
    PromptCancelled {
        id: String,
    },
    HistoryMessage {
        user: bool,
        text: String,
    },
    /// Подтверждённые модель и настройки reasoning, включая начальное состояние.
    ModelChanged {
        model: String,
        effort_levels: Vec<String>,
        effort: Option<String>,
    },
    /// Дельта текста ответа (append-only).
    TextDelta(String),
    ToolLog {
        call_id: String,
        name: String,
        path: String,
    },
    ToolStarted {
        call_id: String,
        name: String,
    },
    ToolFinished {
        call_id: String,
        name: String,
        is_error: bool,
        /// Короткое превью вывода для UI.
        preview: String,
    },
    /// Запрос разрешения; ответ — командой `PermissionReply`.
    PermissionAsk {
        request_id: u64,
        description: String,
    },
    TurnFinished {
        cancelled: bool,
        usage: TurnUsage,
    },
    /// Модель обновила план сессии (тул `todo_write`) — полная замена списка.
    TodoUpdated(Vec<TodoEntry>),
    /// Информационное уведомление (успех) — зелёным в TUI.
    Notice(String),
    Error(String),
}

/// Пункт плана сессии для inline-панели TUI.
#[derive(Debug, Clone)]
pub struct TodoEntry {
    pub content: String,
    pub status: TodoState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoState {
    Pending,
    InProgress,
    Completed,
}

/// Суммарный usage турна (все LLM-шаги). Cache-поля — ядро экономики (PLAN M3.5).
#[derive(Debug, Clone, Copy, Default)]
pub struct TurnUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Реальная стоимость турна в USD (по ценам каталога); None = цены неизвестны.
    pub cost_usd: Option<f64>,
    /// Сэкономлено кэшем: `cache_read × (цена input − цена cache read)`.
    pub saved_usd: Option<f64>,
}

impl TurnUsage {
    /// Доля input-токенов, пришедших из кэша (0..=100).
    /// `input_tokens` уже включает кэшированные (инвариант провайдеров makedo).
    #[must_use]
    pub fn cache_hit_percent(&self) -> u64 {
        if self.input_tokens == 0 {
            return 0;
        }
        self.cache_read_tokens * 100 / self.input_tokens
    }
}
