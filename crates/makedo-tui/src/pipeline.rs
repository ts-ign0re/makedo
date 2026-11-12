//! UI-only agent progress. Plans come from `todo_write`; activity comes from runtime events.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use makedo_types::event::{SessionEvent, TodoEntry, TodoState};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::{app::App, theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub worktree: Option<String>,
    pub started: Instant,
    pub finished: Option<Instant>,
    pub state: State,
    pub todos: Vec<TodoEntry>,
    pub activity: String,
    pub tools: BTreeMap<String, String>,
    pub log: Option<String>,
    pub error: Option<String>,
}

impl Agent {
    pub fn new(id: String, name: String, worktree: Option<String>) -> Self {
        Self {
            id,
            name,
            worktree,
            started: Instant::now(),
            finished: None,
            state: State::Running,
            todos: Vec::new(),
            activity: "Starting".into(),
            tools: BTreeMap::new(),
            log: None,
            error: None,
        }
    }

    pub fn apply(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::PhaseChanged(phase) => self.activity = phase,
            SessionEvent::TodoUpdated(todos) => self.todos = todos,
            SessionEvent::ToolStarted { call_id, name } => {
                self.tools.insert(call_id, name);
            }
            SessionEvent::ToolFinished { call_id, .. } => {
                self.tools.remove(&call_id);
            }
            SessionEvent::ToolLog { path, .. } => self.log = Some(path),
            SessionEvent::Error(error) => self.error = Some(error),
            SessionEvent::TurnFinished { cancelled, .. } => {
                self.tools.clear();
                self.activity = if cancelled { "Stopping" } else { "Finishing" }.into();
            }
            _ => {}
        }
    }

    pub fn finish(&mut self, state: State) {
        self.state = state;
        self.finished.get_or_insert_with(Instant::now);
        self.tools.clear();
    }

    pub fn activity(&self) -> String {
        if self.tools.is_empty() {
            return self.activity.clone();
        }
        self.tools.values().cloned().collect::<Vec<_>>().join(" · ")
    }
}

pub fn duration(elapsed: Duration) -> String {
    format!(
        "{:02}:{:02}",
        elapsed.as_secs() / 60,
        elapsed.as_secs() % 60
    )
}

/// Fit terminal cells, including wide Unicode; never emit control characters.
pub fn clip(text: &str, width: usize) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    if Span::raw(&clean).width() <= width {
        return clean;
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for c in clean.chars() {
        if Span::raw(&out).width() + Span::raw(c.to_string()).width() >= width {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

pub fn steps(todos: &[TodoEntry], width: usize, tick: u64, state: State) -> Line<'static> {
    if todos.is_empty() {
        return Line::default();
    }
    let active = todos
        .iter()
        .position(|t| t.status == TodoState::InProgress)
        .or_else(|| todos.iter().position(|t| t.status == TodoState::Pending))
        .unwrap_or(0);
    // ponytail: show a window around the current step; no horizontal scrolling state.
    let total: usize = todos
        .iter()
        .map(|t| Span::raw(&t.content).width().min(18) + 5)
        .sum();
    let start = if total > width { active } else { 0 };
    let mut spans = Vec::new();
    let mut remaining = width;
    if start > 0 && remaining > 3 {
        spans.push(Span::raw("… "));
        remaining -= 2;
    }
    for (offset, item) in todos.iter().skip(start).enumerate() {
        let suffix = if start + offset + 1 < todos.len() {
            2
        } else {
            0
        };
        let separator = if offset == 0 { "" } else { " → " };
        if remaining < Span::raw(separator).width() + 5 {
            break;
        }
        if !separator.is_empty() {
            spans.push(Span::styled(separator, Style::default().fg(theme::dim())));
            remaining = remaining.saturating_sub(3);
        }
        let (mark, color) = match item.status {
            TodoState::Completed => ("✓", theme::dim()),
            TodoState::Pending => ("○", theme::dim()),
            TodoState::InProgress => match state {
                State::Running => (
                    // Circular frames stay centered; top-heavy braille floats above labels.
                    match tick % 4 {
                        0 => "◐",
                        1 => "◓",
                        2 => "◑",
                        _ => "◒",
                    },
                    theme::cyan(),
                ),
                State::Failed => ("✗", theme::red()),
                State::Cancelled => ("■", theme::yellow()),
                State::Done => ("○", theme::dim()), // completion does not invent completed plan items
            },
        };
        let label = clip(&item.content, remaining.saturating_sub(2 + suffix).min(18));
        let text = format!("{mark} {label}");
        remaining = remaining.saturating_sub(Span::raw(&text).width());
        spans.push(Span::styled(text, Style::default().fg(color)));
        if remaining < 8 && start + offset + 1 < todos.len() {
            if remaining >= 2 {
                spans.push(Span::styled(" …", Style::default().fg(theme::dim())));
            }
            break;
        }
    }
    Line::from(spans)
}

pub fn sidebar(app: &App, area: Rect) -> bool {
    !app.agents.is_empty() && !app.agents_hidden && area.width >= 110 && area.height >= 12
}

#[allow(clippy::too_many_lines)] // Linear panel layout: list, selected details, key hints.
pub fn draw(f: &mut ratatui::Frame<'_>, app: &App, area: Rect, tick: u64) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(theme::border()));
    let mut inner = block.inner(area);
    f.render_widget(block, area);
    inner.x += 2;
    inner.width = inner.width.saturating_sub(2);
    let mut rows = vec![
        Line::styled("AGENTS", Style::default().fg(theme::dim())),
        Line::default(),
    ];
    let width = usize::from(inner.width);
    let focus = app.agent_selected.unwrap_or_else(|| {
        app.agents
            .iter()
            .position(|a| a.state == State::Running)
            .unwrap_or(0)
    });
    let detail_reserve = if app.agent_selected.is_some() { 6 } else { 0 };
    let list_limit = usize::from(inner.height.saturating_sub(1 + detail_reserve));
    let capacity = usize::from(inner.height.saturating_sub(4 + detail_reserve) / 3).max(1);
    let start = focus.saturating_sub(capacity.saturating_sub(1));
    if start > 0 {
        rows.push(Line::styled(
            format!("↑ {start} earlier"),
            Style::default().fg(theme::dim()),
        ));
    }
    for (index, agent) in app.agents.iter().enumerate().skip(start) {
        if rows.len() + 3 > list_limit {
            break;
        }
        let (glyph, color) = match agent.state {
            State::Running => ("●", theme::cyan()),
            State::Done => ("✓", theme::green()),
            State::Failed => ("✗", theme::red()),
            State::Cancelled => ("■", theme::yellow()),
        };
        let elapsed = agent
            .finished
            .unwrap_or_else(Instant::now)
            .duration_since(agent.started);
        let time = duration(elapsed);
        let name = clip(&agent.name, width.saturating_sub(time.len() + 7));
        let selected = app.agent_selected == Some(index);
        let style = if selected {
            Style::default()
                .fg(theme::bg())
                .bg(theme::strong())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(color)
        };
        let marker = if selected { "❯ " } else { "  " };
        let mut header = Line::styled(format!("{marker}{glyph} {name} │ {time}"), style);
        header
            .spans
            .push(Span::raw(" ".repeat(width.saturating_sub(header.width()))));
        rows.push(header);
        let mut line = steps(&agent.todos, width.saturating_sub(5), tick, agent.state);
        if agent.todos.is_empty() {
            let activity = match agent.state {
                State::Running => agent.activity(),
                State::Done => "Completed".into(),
                State::Failed => "Failed".into(),
                State::Cancelled => "Cancelled".into(),
            };
            line = Line::styled(
                clip(&activity, width.saturating_sub(5)),
                Style::default().fg(theme::dim()),
            );
        }
        line.spans
            .insert(0, Span::styled("  └─ ", Style::default().fg(theme::dim())));
        rows.push(line);
        rows.push(Line::default());
    }
    let visible = rows.len();
    let list_height = u16::try_from(visible)
        .unwrap_or(inner.height)
        .min(inner.height);
    f.render_widget(
        Paragraph::new(rows),
        Rect {
            height: list_height,
            ..inner
        },
    );
    let detail_y = inner.y.saturating_add(list_height);
    let detail_height = inner.bottom().saturating_sub(detail_y + 1);
    if let Some(agent) = app.agent_selected.and_then(|i| app.agents.get(i)) {
        let mut detail = vec![Line::styled(
            agent.activity(),
            Style::default().fg(theme::fg()),
        )];
        if let Some(worktree) = &agent.worktree {
            detail.push(Line::raw(format!("makedo/{worktree}")));
        }
        if let Some(error) = &agent.error {
            detail.push(Line::styled(error, Style::default().fg(theme::red())));
        }
        if let Some(path) = &agent.log {
            detail.push(Line::raw(format!("log: {path}")));
        }
        f.render_widget(
            Paragraph::new(detail).wrap(Wrap { trim: false }),
            Rect {
                y: detail_y,
                height: detail_height,
                ..inner
            },
        );
    }
    f.render_widget(
        Paragraph::new("F6 details · F7 hide").style(Style::default().fg(theme::dim())),
        Rect {
            y: inner.bottom().saturating_sub(1),
            height: 1,
            ..inner
        },
    );
}

/// Fixture events use the production reducer and renderer; no provider or filesystem changes.
pub fn demo_events() -> Vec<SessionEvent> {
    let plan = |current: usize, labels: &[&str]| {
        labels
            .iter()
            .enumerate()
            .map(|(i, label)| TodoEntry {
                content: (*label).into(),
                status: match i.cmp(&current) {
                    std::cmp::Ordering::Less => TodoState::Completed,
                    std::cmp::Ordering::Equal => TodoState::InProgress,
                    std::cmp::Ordering::Greater => TodoState::Pending,
                },
            })
            .collect()
    };
    let child = |id: &str, event| SessionEvent::AgentEvent {
        id: id.into(),
        event: Box::new(event),
    };
    vec![
        SessionEvent::PromptAccepted { text: "Проверить изменения и подготовить результат".into(), queued_id: None },
        SessionEvent::TextDelta("Демонстрация панели прогресса. Это тестовые события: запросы к модели и команды не выполняются.\n\nF6 — детали агента, F7 — скрыть панель, Ctrl+C — выход.".into()),
        SessionEvent::PhaseChanged("Agents".into()),
        SessionEvent::TodoUpdated(plan(2, &["Анализ", "Правки", "Проверки", "Ревью"])),
        SessionEvent::AgentStarted { id: "review".into(), name: "code-review".into(), worktree: None },
        child("review", SessionEvent::TodoUpdated(plan(1, &["Анализ", "Отчёт"]))),
        child("review", SessionEvent::PhaseChanged("Writing report".into())),
        SessionEvent::AgentStarted { id: "backend".into(), name: "backend".into(), worktree: Some("backend".into()) },
        child("backend", SessionEvent::TodoUpdated(plan(2, &["Анализ", "Правки", "Тесты"]))),
        child("backend", SessionEvent::PhaseChanged("Verification · cargo test".into())),
        SessionEvent::AgentStarted { id: "ui".into(), name: "UI-аудит".into(), worktree: None },
        child("ui", SessionEvent::TodoUpdated(plan(2, &["Анализ", "Отчёт"]))),
        SessionEvent::ToolFinished { call_id: "ui".into(), name: "task".into(), is_error: false, preview: "Audit complete".into() },
    ]
}

pub async fn demo() -> Result<(), crate::TuiError> {
    let (commands, mut incoming) = tokio::sync::mpsc::channel(16);
    let (events, outgoing) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        for event in demo_events() {
            if events.send(event).await.is_err() {
                return;
            }
        }
        while let Some(command) = incoming.recv().await {
            let event = if matches!(command, makedo_session::SessionCommand::Cancel) {
                SessionEvent::TurnFinished {
                    cancelled: true,
                    usage: makedo_types::event::TurnUsage::default(),
                }
            } else {
                SessionEvent::Notice("Demo only — run makedo normally for a real session.".into())
            };
            if events.send(event).await.is_err() {
                return;
            }
        }
    });
    crate::run(
        makedo_session::SessionHandle {
            commands,
            events: outgoing,
        },
        &makedo_config::TuiConfig::default(),
        false,
        false,
        "DEMO · no API".into(),
        "none".into(),
    )
    .await
}
