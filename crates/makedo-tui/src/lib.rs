//! TUI makedo: ratatui + crossterm. Рендер только по dirty-событию или тику
//! активного спиннера — без «60 fps всегда» (батарея старых машин, PLAN §3.6).

pub mod app;
pub mod commands;
pub mod loaders;
pub mod markdown;
pub mod pipeline;
pub mod spinner;
pub mod theme;

mod render_tests;

use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Duration;

use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use makedo_config::TuiConfig;
use makedo_session::{SessionCommand, SessionHandle};
use makedo_types::event::SessionEvent;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use app::{App, Entry, ToolStatus};

// хосту-окну (`makedo app`) нужны те же типы событий и `Terminal`, что и нам
pub use {crossterm, ratatui};

/// Куда рисуем и откуда берём ввод: терминал (crossterm) или окно (`makedo app`).
#[allow(missing_debug_implementations)] // внутри замыкание и чужой Terminal
pub struct Host<'t, B: ratatui::backend::Backend, S> {
    pub terminal: &'t mut ratatui::Terminal<B>,
    pub events: S,
    /// Перед каждым кадром: заголовок таба/окна, у окна ещё и ресайз поверхности.
    #[allow(clippy::type_complexity)] // один раз, отдельный type-алиас длиннее
    pub before_draw: Box<dyn FnMut(&mut ratatui::Terminal<B>, &str) + 't>,
    /// Хост — наше окно со своим шрифтом: курсор рисуем сами (wgpu его не рисует),
    /// а значок ветки берём из powerline-набора вместо `⎇`.
    pub own_window: bool,
}

/// События окна из канала → поток того же вида, что crossterm `EventStream`.
pub fn events_from(
    rx: tokio::sync::mpsc::Receiver<TermEvent>,
) -> impl futures_util::Stream<Item = std::io::Result<TermEvent>> + Unpin {
    Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|ev| (Ok(ev), rx))
    }))
}

#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("terminal io: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "makedo's interactive UI needs a real terminal (stdin/stdout are not a TTY).\n\
         Run `makedo` directly in a terminal, or use headless mode: makedo -p \"<prompt>\""
    )]
    NotATerminal,
}

/// Интерактивный полноэкранный режим. `needs_setup` = провайдер не настроен
/// (первый запуск без ключа) → сразу открываем выбор `/models`.
pub async fn run(
    mut handle: SessionHandle,
    cfg: &TuiConfig,
    needs_setup: bool,
    global_data: bool,
    model: String,
    doctrine: String,
) -> Result<(), TuiError> {
    // TUI требует настоящий терминал; иначе ratatui::init() паникует
    // ("Device not configured"). Проверяем заранее и даём внятную ошибку.
    use crossterm::event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    };
    use std::io::IsTerminal as _;
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return Err(TuiError::NotATerminal);
    }
    install_panic_hook();
    let mut terminal = ratatui::init();
    // включаем захват мыши (колесо-скролл), фокус-события и bracketed paste
    // (чистая многострочная вставка). ratatui::init() их не включает.
    let _ = crossterm::execute!(
        std::io::stdout(),
        EnableMouseCapture,
        EnableFocusChange,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    let mut cur_title = String::new();
    let host = Host {
        terminal: &mut terminal,
        events: EventStream::new(),
        // заголовок таба терминала: активность сессии (OSC через crossterm SetTitle),
        // шлём только при изменении — не мусорим в stdout каждый кадр
        before_draw: Box::new(move |_, title| {
            if title != cur_title {
                let _ =
                    crossterm::execute!(std::io::stdout(), crossterm::terminal::SetTitle(title));
                title.clone_into(&mut cur_title);
            }
        }),
        own_window: false,
    };
    let result = run_on(
        host,
        &mut handle,
        cfg,
        needs_setup,
        global_data,
        model,
        doctrine,
    )
    .await;
    // сбрасываем заголовок таба + выключаем захват мыши/фокуса/paste
    let _ = crossterm::execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableFocusChange,
        DisableBracketedPaste,
        PopKeyboardEnhancementFlags,
        crossterm::terminal::SetTitle("")
    );
    ratatui::restore();
    result
}

/// Паника в raw-mode оставила бы терминал сломанным и без сообщения (panic=abort).
/// Хук восстанавливает терминал и пишет причину в `~/.makedo/crash.log`.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PopKeyboardEnhancementFlags
        );
        ratatui::restore();
        if let Some(dir) = makedo_config::global_dir() {
            let _ = std::fs::create_dir_all(&dir);
            let msg = format!("makedo panic: {info}\n");
            let _ = std::fs::write(dir.join("crash.log"), &msg);
            eprintln!("{msg}(details in {}/crash.log)", dir.display());
        } else {
            eprintln!("makedo panic: {info}");
        }
        prev(info);
    }));
}

/// Шапка: "<branch> <cwd>" — ветка git (если репо) + сокращённый путь.
fn git_header() -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let path = cwd.to_string_lossy();
    let home = std::env::var("HOME").unwrap_or_default();
    let short = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.into_owned()
    };
    let branch = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|b| !b.is_empty());
    match branch {
        Some(b) => format!("{b} {short}"),
        None => short,
    }
}

/// Wrap by terminal cells, preserving graphemes and span/line styles.
/// Explicit rows keep scroll offsets consistent with the rendered buffer.
fn wrap_line(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    for span in &line.spans {
        let mut buf = String::new();
        for grapheme in span.styled_graphemes(Style::default()) {
            let cells = Span::raw(grapheme.symbol).width();
            if cur_w > 0 && cur_w + cells > width {
                if !buf.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut buf), span.style));
                }
                rows.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            buf.push_str(grapheme.symbol);
            cur_w += cells;
        }
        if !buf.is_empty() {
            cur.push(Span::styled(buf, span.style));
        }
    }
    if !cur.is_empty() || rows.is_empty() {
        rows.push(cur); // хвост либо (для пустой строки) единственный пустой ряд
    }
    rows.into_iter()
        .map(|spans| Line::from(spans).style(line.style))
        .collect()
}

/// Усекает строку до `max` символов, добавляя «…» при обрезке.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// Цикл TUI на любом хосте: терминал (`run`) или окно (`makedo app`).
pub async fn run_on<B, S>(
    host: Host<'_, B, S>,
    handle: &mut SessionHandle,
    cfg: &TuiConfig,
    needs_setup: bool,
    global_data: bool,
    model: String,
    doctrine: String,
) -> Result<(), TuiError>
where
    B: ratatui::backend::Backend<Error = std::io::Error>,
    S: futures_util::Stream<Item = std::io::Result<TermEvent>> + Unpin,
{
    let Host {
        terminal,
        events: mut term_events,
        mut before_draw,
        own_window,
    } = host;
    // тема из конфига (если задана) — иначе дефолт grok
    if let Some(t) = &cfg.theme {
        theme::set(t);
    }
    let mut app = App {
        own_window,
        header: git_header(),
        model,
        doctrine,
        providers: commands::provider_choices(&makedo_providers::catalog::load_custom(
            makedo_config::cache_dir().as_deref(),
        )),
        // пинг локального Ollama: если демон поднят — сразу видим его модели,
        // без «подключения». Отказ соединения мгновенный (не ждём таймаут).
        ollama: makedo_providers::ollama_models().await,
        ..App::default()
    };
    // сессия вне git-проекта: данные хранятся глобально — уведомляем явно
    if global_data && !needs_setup {
        for line in [
            "Not a git project — this session's history and wiki are saved globally in ~/.makedo.",
            "Nothing to do. Run /init anytime to move all data into the project.",
        ] {
            app.log.push(Entry::Notice(line.to_owned()));
        }
    }
    if needs_setup {
        // первый запуск без модели: сразу показываем выбор провайдера
        app.first_run = true;
        app.modal = app::Modal::Providers { sel: 0 };
        app.log.push(Entry::Assistant(
            "Welcome to makedo. Pick a provider and model to begin:".to_owned(),
        ));
    }
    let mut tick: u64 = 0;
    let mut dirty = true;
    loop {
        before_draw(terminal, &app.title());
        if dirty || app.busy {
            terminal.draw(|f| draw(f, &app, tick))?;
            dirty = false;
        }
        let sp = spinner::resolve(app.process_kind());
        let tick_delay = Duration::from_millis(sp.interval_ms.max(50));
        tokio::select! {
            ev = handle.events.recv() => match ev {
                Some(ev) => {
                    app.apply(ev);
                    // коалесинг: втягиваем ВСЕ накопленные события до одного draw —
                    // иначе каждый токен-дельта = полный кадр (тормоза при стриминге)
                    while let Ok(ev) = handle.events.try_recv() {
                        app.apply(ev);
                    }
                    dirty = true;
                }
                None => return Ok(()), // актор умер
            },
            ev = term_events.next() => {
                use crossterm::event::{MouseEventKind, KeyEventKind};
                match ev {
                    // KeyEventKind::Release/Repeat приходят на некоторых терминалах —
                    // реагируем только на Press, иначе двойной ввод
                    Some(Ok(TermEvent::Key(key))) if key.kind != KeyEventKind::Release => {
                        handle_key(key.code, key.modifiers, &mut app, handle).await;
                        dirty = true;
                    }
                    // колесо мыши — скролл лога (как в grok/opencode)
                    Some(Ok(TermEvent::Mouse(m))) => {
                        match m.kind {
                            MouseEventKind::ScrollUp => app.scroll_up(3),
                            MouseEventKind::ScrollDown => app.scroll_down(3),
                            _ => {}
                        }
                        dirty = true;
                    }
                    // bracketed paste — вставляем как есть (многострочное — в input)
                    Some(Ok(TermEvent::Paste(text))) => {
                        handle_paste(&text, &mut app);
                        dirty = true;
                    }
                    // фокус окна: на потере гасим тики спиннера (батарея фоновой вкладки)
                    Some(Ok(TermEvent::FocusGained)) => { app.unfocused = false; dirty = true; }
                    Some(Ok(TermEvent::FocusLost)) => { app.unfocused = true; }
                    // ресайз окна терминала → перерисовать под новый размер
                    // (ratatui autoresize внутри draw подхватит новую геометрию)
                    Some(Ok(TermEvent::Resize(..))) => dirty = true,
                    _ => {}
                }
                if app.should_quit {
                    return Ok(());
                }
            }
            () = tokio::time::sleep(tick_delay), if app.busy && !app.unfocused => {
                tick = tick.wrapping_add(1);
                dirty = true;
            }
        }
    }
}

#[allow(clippy::too_many_lines)] // линейная диспетчеризация клавиш по контексту
async fn handle_key(code: KeyCode, mods: KeyModifiers, app: &mut App, handle: &mut SessionHandle) {
    use KeyModifiers as KM;
    // Ctrl+C выходит ВСЕГДА — раньше любых модалок/палитр (иначе можно застрять)
    if matches!(code, KeyCode::Char('c')) && mods.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return;
    }
    // Ctrl+Enter steers immediately; with an empty draft it flushes the inbox.
    // Do not take text from model/API-key dialogs or interpret it as approval.
    if code == KeyCode::Enter
        && mods.contains(KM::CONTROL)
        && !app.modal.is_open()
        && !app.input.starts_with('/')
        && (app.busy || !app.input.is_empty())
    {
        let text = std::mem::take(&mut app.input);
        app.cursor = 0;
        app.palette.selected = 0;
        app.busy = true;
        let _ = handle.commands.send(SessionCommand::Steer(text)).await;
        return;
    }
    // permission-модалка перехватывает ввод
    if let Some((request_id, _)) = app.permission {
        // Buffer y/Y until Enter so typing YOLO cannot accidentally approve only one call.
        if !app.permission_input.is_empty() || matches!(code, KeyCode::Char('y' | 'Y')) {
            match code {
                KeyCode::Enter
                    if makedo_session::permission::requests_yolo(&app.permission_input) =>
                {
                    let text = std::mem::take(&mut app.permission_input);
                    let _ = handle.commands.send(SessionCommand::Prompt(text)).await;
                }
                KeyCode::Enter if app.permission_input.eq_ignore_ascii_case("y") => {
                    app.permission_input.clear();
                    app.permission = None;
                    let _ = handle
                        .commands
                        .send(SessionCommand::PermissionReply {
                            request_id,
                            approve: true,
                            always: false,
                        })
                        .await;
                }
                KeyCode::Backspace => {
                    app.permission_input.pop();
                }
                KeyCode::Esc => app.permission_input.clear(),
                KeyCode::Char(c)
                    if !mods.contains(KM::CONTROL) && app.permission_input.len() < 4096 =>
                {
                    app.permission_input.push(c);
                }
                _ => {}
            }
            return;
        }
        // Клавиши принимаем и на кириллице — на ЙЦУКЕН те же физические клавиши
        // дают: y→н, a→ф, n→т. Так шорткаты работают в русской раскладке.
        // A (allow-all) — перейти в режим allow-all прямо из запроса.
        if matches!(code, KeyCode::Char('A' | 'Ф')) {
            app.permission = None;
            app.allow_all = true;
            let _ = handle.commands.send(SessionCommand::SetAllowAll).await;
            return;
        }
        let decision = match code {
            KeyCode::Char('y' | 'Y' | 'н' | 'Н') => Some((true, false)),
            KeyCode::Char('a' | 'ф') => Some((true, true)), // always (эта программа)
            KeyCode::Char('n' | 'N' | 'т' | 'Т') | KeyCode::Esc => Some((false, false)),
            _ => None,
        };
        if let Some((approve, always)) = decision {
            app.permission = None;
            let _ = handle
                .commands
                .send(SessionCommand::PermissionReply {
                    request_id,
                    approve,
                    always,
                })
                .await;
        }
        return;
    }
    // модалка выбора модели перехватывает весь ввод
    if app.modal.is_open() {
        handle_modal_key(code, app, handle).await;
        return;
    }
    // палитра команд: перехватывает навигацию/автодополнение, пока открыта
    if app.palette_open() {
        let items = crate::commands::Palette::matches(app.palette_filter());
        match code {
            KeyCode::Up => {
                app.palette.move_up(items.len());
                return;
            }
            KeyCode::Down | KeyCode::Tab => {
                app.palette.move_down(items.len());
                return;
            }
            KeyCode::Esc => {
                app.clear_input();
                return;
            }
            KeyCode::Enter if !items.is_empty() => {
                app.palette.clamp(items.len());
                let chosen = items[app.palette.selected].cmd;
                app.input = format!("/{}", chosen.name);
                app.cursor = app.input.chars().count();
                app.palette.selected = 0;
                // команды без аргумента выполняем сразу; с аргументом — оставляем
                // строку с пробелом, чтобы пользователь дописал
                if chosen.arg_hint.is_empty() {
                    let text = std::mem::take(&mut app.input);
                    app.cursor = 0;
                    handle_slash(text.trim_start_matches('/'), app, handle).await;
                } else {
                    app.input.push(' ');
                    app.cursor = app.input.chars().count();
                }
                return;
            }
            _ => {}
        }
    }
    // прокрутка истории: PageUp/Down всегда, стрелки — когда ввод пуст
    match code {
        KeyCode::PageUp => {
            app.scroll_up(10);
            return;
        }
        KeyCode::PageDown => {
            app.scroll_down(10);
            return;
        }
        KeyCode::Up if app.input.is_empty() => {
            app.scroll_up(1);
            return;
        }
        KeyCode::Down if app.input.is_empty() => {
            app.scroll_down(1);
            return;
        }
        _ => {}
    }
    match (code, mods) {
        (KeyCode::F(6), _) if !app.agents.is_empty() => {
            app.agents_hidden = false;
            app.agent_selected = match app.agent_selected {
                Some(index) if index + 1 < app.agents.len() => Some(index + 1),
                Some(_) => None,
                None => Some(0),
            };
        }
        (KeyCode::F(7), _) => app.agents_hidden = !app.agents_hidden,
        (KeyCode::Esc, _) if app.busy => {
            let _ = handle.commands.send(SessionCommand::Cancel).await;
        }
        // Shift+Enter / Alt+Enter — перенос строки (многострочный ввод), НЕ отправка
        (KeyCode::Enter, m) if m.intersects(KM::SHIFT | KM::ALT) => app.newline(),
        (KeyCode::Enter, _)
            if !app.input.is_empty() && (!app.busy || !app.input.starts_with('/')) =>
        {
            let text = std::mem::take(&mut app.input);
            app.cursor = 0;
            app.palette.selected = 0;
            if let Some(cmd) = text.strip_prefix('/') {
                handle_slash(cmd, app, handle).await;
            } else {
                app.busy = true;
                let _ = handle.commands.send(SessionCommand::Prompt(text)).await;
            }
        }
        // навигация курсора (readline-минимум)
        (KeyCode::Left, m) if m.contains(KM::CONTROL) || m.contains(KM::ALT) => app.word_left(),
        (KeyCode::Right, m) if m.contains(KM::CONTROL) || m.contains(KM::ALT) => app.word_right(),
        (KeyCode::Left, _) => app.move_left(),
        (KeyCode::Right, _) => app.move_right(),
        (KeyCode::Home, _) | (KeyCode::Char('a'), KM::CONTROL) => app.home(),
        (KeyCode::End, _) | (KeyCode::Char('e'), KM::CONTROL) => app.end(),
        (KeyCode::Delete, _) => app.delete(),
        (KeyCode::Char('u'), KM::CONTROL) => app.kill_to_line_start(),
        (KeyCode::Char('k'), KM::CONTROL) => app.kill_to_line_end(),
        (KeyCode::Char('w'), KM::CONTROL) | (KeyCode::Backspace, KM::CONTROL | KM::ALT) => {
            app.delete_word_back();
            app.palette.selected = 0;
        }
        (KeyCode::Backspace, _) => {
            app.backspace();
            app.palette.selected = 0;
        }
        (KeyCode::Char(c), m) if !m.contains(KM::CONTROL) => {
            app.insert_char(c);
            app.palette.selected = 0;
        }
        _ => {}
    }
}

fn handle_paste(text: &str, app: &mut App) {
    use crate::app::Modal;
    match &mut app.modal {
        Modal::ApiKey { buf, .. } => buf.extend(text.chars().filter(|c| !c.is_control())),
        Modal::None if app.permission.is_none() => app.insert_str(text),
        Modal::None => app.permission_input.extend(
            text.replace(['\n', '\r', '\t'], " ")
                .chars()
                .filter(|c| !c.is_control())
                .take(4096),
        ),
        _ => {}
    }
}

/// Model picker: provider, model, key.
#[allow(clippy::too_many_lines)] // linear modal dispatch
async fn handle_modal_key(code: KeyCode, app: &mut App, handle: &mut SessionHandle) {
    use app::Modal;
    match &mut app.modal {
        Modal::Providers { sel } => {
            let n = app.providers.len();
            match code {
                KeyCode::Up => *sel = (*sel + n - 1) % n,
                KeyCode::Down | KeyCode::Tab => *sel = (*sel + 1) % n,
                // Esc из любого шага → закрыть меню в начало
                KeyCode::Esc => {
                    app.modal = Modal::None;
                    app.clear_input();
                }
                KeyCode::Enter => {
                    let idx = *sel;
                    let p = &app.providers[idx];
                    // ollama — уже отпингованные локальные модели (или свежий
                    // пинг, если кэш пуст); остальные — из каталога models.dev
                    let models = if p.kind == "ollama" {
                        if app.ollama.is_empty() {
                            makedo_providers::ollama_models().await
                        } else {
                            app.ollama.clone()
                        }
                    } else if let Some(m) = makedo_providers::cli_models(&p.kind) {
                        m
                    } else if p.kind == "chatgpt" {
                        // не вошли / вход истёк — текст ошибки сам говорит, что делать
                        makedo_providers::chatgpt::models()
                            .await
                            .unwrap_or_else(|error| {
                                app.log.push(Entry::Error(error.to_string()));
                                Vec::new()
                            })
                    } else {
                        provider_models(&p.kind)
                    };
                    if models.is_empty() {
                        let msg = if p.kind == "chatgpt" {
                            "No ChatGPT plan models available: sign in with `makedo login chatgpt`."
                                .to_owned()
                        } else if p.kind == "ollama" {
                            "No local Ollama models found. Is `ollama serve` running? Pull one with `ollama pull llama3.2`.".to_owned()
                        } else {
                            format!("no models for {} in catalog", p.kind)
                        };
                        app.log.push(Entry::Error(msg));
                        app.modal = Modal::None;
                    } else {
                        app.modal = Modal::Models {
                            provider: idx,
                            models,
                            sel: 0,
                        };
                    }
                }
                _ => {}
            }
        }
        Modal::Models {
            provider,
            models,
            sel,
        } => {
            let n = models.len();
            match code {
                KeyCode::Up => *sel = (*sel + n - 1) % n,
                KeyCode::Down | KeyCode::Tab => *sel = (*sel + 1) % n,
                KeyCode::Esc => {
                    app.modal = Modal::None;
                    app.clear_input();
                }
                KeyCode::Enter => {
                    let idx = *provider;
                    let model = models[*sel].clone();
                    let p = app.providers[idx].clone();
                    // ключ уже есть (env/credentials) ИЛИ провайдер keyless (ollama)?
                    // → сразу переключаем, минуя шаг ввода ключа
                    let has_key = p.env.is_empty()
                        || std::env::var(&p.env).is_ok()
                        || makedo_config::credentials::get(&p.env).is_some();
                    if has_key {
                        switch_and_close(app, handle, &p.kind, &model).await;
                    } else {
                        app.modal = Modal::ApiKey {
                            provider: idx,
                            model,
                            buf: String::new(),
                        };
                    }
                }
                _ => {}
            }
        }
        Modal::ApiKey {
            provider,
            model,
            buf,
        } => match code {
            KeyCode::Esc => {
                app.modal = Modal::None;
                app.clear_input();
            }
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(c) => buf.push(c),
            KeyCode::Enter if !buf.is_empty() => {
                let (idx, model, key) = (*provider, model.clone(), buf.clone());
                let p = app.providers[idx].clone();
                let _ = makedo_config::credentials::save(&p.env, key.trim());
                switch_and_close(app, handle, &p.kind, &model).await;
            }
            _ => {}
        },
        // Выбор темы с ЖИВЫМ предпросмотром: стрелки применяют тему немедленно.
        Modal::Themes { sel, original } => {
            let n = theme::NAMES.len();
            match code {
                KeyCode::Up => {
                    *sel = (*sel + n - 1) % n;
                    theme::set(theme::NAMES[*sel]); // применяем сразу
                }
                KeyCode::Down | KeyCode::Tab => {
                    *sel = (*sel + 1) % n;
                    theme::set(theme::NAMES[*sel]);
                }
                KeyCode::Esc => {
                    // откат к теме, что была при открытии
                    theme::set(theme::NAMES[*original]);
                    app.modal = Modal::None;
                    app.clear_input();
                }
                KeyCode::Enter => {
                    theme::set(theme::NAMES[*sel]);
                    let _ = makedo_config::persist_theme(theme::name());
                    app.log
                        .push(Entry::Notice(format!("theme → {}", theme::name())));
                    app.modal = Modal::None;
                    app.clear_input();
                }
                _ => {}
            }
        }
        Modal::Doctrine { sel } => {
            let choices = makedo_config::Doctrine::choices();
            let n = choices.len();
            match code {
                KeyCode::Up => *sel = (*sel + n - 1) % n,
                KeyCode::Down | KeyCode::Tab => *sel = (*sel + 1) % n,
                KeyCode::Esc => {
                    app.modal = Modal::None;
                    app.clear_input();
                }
                KeyCode::Enter => {
                    let d = choices[*sel].0;
                    apply_doctrine(app, handle, d).await;
                    app.clear_input();
                }
                _ => {}
            }
        }
        Modal::Effort { sel } => {
            let n = app.effort_levels.len();
            match code {
                KeyCode::Up if n > 0 => *sel = (*sel + n - 1) % n,
                KeyCode::Down | KeyCode::Tab if n > 0 => *sel = (*sel + 1) % n,
                KeyCode::Esc => {
                    app.modal = Modal::None;
                    app.clear_input();
                }
                KeyCode::Enter if n > 0 => {
                    let value = app.effort_levels[*sel].clone();
                    app.modal = Modal::None;
                    app.clear_input();
                    let _ = handle.commands.send(SessionCommand::SetEffort(value)).await;
                }
                _ => {}
            }
        }
        // онбординг: Enter — да, Esc/n — пропустить шаг
        Modal::Onboard { step, cmds } => {
            let yes = matches!(code, KeyCode::Enter | KeyCode::Char('y'));
            let no = matches!(code, KeyCode::Esc | KeyCode::Char('n'));
            if !yes && !no {
                return;
            }
            if *step == 0 {
                if yes {
                    app.busy = true;
                    app.log.push(Entry::Notice(
                        "warming up the wiki — a curator turn is reading the project".to_owned(),
                    ));
                    let _ = handle.commands.send(SessionCommand::Enrich(None)).await;
                }
                if cmds.is_empty() {
                    app.modal = Modal::None; // verify-шага нет — нечего предлагать
                } else {
                    app.modal = Modal::Onboard {
                        step: 1,
                        cmds: cmds.clone(),
                    };
                }
            } else {
                if yes {
                    let cmds = cmds.clone();
                    if let Ok(dir) = std::env::current_dir() {
                        let _ = makedo_config::persist_verify_cmds(&dir, &cmds);
                    }
                    let _ = handle
                        .commands
                        .send(SessionCommand::SetVerifyCmds(cmds))
                        .await;
                }
                app.modal = Modal::None;
            }
        }
        Modal::None => {}
    }
}

/// Модели провайдера из каталога, отсортированные.
fn provider_models(kind: &str) -> Vec<String> {
    let catalog = makedo_providers::catalog::load(makedo_config::cache_dir().as_deref());
    catalog.get(kind).map_or_else(Vec::new, |e| {
        let mut m: Vec<String> = e.models.keys().cloned().collect();
        m.sort();
        m
    })
}

async fn switch_and_close(app: &mut App, handle: &mut SessionHandle, kind: &str, model: &str) {
    app.modal = app::Modal::None;
    let _ = handle
        .commands
        .send(SessionCommand::SwitchModel {
            kind: kind.to_owned(),
            model: model.to_owned(),
        })
        .await;
    // онбординг первого запуска: прогреть wiki → включить самопроверку
    if app.first_run {
        app.first_run = false;
        let cmds = std::env::current_dir()
            .map(|d| makedo_config::detect_verify_cmds(&d))
            .unwrap_or_default();
        app.modal = app::Modal::Onboard { step: 0, cmds };
    }
}

/// Применяет доктрину: сохраняет в конфиг + просит сессию пересобрать промпт (live).
async fn apply_doctrine(app: &mut App, handle: &mut SessionHandle, d: makedo_config::Doctrine) {
    app.modal = app::Modal::None;
    d.as_str().clone_into(&mut app.doctrine);
    let _ = makedo_config::persist_doctrine(d.as_str());
    let _ = handle.commands.send(SessionCommand::SetDoctrine(d)).await;
}

/// Слэш-команды TUI — алгоритмические, без LLM (кроме отправки промпта/тула).
#[allow(clippy::too_many_lines)] // линейная диспетчеризация слэш-команд
async fn handle_slash(cmd: &str, app: &mut App, handle: &mut SessionHandle) {
    let (name, rest) = cmd.split_once(' ').unwrap_or((cmd, ""));
    let rest = rest.trim();
    match name {
        "agents" => app.agents_hidden = !app.agents_hidden,
        "help" => {
            for line in crate::commands::help_text().lines() {
                app.log.push(Entry::Assistant(line.to_owned()));
            }
        }
        "ponytail" => {
            let level = match rest {
                "lite" => makedo_config::PonytailLevel::Lite,
                "" | "full" => makedo_config::PonytailLevel::Full,
                "ultra" => makedo_config::PonytailLevel::Ultra,
                "off" => makedo_config::PonytailLevel::Off,
                _ => {
                    app.log.push(Entry::Error(
                        "usage: /ponytail [lite|full|ultra|off]".into(),
                    ));
                    return;
                }
            };
            let _ = handle
                .commands
                .send(SessionCommand::SetPonytail(level))
                .await;
        }
        "clear" => {
            app.log.clear();
            app.assistant_stream = None;
            app.todos.clear();
            app.log.push(Entry::Notice(
                "Screen cleared; model context retained.".into(),
            ));
        }
        "logs" => {
            if app.tool_logs.is_empty() {
                app.log
                    .push(Entry::Notice("No tool logs in this view yet.".into()));
            }
            for (id, name, path) in &app.tool_logs {
                app.log
                    .push(Entry::Notice(format!("[{id}] {name}: {path}")));
            }
            app.scroll = 0;
        }
        "quit" => app.should_quit = true,
        "theme" => {
            if rest.is_empty() {
                // без аргумента — открываем интерактивный выбор с предпросмотром
                let cur = theme::NAMES
                    .iter()
                    .position(|n| *n == theme::name())
                    .unwrap_or(0);
                app.modal = app::Modal::Themes {
                    sel: cur,
                    original: cur,
                };
            } else if theme::set(rest) {
                let _ = makedo_config::persist_theme(theme::name());
                app.log
                    .push(Entry::Notice(format!("theme → {}", theme::name())));
            } else {
                app.log.push(Entry::Error(format!(
                    "unknown theme '{rest}' — try: {}",
                    theme::NAMES.join(", ")
                )));
            }
        }
        // доктрина проектирования — пикер (или прямой аргумент)
        "doctrine" => {
            let choices = makedo_config::Doctrine::choices();
            if rest.is_empty() {
                let sel = choices
                    .iter()
                    .position(|(_, n, _)| *n == app.doctrine)
                    .unwrap_or(0);
                app.modal = app::Modal::Doctrine { sel };
            } else if let Some(d) = makedo_config::Doctrine::from_name(rest) {
                apply_doctrine(app, handle, d).await;
            } else {
                app.log.push(Entry::Error(format!(
                    "unknown doctrine '{rest}' — try: none, ddd, hexagonal"
                )));
            }
        }
        "allow-all" | "yolo" => {
            app.allow_all = true; // метка режима в правом-нижнем углу инпута
            let _ = handle.commands.send(SessionCommand::SetAllowAll).await;
        }
        "effort" => {
            if app.effort_levels.is_empty() {
                app.log.push(Entry::Error(
                    "No effort levels configured for this model.".to_owned(),
                ));
            } else if rest.is_empty() {
                let sel = app
                    .effort_levels
                    .iter()
                    .position(|level| Some(level) == app.effort.as_ref())
                    .unwrap_or(0);
                app.modal = app::Modal::Effort { sel };
            } else {
                let _ = handle
                    .commands
                    .send(SessionCommand::SetEffort(rest.to_owned()))
                    .await;
            }
        }
        "init" => {
            // git init + перенос данных в проект (subprocess makedo init)
            let exe = std::env::current_exe().unwrap_or_else(|_| "makedo".into());
            match std::process::Command::new(exe).arg("init").output() {
                Ok(out) => {
                    for line in String::from_utf8_lossy(&out.stderr).lines() {
                        app.log.push(Entry::Notice(line.to_owned()));
                    }
                    app.log.push(Entry::Notice(
                        "Data moved into the project. Restart makedo (Ctrl+C) to use the new location."
                            .to_owned(),
                    ));
                }
                Err(e) => app.log.push(Entry::Error(format!("init failed: {e}"))),
            }
        }
        "create-image" => {
            if rest.is_empty() {
                app.log
                    .push(Entry::Error("usage: /create-image <prompt>".to_owned()));
                return;
            }
            app.busy = true;
            let _ = handle
                .commands
                .send(SessionCommand::RunTool {
                    name: "image_gen".to_owned(),
                    args: serde_json::json!({"prompt": rest}),
                })
                .await;
        }
        // прогрев wiki: турн-куратор в текущей сессии (записи в wiki авторазрешены)
        "enrich" => {
            app.busy = true;
            let focus = (!rest.is_empty()).then(|| rest.to_owned());
            let _ = handle.commands.send(SessionCommand::Enrich(focus)).await;
        }
        // выбор модели — интерактивно ВНУТРИ TUI (провайдер → модель → ключ)
        "models" | "model" => {
            // освежаем список локальных Ollama-моделей на момент открытия
            app.ollama = makedo_providers::ollama_models().await;
            app.modal = app::Modal::Providers { sel: 0 };
        }
        // worktree/finish — git-операции процесс-уровня; TUI не дублирует логику
        "worktree" | "finish" => {
            app.log.push(Entry::Assistant(format!(
                "Run `makedo {name}{}` from a shell — these act on git worktrees.",
                if rest.is_empty() {
                    String::new()
                } else {
                    format!(" {rest}")
                }
            )));
        }
        other => {
            app.log.push(Entry::Error(format!(
                "unknown command: /{other} — type / to see the list"
            )));
        }
    }
}

#[allow(clippy::too_many_lines)] // линейный рендер; нарезка скрыла бы поток
pub(crate) fn draw(f: &mut ratatui::Frame<'_>, app: &App, tick: u64) {
    // затираем ВЕСЬ экран: зона margin ниже виджетами не покрыта и иначе хранит
    // символы прошлого кадра (артефакты слева при скролле).
    f.render_widget(ratatui::widgets::Clear, f.area());
    // единый фон темы поверх всего кадра (grok: тёмный, независимо от терминала)
    f.render_widget(
        Block::default().style(Style::default().bg(theme::bg()).fg(theme::fg())),
        f.area(),
    );
    // отступ по всему периметру (приятные поля, как в grok)
    let root = Layout::default()
        .horizontal_margin(2)
        .vertical_margin(1)
        .constraints([Constraint::Min(0)])
        .split(f.area())[0];

    let screen_root = root;
    let show_agents = pipeline::sidebar(app, root);
    let root = if show_agents {
        let [main, _, agents] = Layout::horizontal([
            Constraint::Min(60),
            Constraint::Length(2),
            Constraint::Length(44),
        ])
        .areas(root);
        pipeline::draw(f, app, agents, tick);
        main
    } else {
        root
    };

    // One horizontal plan line; a collapsed agent summary on narrow terminals.
    #[allow(clippy::cast_possible_truncation)] // len ≤ 8
    let todo_height =
        u16::from(!app.todos.is_empty()) + u16::from(!app.agents.is_empty() && !show_agents);
    let gap_todo = u16::from(todo_height > 0); // воздух над todo, только если он есть
    let header_h = u16::from(!app.header.is_empty());
    // Borders (2), padding (2), prompt (2). Use the same rows for height,
    // rendering and cursor placement, including soft wraps and wide graphemes.
    let (input_rows, input_cursor) =
        wrap_text(&app.input, app.cursor, root.width.saturating_sub(6));
    // Grow up to half the screen, keeping room for the log, status and footer.
    // Longer drafts scroll vertically to keep the cursor visible.
    let max_input_rows = (root.height / 2)
        .min(
            root.height
                .saturating_sub(header_h + todo_height + gap_todo + 10),
        )
        .max(1);
    #[allow(clippy::cast_possible_truncation)]
    let input_h = input_rows.len().min(usize::from(max_input_rows)) as u16 + 2;
    // пустые строки-разделители дают «воздух»: лог ↔ todo ↔ индикатор ↔ ввод ↔ футер
    let [
        header_area,
        log_area,
        _gap_todo,
        todo_area,
        _gap_log,
        status_area,
        _gap_status,
        input_area,
        _gap_footer,
        footer_area,
    ] = Layout::vertical([
        Constraint::Length(header_h),
        Constraint::Min(3),
        Constraint::Length(gap_todo), // воздух между логом и todo
        Constraint::Length(todo_height),
        Constraint::Length(1), // воздух над индикатором
        Constraint::Length(1),
        Constraint::Length(1), // воздух перед вводом
        Constraint::Length(input_h),
        Constraint::Length(1), // воздух перед футером
        Constraint::Length(1),
    ])
    .areas(root);

    // git-шапка: ветка + путь (слева), токены сессии (справа)
    if header_h > 0 {
        let tok = app.last_usage.map_or_else(String::new, |u| {
            format!("{}K tok", (u.input_tokens + u.output_tokens) / 1000)
        });
        let left = Span::styled(
            // U+E0A0 — powerline-значок ветки: в терминале его может не быть, в окне шрифт наш
            format!(
                "{} {}",
                if app.own_window { "\u{e0a0}" } else { "⎇" },
                app.header
            ),
            Style::default().fg(theme::dim()),
        );
        let pad = usize::from(header_area.width)
            .saturating_sub(app.header.chars().count() + 2 + tok.chars().count());
        let line = Line::from(vec![
            left,
            Span::raw(" ".repeat(pad)),
            Span::styled(tok, Style::default().fg(theme::dim())),
        ]);
        f.render_widget(Paragraph::new(line), header_area);
    }

    draw_footer(f, app, footer_area);

    // welcome-экран: центрированная панель с рамкой + makedo-анимация (пустой лог)
    if app.log.is_empty() && !app.busy {
        draw_welcome(f, log_area, tick);
        draw_input(f, app, input_area, &input_rows, input_cursor);
        // dropdown /models или палитра — иначе на welcome-экране меню невидимо = блок
        if app.modal.is_open() {
            draw_modal(f, app, input_area);
        } else {
            draw_palette(f, app, input_area);
        }
        // permission поверх welcome тоже (иначе панель не видна = блок)
        draw_permission(f, app, screen_root);
        return;
    }

    // ЕДИНЫЙ КОНТЕНТНЫЙ КРАЙ (как в grok): вся периферия — root horizontal_margin(2).
    // Внутри log_area НИКАКИХ доп. отступов: текст, плашки, таблицы, рамка инпута,
    // статус и футер начинаются от col 0 log_area. Единственное исключение — `❯`
    // внутри рамки инпута (border+padding), что нормально: он визуально в боксе.
    let full = log_area.width as usize;
    let log_w = full; // контент занимает всю ширину log_area
    let mut lines: Vec<Line<'_>> = Vec::new();
    // sticky-заголовки (как в grok): каждой лог-строке — её «секция» (индекс вопроса
    // пользователя). Когда плашка вопроса уходит выше окна — пиним её к верху.
    let mut owner: Vec<i32> = Vec::new();
    let mut questions: Vec<String> = Vec::new();
    let mut head_logical: Vec<usize> = Vec::new(); // лог-строка текста плашки на вопрос
    let mut cur: i32 = -1;
    let (mut prev_tool, mut prev_user, mut first) = (false, false, true);
    for e in &app.log {
        let cur_tool = matches!(e, Entry::Tool { .. });
        let cur_user = matches!(e, Entry::User(_) | Entry::QueuedUser { .. });
        // воздух на границе «ответ модели ↔ список тул-коллов» (плашка User сама
        // добавляет зазоры сверху/снизу, поэтому её края не трогаем)
        if !first && prev_tool != cur_tool && !cur_user && !prev_user {
            lines.push(Line::raw(""));
        }
        first = false;
        prev_tool = cur_tool;
        prev_user = cur_user;
        while owner.len() < lines.len() {
            owner.push(cur);
        }
        match e {
            // вопрос пользователя — на выделенной плашке (bg на всю ширину)
            Entry::User(t) | Entry::QueuedUser { text: t, .. } => {
                let status = match e {
                    Entry::QueuedUser {
                        cancelled: true, ..
                    } => "cancelled",
                    Entry::QueuedUser { .. } => "queued",
                    _ => "",
                };
                cur = i32::try_from(questions.len()).unwrap_or(-1);
                questions.push(if status.is_empty() {
                    t.clone()
                } else {
                    format!("({status}) {t}")
                });
                // плашка: пустая bg-строка сверху и снизу + текст жирным светлым
                // на контрастном фоне (grok-стиль).
                let plate = Style::default().bg(theme::plate_bg());
                let pw = log_w;
                let blank = " ".repeat(pw);
                lines.push(Line::raw("")); // внешний зазор
                let top = if status.is_empty() {
                    blank.clone()
                } else {
                    format!(
                        "  {status}{}",
                        " ".repeat(pw.saturating_sub(status.len() + 2))
                    )
                };
                lines.push(Line::from(top).style(plate.fg(theme::dim())));
                // 2 пробела слева ВНУТРИ плашки: ❯ не липнет к краю фона и встаёт
                // ровно под ❯ поля ввода (граница+padding бокса тоже дают 2)
                head_logical.push(lines.len()); // следующая строка — текст плашки
                // Wrap the body before adding the prompt: every continuation
                // keeps the same text column and two cells of right padding.
                let (rows, _) = wrap_text(t, 0, log_area.width.saturating_sub(6));
                for (i, row) in rows.into_iter().enumerate() {
                    let prefix = if i == 0 { "  ❯ " } else { "    " };
                    let content = format!("{prefix}{row}");
                    let pad = pw.saturating_sub(Span::raw(content.as_str()).width());
                    lines.push(
                        Line::from(format!("{content}{}", " ".repeat(pad)))
                            .style(plate.fg(theme::strong()).add_modifier(Modifier::BOLD)),
                    );
                }
                lines.push(Line::from(blank).style(plate)); // нижний отступ
                lines.push(Line::raw("")); // внешний зазор
            }
            // ответ модели — рендерим как markdown (заголовки/списки/таблицы/bold)
            Entry::Assistant(t) => lines.extend(markdown::render(t, log_w)),
            Entry::Tool {
                name,
                status,
                preview,
                ..
            } => {
                let (mark, color) = match status {
                    ToolStatus::Queued => ("○", theme::dim()),
                    ToolStatus::Running => (
                        spinner::compact(spinner::resolve(spinner::ProcessKind::ToolRun), tick),
                        theme::cyan(),
                    ),
                    ToolStatus::Ok => ("✓", theme::green()),
                    ToolStatus::Failed => ("✗", theme::red()),
                };
                let head = format!("{mark} {name}");
                let mut spans = if *status == ToolStatus::Running {
                    vec![
                        Span::styled(format!("{mark} "), Style::default().fg(color)),
                        Span::styled(name.clone(), Style::default().fg(theme::fg())),
                    ]
                } else {
                    vec![Span::styled(head.clone(), Style::default().fg(color))]
                };
                if !preview.is_empty() {
                    // тул-строка — одна КОМПАКТНАЯ строка: первая строка вывода,
                    // усечённая до 72 симв. На широком терминале без этого превью
                    // тянется во всю ширину и хвосты вылезают «во flow ответов».
                    let first = preview.lines().next().unwrap_or_default();
                    let budget = log_w.saturating_sub(head.chars().count() + 3).min(72);
                    let one = truncate_chars(first.trim(), budget);
                    spans.push(Span::styled(
                        format!("  {one}"),
                        Style::default().fg(theme::dim()),
                    ));
                }
                lines.push(Line::from(spans));
            }
            Entry::Notice(t) => {
                lines.push(Line::styled(t.clone(), Style::default().fg(theme::green())));
            }
            Entry::Error(t) => {
                lines.push(Line::styled(t.clone(), Style::default().fg(theme::red())));
            }
        }
        while owner.len() < lines.len() {
            owner.push(cur);
        }
    }
    // Лог растёт СНИЗУ ВВЕРХ. Переносим строки САМИ (точный посимвольный wrap),
    // а не через ratatui Wrap: его WordWrapper по-разному считает пустые/пробельные
    // строки в line_count и в рендере → плашки рвались, по краям оставался мусор.
    // Свой wrap даёт ровно предсказуемое число рядов.
    let visible = log_area.height as usize;
    // переносим строки под ширину log_area; без доп. индента — единый левый край.
    // Попутно тянем секцию каждой строки и запоминаем wrapped-индекс плашек.
    let wrap_w = full.max(1);
    let mut wrapped: Vec<Line<'_>> = Vec::with_capacity(lines.len());
    let mut wrapped_owner: Vec<i32> = Vec::with_capacity(lines.len());
    let mut logical_to_wrapped: Vec<usize> = Vec::with_capacity(lines.len());
    for (i, l) in lines.iter().enumerate() {
        logical_to_wrapped.push(wrapped.len());
        for row in wrap_line(l, wrap_w) {
            wrapped.push(row);
            wrapped_owner.push(owner[i]);
        }
    }
    let head_wrapped: Vec<usize> = head_logical
        .iter()
        .map(|&li| logical_to_wrapped[li])
        .collect();
    let total = wrapped.len();
    let max_scroll = total.saturating_sub(visible);
    let scroll = app.scroll.min(max_scroll);
    let start = max_scroll - scroll; // scroll=0 → низ; растёт → вверх по истории
    let end = (start + visible).min(total);
    let mut view: Vec<Line<'_>> = wrapped[start..end].to_vec();
    // контента меньше экрана — доклеиваем пустые строки СВЕРХУ (лог прижат к низу)
    if view.len() < visible {
        let mut padded = vec![Line::raw(""); visible - view.len()];
        padded.append(&mut view);
        view = padded;
    } else if start < total {
        // STICKY: если верх окна принадлежит секции, чья плашка ушла выше окна —
        // пиним вопрос этой секции к самой верхней строке (grok-стиль).
        if let Ok(s) = usize::try_from(wrapped_owner[start])
            && head_wrapped[s] < start
        {
            let plate = Style::default().bg(theme::plate_bg());
            let q = truncate_chars(&format!("  ❯ {}", questions[s]), log_w);
            let pad = log_w.saturating_sub(q.chars().count());
            if let Some(top) = view.first_mut() {
                *top = Line::from(format!("{q}{}", " ".repeat(pad)))
                    .style(plate.fg(theme::strong()).add_modifier(Modifier::BOLD));
            }
        }
    }
    // без block padding: единый левый край, перенос — свой (wrap ВЫКЛ).
    // базовый fg = корпусный серый темы (немаркированный текст наследует его)
    f.render_widget(
        Paragraph::new(view).style(Style::default().fg(theme::fg())),
        log_area,
    );

    draw_todos(f, app, tick, todo_area);
    if !app.agents.is_empty() && !show_agents && todo_area.height > 0 {
        let running = app
            .agents
            .iter()
            .filter(|agent| agent.state == pipeline::State::Running)
            .count();
        f.render_widget(
            Paragraph::new(format!(
                "Agents: {running} running / {} total · F7 panel · /logs",
                app.agents.len()
            ))
            .style(Style::default().fg(theme::dim())),
            ratatui::layout::Rect {
                y: todo_area.bottom() - 1,
                height: 1,
                ..todo_area
            },
        );
    }

    let status = if app.permission.is_some() {
        // сам запрос рисуется центрированной модалкой (draw_permission), тут — метка
        Line::styled(
            "awaiting your approval…",
            Style::default().fg(theme::yellow()),
        )
    } else if app.busy {
        let sp = spinner::resolve(app.process_kind());
        let seconds = app
            .turn_started
            .map_or(0, |start| start.elapsed().as_secs());
        let elapsed = if seconds < 60 {
            format!("{seconds}s")
        } else {
            format!("{}m {:02}s", seconds / 60, seconds % 60)
        };
        let queued = app
            .log
            .iter()
            .filter(|entry| {
                matches!(
                    entry,
                    Entry::Tool {
                        status: ToolStatus::Queued,
                        ..
                    }
                )
            })
            .count();
        let mut suffix = format!(" · {elapsed}");
        if app.running_tools > 1 {
            let _ = write!(suffix, " · {} tools", app.running_tools);
        }
        if queued > 0 {
            let _ = write!(suffix, " · {queued} queued");
        }
        let phase = if app.phase.is_empty() {
            "working"
        } else {
            &app.phase
        };
        let budget = usize::from(status_area.width).saturating_sub(Span::raw(&suffix).width() + 2);
        Line::from(vec![
            Span::styled(
                format!("{} ", spinner::compact(sp, tick)),
                Style::default().fg(theme::cyan()),
            ),
            Span::styled(
                format!("{}{suffix}", pipeline::clip(phase, budget)),
                Style::default().fg(theme::dim()),
            ),
        ])
    } else {
        let status = app.last_usage.map_or_else(
            || "ready".to_owned(),
            |u| {
                let money = match (u.cost_usd, u.saved_usd) {
                    (Some(c), Some(s)) if s > 0.0005 => {
                        format!(", ${c:.4} (saved ${s:.4})")
                    }
                    (Some(c), _) => format!(", ${c:.4}"),
                    _ => String::new(),
                };
                format!(
                    "ready — {}in/{}out tok, cache {}%{money}",
                    u.input_tokens,
                    u.output_tokens,
                    u.cache_hit_percent()
                )
            },
        );
        Line::styled(status, Style::default().fg(theme::dim()))
    };
    // индикатор на едином левом крае (без доп. отступа)
    f.render_widget(Paragraph::new(status), status_area);

    draw_input(f, app, input_area, &input_rows, input_cursor);
    // dropdown /models — над input, как палитра команд (НЕ блокирующая модалка)
    if app.modal.is_open() {
        draw_modal(f, app, input_area);
    } else {
        draw_palette(f, app, input_area);
    }
    // permission-запрос — центрированная модалка поверх всего (не зажатая полоска)
    draw_permission(f, app, root);
}

/// Permission-запрос как центрированная панель: команда с переносом + опции.
fn draw_permission(f: &mut ratatui::Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    use ratatui::layout::Alignment;
    use ratatui::widgets::{Clear, Padding};
    let Some((_, desc)) = &app.permission else {
        return;
    };
    let pw = 76u16.min(area.width.saturating_sub(4));
    // высота: заголовок(1)+пусто(1)+команда(wrapped)+пусто(1)+опции(1) внутри
    // рамки(2)+padding(2). Считаем перенос честно, иначе строка [y]/[a]/[n]
    // уезжает за панель и пользователь не видит, что нажать.
    let inner_w = pw.saturating_sub(6).max(1) as usize;
    let wrapped = desc.chars().count().div_ceil(inner_w).max(1);
    #[allow(clippy::cast_possible_truncation)]
    let ph = (wrapped as u16 + 10).min(area.height);
    let panel = ratatui::layout::Rect {
        x: area.x + area.width.saturating_sub(pw) / 2,
        y: area.y + area.height.saturating_sub(ph) / 2,
        width: pw,
        height: ph,
    };
    let lines = vec![
        Line::styled("makedo wants to run:", Style::default().fg(theme::fg())),
        Line::raw(""),
        Line::styled(
            desc.clone(),
            Style::default()
                .fg(theme::strong())
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                "[y]",
                Style::default()
                    .fg(theme::green())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Enter: allow once   ", Style::default().fg(theme::fg())),
            Span::styled(
                "[a]",
                Style::default()
                    .fg(theme::blue())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" always   ", Style::default().fg(theme::fg())),
            Span::styled(
                "[A]",
                Style::default()
                    .fg(theme::yellow())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" allow all   ", Style::default().fg(theme::fg())),
            Span::styled(
                "[n]",
                Style::default()
                    .fg(theme::red())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" deny", Style::default().fg(theme::fg())),
        ]),
        Line::raw(""),
        Line::styled(
            if app.permission_input.is_empty() {
                "Type YOLO + Enter to allow everything".into()
            } else {
                format!(
                    "❯ {}",
                    pipeline::clip(&app.permission_input, inner_w.saturating_sub(2))
                )
            },
            Style::default().fg(theme::yellow()),
        ),
    ];
    f.render_widget(Clear, panel);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .alignment(Alignment::Left)
            .style(Style::default().bg(theme::bg()).fg(theme::fg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::yellow()))
                    .style(Style::default().bg(theme::bg()))
                    .padding(Padding::symmetric(2, 1))
                    .title("permission required"),
            ),
        panel,
    );
}

/// `/models` как dropdown над input (тот же вид, что палитра команд):
/// провайдеры → модели → скрытый ввод ключа. Не блокирующая центр-панель.
#[allow(clippy::too_many_lines)] // линейный матч по вариантам модалки
fn draw_modal(f: &mut ratatui::Frame<'_>, app: &App, input_area: ratatui::layout::Rect) {
    use app::Modal;
    use ratatui::widgets::Clear;
    let (title, rows): (&str, Vec<Line<'_>>) = match &app.modal {
        Modal::Providers { sel } => (
            "provider  ↑/↓ · Enter · Esc",
            app.providers
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    // ollama — keyless: показываем реальный статус (сколько моделей
                    // локально видно), а не «нужен ключ»
                    let hint = if p.kind == "ollama" {
                        if app.ollama.is_empty() {
                            "not running".to_owned()
                        } else {
                            format!("✓ {} models", app.ollama.len())
                        }
                    } else if let Some(bin) = p.kind.strip_suffix("-cli") {
                        if makedo_providers::cli_available(bin) {
                            "✓ installed".to_owned()
                        } else {
                            "not installed".to_owned()
                        }
                    } else if std::env::var(&p.env).is_ok()
                        || makedo_config::credentials::get(&p.env).is_some()
                    {
                        "✓ key".to_owned()
                    } else {
                        "needs key".to_owned()
                    };
                    row_line(i == *sel, &p.label, &hint)
                })
                .collect(),
        ),
        Modal::Models {
            provider,
            models,
            sel,
        } => {
            let p = &app.providers[*provider];
            let has = p.env.is_empty()
                || std::env::var(&p.env).is_ok()
                || makedo_config::credentials::get(&p.env).is_some();
            // явно говорим, что случится по Enter: switch или запрос ключа
            let next = if has {
                "  Enter → switch (key saved)"
            } else {
                "  Enter → next: paste API key"
            };
            let mut rows: Vec<Line<'_>> = models
                .iter()
                .enumerate()
                .map(|(i, m)| row_line(i == *sel, m, ""))
                .collect();
            rows.push(Line::styled(next, Style::default().fg(theme::dim())));
            ("model  ↑/↓ · Enter · Esc", rows)
        }
        Modal::ApiKey {
            provider,
            model,
            buf,
        } => {
            let p = &app.providers[*provider];
            (
                "paste API key  ·  Enter save · Esc cancel",
                vec![
                    Line::styled(
                        format!("  {} · {model}", p.label),
                        Style::default().fg(theme::fg()),
                    ),
                    Line::from(vec![
                        Span::styled(format!("  {} ▏", p.env), Style::default().fg(theme::blue())),
                        Span::styled(
                            if buf.is_empty() {
                                "paste it here and press Enter".to_owned()
                            } else {
                                "•".repeat(buf.chars().count())
                            },
                            if buf.is_empty() {
                                Style::default().fg(theme::dim())
                            } else {
                                Style::default()
                                    .fg(theme::blue())
                                    .add_modifier(Modifier::BOLD)
                            },
                        ),
                    ]),
                ],
            )
        }
        Modal::Themes { sel, .. } => (
            "theme  ↑/↓ live preview · Enter save · Esc cancel",
            theme::NAMES
                .iter()
                .enumerate()
                .map(|(i, name)| row_line(i == *sel, name, ""))
                .collect(),
        ),
        Modal::Doctrine { sel } => (
            "design doctrine  ↑/↓ · Enter apply · Esc cancel",
            makedo_config::Doctrine::choices()
                .iter()
                .enumerate()
                .map(|(i, (_, name, desc))| row_line(i == *sel, name, desc))
                .collect(),
        ),
        Modal::Effort { sel } => (
            "reasoning effort  ↑/↓ · Enter apply · Esc cancel",
            app.effort_levels
                .iter()
                .enumerate()
                .map(|(i, level)| {
                    row_line(
                        i == *sel,
                        level,
                        if Some(level) == app.effort.as_ref() {
                            "current"
                        } else {
                            ""
                        },
                    )
                })
                .collect(),
        ),
        Modal::Onboard { step, cmds } => (
            "setup  Enter — yes · Esc — skip",
            match step {
                0 => vec![
                    Line::styled(
                        "  Warm up the wiki now? A curator turn reads the project and \
                         fills .makedo/wiki (one model call, cheap).",
                        Style::default().fg(theme::fg()),
                    ),
                    Line::styled(
                        "  You can rerun anytime with /enrich [focus].",
                        Style::default().fg(theme::dim()),
                    ),
                ],
                _ => vec![
                    Line::styled(
                        format!(
                            "  Enable self-verification after edits? Detected: {}",
                            cmds.join(" && ")
                        ),
                        Style::default().fg(theme::fg()),
                    ),
                    Line::styled(
                        "  Saved to .makedo/config.toml [verify]; failures go back to the model.",
                        Style::default().fg(theme::dim()),
                    ),
                ],
            },
        ),
        Modal::None => return,
    };
    // геометрия dropdown как у палитры: над полем ввода
    #[allow(clippy::cast_possible_truncation)]
    let height = (rows.len() as u16 + 2).min(input_area.y.max(2));
    let selected = match &app.modal {
        Modal::Providers { sel } | Modal::Models { sel, .. } | Modal::Effort { sel } => *sel,
        _ => 0,
    };
    let offset = selected.saturating_sub(usize::from(height.saturating_sub(2)).saturating_sub(1));
    let area = ratatui::layout::Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(height),
        width: input_area.width,
        height,
    };
    f.render_widget(Clear, area);
    // фон дропдауна из темы (иначе Clear показывает дефолт терминала — тёмный
    // прямоугольник на светлой теме)
    f.render_widget(
        Paragraph::new(rows.into_iter().skip(offset).collect::<Vec<_>>())
            .style(Style::default().bg(theme::bg()).fg(theme::fg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::border()))
                    .style(Style::default().bg(theme::bg()))
                    .title(title),
            ),
        area,
    );
}

/// Строка списка модалки: маркер выбора + название + правый хинт.
fn row_line(selected: bool, name: &str, hint: &str) -> Line<'static> {
    let base = if selected {
        Style::default()
            .fg(theme::blue())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::fg())
    };
    let mark = if selected { "❯ " } else { "  " };
    let mut spans = vec![Span::styled(format!("{mark}{name}"), base)];
    if !hint.is_empty() {
        spans.push(Span::styled(
            format!("   {hint}"),
            Style::default().fg(theme::dim()),
        ));
    }
    Line::from(spans)
}

/// Footer-подсказки: жирная клавиша, серое действие, разделители `│` (как grok).
fn draw_footer(f: &mut ratatui::Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    let key = Style::default().add_modifier(Modifier::BOLD);
    let act = Style::default().fg(theme::dim());
    let sep = Span::styled("   │   ", Style::default().fg(theme::dim()));
    // при активном permission — подсказки именно про его выбор (иначе неясно)
    let hints: &[(&str, &str)] = if app.permission.is_some() {
        &[("y", "allow"), ("a", "always"), ("A", "all"), ("n", "deny")]
    } else if app.busy {
        &[
            ("Enter", "queue"),
            ("Ctrl+Enter", "send now"),
            ("Esc", "cancel"),
            ("Ctrl+C", "quit"),
        ]
    } else {
        &[
            ("Enter", "send"),
            ("/", "commands"),
            ("Esc", "cancel"),
            ("Ctrl+C", "quit"),
        ]
    };
    let mut spans = Vec::new();
    for (i, (k, a)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(sep.clone());
        }
        let key_style = if app.permission.is_some() {
            key.fg(theme::yellow())
        } else {
            key
        };
        spans.push(Span::styled((*k).to_owned(), key_style));
        spans.push(Span::styled(format!(":{a}"), act));
    }
    // футер на едином левом крае (без доп. отступа) — вровень с рамкой инпута
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Поле ввода: тонкая скруглённая рамка без заголовка, `❯` слева, а справа-снизу
/// на рамке — метка «модель · режим» (как в grok).
fn draw_input(
    f: &mut ratatui::Frame<'_>,
    app: &App,
    input_area: ratatui::layout::Rect,
    rows: &[String],
    cursor: (usize, u16),
) {
    use ratatui::widgets::{BorderType, Padding};
    let arrow = Style::default()
        .fg(theme::blue())
        .add_modifier(Modifier::BOLD);
    let visible_rows = usize::from(input_area.height.saturating_sub(2));
    let first = cursor.0.saturating_sub(visible_rows.saturating_sub(1));
    // Keep the prompt and wrapped continuation lines aligned, even when scrolled.
    let content: Vec<Line<'_>> = rows
        .iter()
        .skip(first)
        .take(visible_rows)
        .enumerate()
        .map(|(i, l)| {
            let prefix = if i == 0 {
                Span::styled("❯ ", arrow)
            } else {
                Span::raw("  ")
            };
            Line::from(vec![prefix, Span::raw(l.as_str())])
        })
        .collect();
    // метка модель · режим — как TITLE на нижней грани рамки, справа.
    let mode = if app.allow_all { " · YOLO" } else { "" };
    let effort = app
        .effort
        .as_ref()
        .map_or_else(String::new, |value| format!(" · effort {value}"));
    let label = format!("{}{effort}{mode}", app.model);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::border()))
        .padding(Padding::horizontal(1));
    if !label.is_empty() {
        block = block.title_bottom(
            Line::from(format!(" {label} "))
                .right_aligned()
                .style(Style::default().fg(theme::dim())),
        );
    }
    f.render_widget(Paragraph::new(content).block(block), input_area);

    // курсор: (строка, колонка) по позиции в input; base col = border(1)+padding(1)+2.
    if !app.modal.is_open() && app.permission.is_none() && visible_rows > 0 && input_area.width > 6
    {
        let cursor_x = input_area.x + 4 + cursor.1;
        let cursor_y = input_area.y + 1 + u16::try_from(cursor.0 - first).unwrap_or(0);
        let max_x = input_area.right().saturating_sub(2);
        let max_y = input_area.bottom().saturating_sub(2);
        let pos = (cursor_x.min(max_x), cursor_y.min(max_y));
        f.set_cursor_position(pos);
        if app.own_window
            && let Some(cell) = f.buffer_mut().cell_mut(pos)
        {
            cell.modifier.insert(Modifier::REVERSED);
        }
    }
}

/// Soft-wrap display text and locate its char-indexed cursor without changing it.
/// Ratatui's graphemes and cell widths keep Cyrillic, CJK and emoji in sync
/// with the actual renderer. The editor cursor remains a Unicode char index.
fn wrap_text(input: &str, cursor: usize, width: u16) -> (Vec<String>, (usize, u16)) {
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    let mut position = (0, 0);
    let mut char_index = 0;
    for line in input.split('\n') {
        let mut row = String::new();
        let mut col = 0;
        let span = Span::raw(line);
        for grapheme in span.styled_graphemes(Style::default()) {
            let cells = Span::raw(grapheme.symbol).width();
            if col > 0 && col + cells > width {
                rows.push(std::mem::take(&mut row));
                col = 0;
            }
            let end = char_index + grapheme.symbol.chars().count();
            if (char_index..end).contains(&cursor) {
                position = (rows.len(), col);
            }
            row.push_str(grapheme.symbol);
            col += cells;
            char_index = end;
        }
        if cursor == char_index {
            position = (rows.len(), col);
        }
        rows.push(row);
        char_index += 1; // Explicit newline between logical lines.
    }
    (
        rows,
        (
            position.0,
            u16::try_from(position.1.min(width)).unwrap_or(0),
        ),
    )
}

/// ASCII-лого MAKEDO (4×N).
const MAKEDO_ART: &[&str] = &[
    "▗▖  ▗▖  ▗▄▖  ▗▖ ▗▖ ▗▄▄▄▖ ▗▄▄▄   ▗▄▖ ",
    "▐▛▚▞▜▌ ▐▌ ▐▌ ▐▌▗▞▘ ▐▌    ▐▌  █ ▐▌ ▐▌",
    "▐▌  ▐▌ ▐▛▀▜▌ ▐▛▚▖  ▐▛▀▀▘ ▐▌  █ ▐▌ ▐▌",
    "▐▌  ▐▌ ▐▌ ▐▌ ▐▌ ▐▌ ▐▙▄▄▖ ▐▙▄▄▀ ▝▚▄▞▘",
];

/// Welcome-экран: центрированная панель с рамкой; по логотипу бежит волна с затухающим следом.
fn draw_welcome(f: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, tick: u64) {
    use ratatui::layout::Alignment;
    use ratatui::widgets::Padding;

    // центрируем панель фиксированного размера внутри area
    let pw = 52u16.min(area.width.saturating_sub(2));
    let ph = 13u16.min(area.height);
    let panel = area.centered(Constraint::Length(pw), Constraint::Length(ph));

    // анимация: позиция головы волны, бегущей по ширине лого
    let art_w = MAKEDO_ART.first().map_or(0, |l| l.chars().count());
    let head = if art_w == 0 {
        0
    } else {
        #[allow(clippy::cast_possible_truncation)]
        {
            (tick as usize) % (art_w + 8)
        }
    };
    let mut lines: Vec<Line<'_>> = Vec::new();
    lines.push(Line::raw(""));
    for row in MAKEDO_ART {
        let mut spans = Vec::new();
        for (i, ch) in row.chars().enumerate() {
            // расстояние от головы волны → яркость (trail затухает позади)
            let dist = head.saturating_sub(i);
            let style = if ch == ' ' {
                Style::default()
            } else if i == head || dist == 0 {
                Style::default()
                    .fg(theme::strong())
                    .add_modifier(Modifier::BOLD)
            } else if dist <= 3 {
                Style::default()
                    .fg(theme::blue())
                    .add_modifier(Modifier::BOLD)
            } else if dist <= 7 {
                Style::default().fg(theme::blue())
            } else {
                Style::default().fg(theme::dim())
            };
            spans.push(Span::styled(ch.to_string(), style));
        }
        lines.push(Line::from(spans).alignment(Alignment::Center));
    }
    lines.push(Line::raw(""));
    lines.push(
        Line::styled(
            format!("makedo {}", env!("CARGO_PKG_VERSION")),
            Style::default()
                .fg(theme::fg())
                .add_modifier(Modifier::BOLD),
        )
        .alignment(Alignment::Center),
    );
    lines.push(
        Line::styled("make do — type to begin", Style::default().fg(theme::dim()))
            .alignment(Alignment::Center),
    );
    lines.push(Line::raw(""));
    lines.push(
        Line::styled(
            "/ commands   ·   /help   ·   Ctrl+C quit",
            Style::default().fg(theme::dim()),
        )
        .alignment(Alignment::Center),
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::border()))
        .padding(Padding::symmetric(2, 1));
    f.render_widget(Paragraph::new(lines).block(block), panel);
}

/// Выпадающая палитра команд с фильтрацией: рисуется над input-полем.
fn draw_palette(f: &mut ratatui::Frame<'_>, app: &App, input_area: ratatui::layout::Rect) {
    use ratatui::widgets::Clear;
    if !app.palette_open() {
        return;
    }
    let items = crate::commands::Palette::matches(app.palette_filter());
    if items.is_empty() {
        return;
    }
    let mut sel = app.palette.selected;
    if sel >= items.len() {
        sel = items.len() - 1;
    }
    #[allow(clippy::cast_possible_truncation)]
    let height = (items.len() as u16 + 2).min(input_area.y.max(2)); // не вылезать за верх
    let area = ratatui::layout::Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(height),
        width: input_area.width,
        height,
    };
    let rows: Vec<Line<'_>> = items
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let c = m.cmd;
            let selected = i == sel;
            let base = if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::fg())
            };
            // подсветка совпавшей части имени синим (как в grok)
            let (s, e) = m.hl;
            let mut spans = vec![Span::styled(if selected { " ❯ /" } else { "   /" }, base)];
            spans.push(Span::styled(c.name[..s].to_owned(), base));
            spans.push(Span::styled(
                c.name[s..e].to_owned(),
                base.fg(theme::blue()).add_modifier(Modifier::BOLD),
            ));
            let tail = &c.name[e..];
            let name_tail = if c.arg_hint.is_empty() {
                tail.to_owned()
            } else {
                format!("{tail} {}", c.arg_hint)
            };
            spans.push(Span::styled(format!("{name_tail:<20}"), base));
            spans.push(Span::styled(
                c.description.to_owned(),
                Style::default().fg(theme::dim()),
            ));
            Line::from(spans)
        })
        .collect();
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(rows)
            .style(Style::default().bg(theme::bg()).fg(theme::fg()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::border()))
                    .style(Style::default().bg(theme::bg()))
                    .title("commands  (↑/↓ select · Tab next · Enter run · Esc close)"),
            ),
        area,
    );
}

/// План сессии: статусы + живая индикация исполнения (`in_progress` крутится).
fn draw_todos(f: &mut ratatui::Frame<'_>, app: &App, tick: u64, area: ratatui::layout::Rect) {
    if app.todos.is_empty() || area.height == 0 {
        return;
    }
    let state = if app.turn_failed {
        pipeline::State::Failed
    } else if app.busy {
        pipeline::State::Running
    } else {
        pipeline::State::Done
    };
    let line = pipeline::steps(&app.todos, usize::from(area.width), tick, state);
    f.render_widget(
        Paragraph::new(line),
        ratatui::layout::Rect { height: 1, ..area },
    );
}

/// Headless-режим: один промпт, события в stdout, exit-код по результату.
/// Нужен для dogfood/CI (exit-критерий M0) и скриптинга.
pub async fn run_headless(mut handle: SessionHandle, prompt: String) -> i32 {
    if handle
        .commands
        .send(SessionCommand::Prompt(prompt))
        .await
        .is_err()
    {
        return 1;
    }
    let mut stdout = std::io::stdout();
    let mut code = 0;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            SessionEvent::PermissionsChanged { allow_all: true } => {
                eprintln!("[permissions] YOLO: all tools allowed");
            }
            SessionEvent::TextDelta(t) => {
                let _ = stdout.write_all(t.as_bytes());
                let _ = stdout.flush();
            }
            SessionEvent::ToolLog {
                call_id,
                name,
                path,
            } => eprintln!("[log] {call_id} {name}: {path}"),
            SessionEvent::ToolStarted { name, .. } => eprintln!("[tool] {name}"),
            SessionEvent::ToolFinished { name, is_error, .. } => {
                eprintln!("[tool] {name} {}", if is_error { "failed" } else { "ok" });
            }
            SessionEvent::PermissionAsk {
                request_id,
                description,
            } => {
                // headless: политика «спросить» без терминала = отказ (безопасный дефолт)
                eprintln!("[permission denied: {description}]");
                let _ = handle
                    .commands
                    .send(SessionCommand::PermissionReply {
                        request_id,
                        approve: false,
                        always: false,
                    })
                    .await;
            }
            SessionEvent::Error(e) => {
                eprintln!("[error] {e}");
                code = 1;
            }
            SessionEvent::TurnFinished { cancelled, usage } => {
                let _ = stdout.write_all(b"\n");
                let money = match (usage.cost_usd, usage.saved_usd) {
                    (Some(c), Some(s)) => format!(" cost=${c:.4} saved=${s:.4}"),
                    // CLI-делегаты сообщают цену без разбивки экономии кэша
                    (Some(c), None) => format!(" cost=${c:.4}"),
                    _ => String::new(),
                };
                eprintln!(
                    "[usage] in={} out={} cache_read={} cache_write={} (hit {}%){money}",
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_read_tokens,
                    usage.cache_write_tokens,
                    usage.cache_hit_percent()
                );
                if cancelled {
                    code = 130;
                }
                break;
            }
            _ => {}
        }
    }
    code
}
