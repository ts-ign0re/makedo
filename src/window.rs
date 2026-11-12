//! `makedo app`: тот же TUI в собственном окне (winit + wgpu), без эмулятора терминала.
//!
//! ponytail: прототип под macOS. Шрифты системные (`MAKEDO_FONT` — свой файл,
//! `MAKEDO_FONT_SIZE` — кегль в пунктах), вставка через `pbpaste`, выделения текста
//! мышью нет. Другие ОС и свой шрифт в бандле — когда окно станет каналом поставки.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::Context as _;
use makedo_tui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
};
use makedo_tui::ratatui::Terminal;
use ratatui_wgpu::shaders::AspectPreservingDefaultPostProcessor;
use ratatui_wgpu::{Builder, Dimensions, Font, WgpuBackend, wgpu};
use tokio::sync::mpsc::{Receiver, Sender, channel};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement as _;
use winit::window::{Window, WindowId};

/// Текст рисует системный Menlo; свой шрифт — `MAKEDO_FONT`.
const FONT: &str = "/System/Library/Fonts/Menlo.ttc";
/// В Menlo нет брайля (спиннеры) и `⎇`. Их добирает `JuliaMono` из бандла: он тоже
/// моноширинный и почти той же ширины, поэтому клетка не сужается и символы выходят
/// в полный размер. Другие запасные шрифты не годятся: бэкенд берёт ширину клетки как
/// минимум по всем шрифтам, а символы пропорциональных рисует вполовину.
/// `MAKEDO_SYMBOLS_FONT` — свой файл (и для запуска без бандла).
const BUNDLED_SYMBOLS_FONT: &str = "../Resources/JuliaMono-Regular.ttf";

/// ponytail: шрифты живут весь процесс — течь в 'static проще владеющей обёртки.
fn load_font(path: &std::path::Path) -> Option<Font<'static>> {
    Font::new(Box::leak(std::fs::read(path).ok()?.into_boxed_slice()))
}

/// Именно aspect-preserving: обычный постпроцессор растягивает текст на всё окно без
/// сглаживания, и при размере окна, не кратном клетке, у букв срезает строки пикселей.
/// Этот выводит пиксель в пиксель, остаток по краям остаётся фоном.
type Backend = WgpuBackend<'static, 'static, AspectPreservingDefaultPostProcessor>;
type Session = (
    makedo_session::SessionHandle,
    makedo_config::Config,
    bool,
    bool,
);

pub fn run(args: &[String]) -> anyhow::Result<()> {
    let (dir, _worktree_lease) = crate::work_dir(args)?;
    let rt = crate::runtime()?;
    let session = {
        let _in_runtime = rt.enter();
        crate::build_session(&dir)?
    };
    let mut app = App {
        rt,
        session: Some(session),
        tx: None,
        size: Arc::new(AtomicU64::new(0)),
        repaint: Arc::new(AtomicBool::new(false)),
        mods: ModifiersState::empty(),
        scroll_px: 0.0,
        line_px: 1.0,
        error: None,
    };
    EventLoop::new()?.run_app(&mut app)?;
    app.error.map_or(Ok(()), Err)
}

struct App {
    rt: tokio::runtime::Runtime,
    /// Отдаётся потоку TUI при создании окна.
    session: Option<Session>,
    tx: Option<Sender<Event>>,
    /// Ожидающий ресайз поверхности в пикселях: `w << 32 | h`, 0 — нет.
    size: Arc<AtomicU64>,
    /// Система просит перерисовать окно целиком (показали, вышло из-под другого окна).
    repaint: Arc<AtomicBool>,
    mods: ModifiersState,
    /// Накопленная прокрутка тачпада; одно событие скролла = 3 строки.
    scroll_px: f64,
    line_px: f64,
    error: Option<anyhow::Error>,
}

impl App {
    fn open(&mut self, el: &ActiveEventLoop, session: Session) -> anyhow::Result<()> {
        let window = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("makedo")
                    .with_inner_size(LogicalSize::new(1100.0, 720.0)),
            )?,
        );
        let pt = std::env::var("MAKEDO_FONT_SIZE")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(14.0);
        let font_px = (pt * window.scale_factor()).round().max(6.0);
        self.line_px = font_px * 1.2;

        // поверхность создаём здесь, на главном потоке (Metal требует этого на macOS);
        // сам бэкенд не Send — его собирает поток TUI
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance.create_surface(window.clone())?;

        let (tx, rx) = channel(256);
        self.tx = Some(tx);
        let (rt, size) = (self.rt.handle().clone(), self.size.clone());
        let repaint = self.repaint.clone();
        std::thread::spawn(move || {
            let gpu = Gpu {
                instance,
                surface,
                font_px,
                repaint,
            };
            let code = match rt.block_on(tui(gpu, rx, size, window, session)) {
                Ok(()) => 0, // TUI вышел (/quit, Ctrl+C) — закрываем приложение
                Err(e) => {
                    eprintln!("makedo app: {e:#}");
                    1
                }
            };
            std::process::exit(code);
        });
        Ok(())
    }

    fn send(&self, ev: Event) {
        if let Some(tx) = &self.tx {
            // очередь полна = TUI завис; ввод в этот момент всё равно некому читать
            let _ = tx.try_send(ev);
        }
    }
}

/// То, что создано на главном потоке и уезжает в поток TUI.
struct Gpu {
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    font_px: f64,
    repaint: Arc<AtomicBool>,
}

/// Цикл TUI на своём потоке: главный занят окном.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // кегль в px — малое положительное
async fn tui(
    gpu: Gpu,
    rx: Receiver<Event>,
    size: Arc<AtomicU64>,
    window: Arc<Window>,
    (mut handle, cfg, needs_setup, global_data): Session,
) -> anyhow::Result<()> {
    let path = std::env::var("MAKEDO_FONT").unwrap_or_else(|_| FONT.to_owned());
    let font = load_font(path.as_ref())
        .with_context(|| format!("{path} is not a usable font; set MAKEDO_FONT"))?;
    let symbols = std::env::var_os("MAKEDO_SYMBOLS_FONT")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            Some(
                std::env::current_exe()
                    .ok()?
                    .parent()?
                    .join(BUNDLED_SYMBOLS_FONT),
            )
        })
        .and_then(|path| load_font(&path));

    if let Some(theme) = &cfg.tui.theme {
        makedo_tui::theme::set(theme);
    }
    let px = window.inner_size();
    // порядок важен: список бэкенд пробует раньше шрифта из from_font, поэтому
    // основной идёт в нём первым, символьный — за ним
    let backend: Backend = Builder::from_font(font.clone())
        .with_regular_fonts(std::iter::once(font).chain(symbols))
        .with_font_size_px(gpu.font_px as u32)
        .with_width_and_height(Dimensions {
            width: NonZeroU32::new(px.width).context("window has zero width")?,
            height: NonZeroU32::new(px.height).context("window has zero height")?,
        })
        .with_bg_color(makedo_tui::theme::bg())
        .with_fg_color(makedo_tui::theme::fg())
        // без ожидания vsync: рисуем только по событию, а очередь кадров Fifo добавляла
        // к каждому нажатию до пары кадров задержки
        .with_present_mode(wgpu::PresentMode::AutoNoVsync)
        .with_instance(gpu.instance)
        .build_with_surface(gpu.surface)
        .await?;
    let mut terminal = Terminal::new(backend)?;

    let mut title = String::new();
    let host = makedo_tui::Host {
        terminal: &mut terminal,
        events: makedo_tui::events_from(rx),
        before_draw: Box::new(move |terminal, new_title| {
            // ratatui шлёт бэкенду только изменившиеся клетки; clear заставит отдать все
            if gpu.repaint.swap(false, Ordering::Relaxed) {
                let _ = terminal.clear();
            }
            let s = size.swap(0, Ordering::Relaxed);
            if s != 0 {
                terminal.backend_mut().resize((s >> 32) as u32, s as u32);
            }
            if new_title != title {
                window.set_title(new_title);
                new_title.clone_into(&mut title);
            }
        }),
        own_window: true,
    };
    makedo_tui::run_on(
        host,
        &mut handle,
        &cfg.tui,
        needs_setup,
        global_data,
        cfg.provider.model.clone(),
        cfg.skills.doctrine.as_str().to_owned(),
    )
    .await?;
    Ok(())
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        // resumed может прийти повторно — окно создаём один раз
        let Some(session) = self.session.take() else {
            return;
        };
        if let Err(e) = self.open(el, session) {
            self.error = Some(e);
            el.exit();
        }
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            // как закрытие вкладки терминала: сессия пишется по ходу, ждать нечего
            WindowEvent::CloseRequested => std::process::exit(0),
            WindowEvent::Resized(s) => {
                self.size.store(
                    u64::from(s.width) << 32 | u64::from(s.height),
                    Ordering::Relaxed,
                );
                self.send(Event::Resize(0, 0)); // размер возьмёт бэкенд, это только «перерисуй»
            }
            WindowEvent::RedrawRequested | WindowEvent::Occluded(false) => {
                self.repaint.store(true, Ordering::Relaxed);
                self.send(Event::Resize(0, 0));
            }
            WindowEvent::Focused(true) => self.send(Event::FocusGained),
            WindowEvent::Focused(false) => self.send(Event::FocusLost),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::MouseWheel { delta, .. } => {
                self.scroll_px += match delta {
                    MouseScrollDelta::LineDelta(_, y) => f64::from(y) * 3.0 * self.line_px,
                    MouseScrollDelta::PixelDelta(p) => p.y,
                };
                let step = 3.0 * self.line_px;
                while self.scroll_px.abs() >= step {
                    let up = self.scroll_px > 0.0;
                    self.scroll_px -= if up { step } else { -step };
                    self.send(Event::Mouse(MouseEvent {
                        kind: if up {
                            MouseEventKind::ScrollUp
                        } else {
                            MouseEventKind::ScrollDown
                        },
                        column: 0,
                        row: 0,
                        modifiers: KeyModifiers::empty(),
                    }));
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let mods = self.mods;
                if mods.super_key() {
                    // Cmd+V — вставка; остальные Cmd-сочетания оставляем меню (Cmd+Q и т.п.)
                    if matches!(&event.logical_key, Key::Character(c) if c.as_str() == "v")
                        && let Ok(out) = std::process::Command::new("pbpaste").output()
                    {
                        self.send(Event::Paste(
                            String::from_utf8_lossy(&out.stdout).into_owned(),
                        ));
                    }
                    return;
                }
                // с Ctrl/Alt нужна сама клавиша, а не то, что раскладка из неё делает (⌥D → ∂)
                let key = if mods.control_key() || mods.alt_key() {
                    event.key_without_modifiers()
                } else {
                    event.logical_key
                };
                for code in key_codes(&key, mods.shift_key()) {
                    self.send(Event::Key(KeyEvent::new(code, key_mods(mods))));
                }
            }
            _ => {}
        }
    }
}

fn key_mods(mods: ModifiersState) -> KeyModifiers {
    let mut out = KeyModifiers::empty();
    out.set(KeyModifiers::SHIFT, mods.shift_key());
    out.set(KeyModifiers::CONTROL, mods.control_key());
    out.set(KeyModifiers::ALT, mods.alt_key());
    out
}

/// Клавиша winit → коды crossterm, которые понимает `handle_key`.
fn key_codes(key: &Key, shift: bool) -> Vec<KeyCode> {
    let named = |n: &NamedKey| {
        Some(match n {
            NamedKey::Enter => KeyCode::Enter,
            NamedKey::Backspace => KeyCode::Backspace,
            NamedKey::Delete => KeyCode::Delete,
            NamedKey::Escape => KeyCode::Esc,
            NamedKey::Tab if shift => KeyCode::BackTab,
            NamedKey::Tab => KeyCode::Tab,
            NamedKey::Space => KeyCode::Char(' '),
            NamedKey::ArrowUp => KeyCode::Up,
            NamedKey::ArrowDown => KeyCode::Down,
            NamedKey::ArrowLeft => KeyCode::Left,
            NamedKey::ArrowRight => KeyCode::Right,
            NamedKey::Home => KeyCode::Home,
            NamedKey::End => KeyCode::End,
            NamedKey::PageUp => KeyCode::PageUp,
            NamedKey::PageDown => KeyCode::PageDown,
            // только те F-клавиши, что слушает TUI
            NamedKey::F6 => KeyCode::F(6),
            NamedKey::F7 => KeyCode::F(7),
            _ => return None,
        })
    };
    match key {
        Key::Named(n) => named(n).into_iter().collect(),
        Key::Character(s) => s.chars().map(KeyCode::Char).collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_to_what_the_tui_handles() {
        assert_eq!(
            key_codes(&Key::Named(NamedKey::Enter), false),
            [KeyCode::Enter]
        );
        assert_eq!(
            key_codes(&Key::Named(NamedKey::Tab), true),
            [KeyCode::BackTab]
        );
        assert_eq!(
            key_codes(&Key::Character("ж".into()), false),
            [KeyCode::Char('ж')]
        );
        assert!(key_codes(&Key::Named(NamedKey::F1), false).is_empty());
        let mods = key_mods(ModifiersState::CONTROL | ModifiersState::SHIFT);
        assert_eq!(mods, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
    }
}
