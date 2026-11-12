#![allow(
    clippy::doc_markdown,
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::single_char_pattern,
    clippy::unwrap_used
)]
//! Рендер-тесты через `TestBackend`: снимаем РЕАЛЬНЫЙ буфер экрана и проверяем,
//! что элементы UI видны и в границах. Зелёные логик-тесты этого не ловят —
//! только буфер показывает, что нарисовано на самом деле.
#![cfg(test)]

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::app::App;
use crate::draw;

/// Рендерит app в буфер WxH и возвращает его как текст (строки склеены `\n`).
fn render(app: &App, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| draw(f, app, 0)).unwrap();
    let buf = term.backend().buffer();
    (0..h)
        .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn permission_panel_shows_all_options_and_command() {
    let mut app = App::default();
    // длинная команда с переносом — именно тут опции раньше уезжали за панель
    app.permission = Some((
        1,
        "bash ls -la /Users/user/_work/poker && cat /Users/user/_work/poker/README.md 2>/dev/null | head -50".to_owned(),
    ));
    let screen = render(&app, 120, 40);
    assert!(
        screen.contains("permission required"),
        "нет заголовка панели"
    );
    assert!(
        screen.contains("makedo wants to run"),
        "нет строки-приглашения"
    );
    // КРИТИЧНО: все четыре опции реально нарисованы (раньше [n] уезжал за панель)
    assert!(screen.contains("[y]"), "опция [y] не видна:\n{screen}");
    assert!(screen.contains("allow once"), "текст allow once не виден");
    assert!(screen.contains("[a]"), "опция [a] не видна:\n{screen}");
    assert!(screen.contains("always"), "текст always не виден");
    assert!(
        screen.contains("[A]"),
        "опция [A] allow-all не видна:\n{screen}"
    );
    assert!(screen.contains("allow all"), "текст allow all не виден");
    assert!(screen.contains("[n]"), "опция [n] не видна:\n{screen}");
    assert!(screen.contains("deny"), "текст deny не виден");
    // footer при permission подсказывает выбор
    assert!(
        screen.contains("y") && screen.contains("allow"),
        "footer без y:allow"
    );
}

#[test]
fn assistant_renders_markdown_and_user_plate() {
    let mut app = App::default();
    app.busy = true;
    app.log.push(crate::app::Entry::User("мой вопрос".into()));
    app.log
        .push(crate::app::Entry::Assistant("## Заголовок\n- пункт".into()));
    let screen = render(&app, 100, 25);
    assert!(screen.contains("мой вопрос"), "вопрос не виден");
    assert!(screen.contains("Заголовок"), "markdown-заголовок не виден");
    assert!(screen.contains("•"), "markdown-список (•) не виден");
    assert!(
        !screen.contains("## Заголовок"),
        "markdown ## не должен быть сырым"
    );
}

#[test]
fn header_shows_when_set() {
    let mut app = App::default();
    app.busy = true;
    app.header = "main ~/proj".into();
    app.log.push(crate::app::Entry::Assistant("x".into()));
    let screen = render(&app, 100, 25);
    assert!(screen.contains("main ~/proj"), "git-шапка не видна");
    assert!(screen.contains('⎇'), "нет символа ветки");
}

#[test]
fn scroll_shows_earlier_lines() {
    let mut app = App::default();
    app.busy = true;
    for i in 0..60 {
        app.log
            .push(crate::app::Entry::Assistant(format!("line{i}")));
    }
    let bottom = render(&app, 100, 40);
    assert!(bottom.contains("line59"), "низ должен показывать последнюю");
    assert!(
        !bottom.contains("line0 "),
        "при scroll=0 самых ранних не видно"
    );
    app.scroll = 40;
    let scrolled = render(&app, 100, 40);
    assert!(
        scrolled.contains("line10"),
        "скролл вверх должен показать ранние строки"
    );
    assert!(!scrolled.contains("line59"), "при скролле низ уходит");
}

#[test]
fn margin_zone_is_blank() {
    // левые 2 колонки (зона margin) должны быть пустыми — иначе артефакты
    let mut app = App::default();
    app.busy = true;
    app.log.push(crate::app::Entry::Assistant("X".repeat(200)));
    let screen = render(&app, 100, 20);
    for row in screen.lines() {
        let chars: Vec<char> = row.chars().collect();
        let left: String = chars.iter().take(2).collect();
        let right: String = chars.iter().rev().take(2).collect();
        assert!(
            left.trim().is_empty(),
            "левая margin-зона не пуста: {left:?}"
        );
        assert!(
            right.trim().is_empty(),
            "правая margin-зона не пуста: {right:?}"
        );
    }
}

#[test]
fn log_grows_bottom_up() {
    let mut app = App::default();
    app.busy = true; // не welcome-ветка
    app.log.push(crate::app::Entry::Assistant("FIRST".into()));
    app.log.push(crate::app::Entry::Assistant("LAST".into()));
    let screen = render(&app, 100, 30);
    let rows: Vec<&str> = screen.lines().collect();
    // FIRST должна быть ВЫШЕ LAST, и обе — в нижней части (не в первых строках)
    let first_y = rows.iter().position(|r| r.contains("FIRST")).unwrap();
    let last_y = rows.iter().position(|r| r.contains("LAST")).unwrap();
    assert!(first_y < last_y, "FIRST должна быть выше LAST");
    assert!(
        first_y > 10,
        "контент должен быть прижат к низу, а не к верху ({first_y})"
    );
}

#[test]
fn welcome_shows_logo_and_hints() {
    let app = App::default();
    let screen = render(&app, 120, 40);
    assert!(screen.contains("makedo 0.1.0"), "нет версии в welcome");
    assert!(
        screen.contains("make do — type to begin"),
        "нет подписи welcome"
    );
    // footer виден
    assert!(screen.contains("send"), "нет footer Enter:send");
}

#[test]
fn input_has_prompt_arrow() {
    let app = App::default();
    let screen = render(&app, 120, 40);
    assert!(
        screen.contains('❯'),
        "нет стрелки-приглашения ❯ в поле ввода"
    );
}

#[test]
fn tool_output_truncated_to_one_row() {
    // многострочный вывод тула НЕ должен заливать лог — только 1-я строка, усечённо
    let mut app = App::default();
    app.busy = true;
    app.log.push(crate::app::Entry::Tool {
        call_id: "test-call".into(),
        name: "bash".into(),
        status: crate::app::ToolStatus::Ok,
        preview: "first line of output\nSECRET_SECOND\nSECRET_THIRD".into(),
    });
    let screen = render(&app, 100, 20);
    assert!(screen.contains("bash"), "имя тула не видно");
    assert!(
        screen.contains("first line"),
        "первая строка вывода не видна"
    );
    assert!(
        !screen.contains("SECRET_SECOND") && !screen.contains("SECRET_THIRD"),
        "последующие строки вывода не должны попадать в лог"
    );
}

#[test]
fn user_plate_has_vertical_padding() {
    // плашка вопроса: bg-фон покрывает строку текста И по одной строке сверху/снизу
    let mut app = App::default();
    app.busy = true;
    app.log.push(crate::app::Entry::User("вопрос".into()));
    let backend = TestBackend::new(100, 20);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| draw(f, &app, 0)).unwrap();
    let buf = term.backend().buffer();
    let plate = crate::theme::plate_bg();
    // строка считается «плашкой», если середина экрана окрашена в plate bg
    let is_plate = |y: u16| buf[(40, y)].bg == plate;
    let plate_rows: Vec<u16> = (0..20).filter(|&y| is_plate(y)).collect();
    assert!(
        plate_rows.len() >= 3,
        "плашка должна занимать ≥3 строк (верх.отступ+текст+низ.отступ), а не {}",
        plate_rows.len()
    );
    // строки идут подряд
    assert!(
        plate_rows.windows(2).all(|w| w[1] == w[0] + 1),
        "плашка-строки должны быть смежными: {plate_rows:?}"
    );
}

#[test]
fn wrapped_user_message_keeps_text_indent_and_plate_padding() {
    for width in [40, 80] {
        for message in [
            format!("{}\nNEXT\n\nEND", "Я".repeat(usize::from(width) * 2)),
            format!("{}\nEND", "界👩‍💻".repeat(25)),
        ] {
            let mut app = App::default();
            app.log.push(crate::app::Entry::User(message));
            let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
            terminal.draw(|f| draw(f, &app, 0)).unwrap();
            let buffer = terminal.backend().buffer();
            let plate: Vec<_> = (0..40)
                .filter(|&y| buffer[(2, y)].bg == crate::theme::plate_bg())
                .collect();
            assert!(plate.len() > 3, "message must wrap into several rows");
            let first = plate[1];
            assert_eq!(buffer[(4, first)].symbol(), "❯");
            for &y in &plate {
                for x in [2, 3, width - 4, width - 3] {
                    assert_eq!(
                        buffer[(x, y)].symbol(),
                        " ",
                        "plate padding at ({x}, {y}), width={width}"
                    );
                }
                if y > first {
                    for x in [4, 5] {
                        assert_eq!(
                            buffer[(x, y)].symbol(),
                            " ",
                            "continuation must align below text, width={width}"
                        );
                    }
                }
            }
            let end = (0..40).find(|&y| buffer[(6, y)].symbol() == "E").unwrap();
            assert_eq!(buffer[(7, end)].symbol(), "N");
            assert_eq!(buffer[(8, end)].symbol(), "D");
        }
    }
}

#[test]
fn queued_messages_show_immediately_and_delivery_does_not_duplicate_or_merge_replies() {
    use crate::app::Entry;
    use makedo_types::event::SessionEvent;
    let mut app = App::default();
    app.user_prompt("original".into());
    app.apply(SessionEvent::PhaseChanged("Model".into()));
    app.apply(SessionEvent::TextDelta("working".into()));
    for id in ["a", "b"] {
        app.apply(SessionEvent::PromptQueued {
            id: id.into(),
            text: "same queued message".into(),
        });
    }
    app.apply(SessionEvent::TextDelta(" still".into()));
    let screen = render(&app, 100, 40);
    assert_eq!(screen.matches("same queued message").count(), 2);
    assert!(screen.contains("Ctrl+Enter") && screen.contains("send now"));
    assert!(matches!(&app.log[1], Entry::Assistant(text) if text == "working still"));
    app.apply(SessionEvent::PromptAccepted {
        text: "same queued message".into(),
        queued_id: Some("a".into()),
    });
    app.apply(SessionEvent::PhaseChanged("Model".into()));
    app.apply(SessionEvent::TextDelta("new reply".into()));
    app.apply(SessionEvent::PromptCancelled { id: "b".into() });
    assert!(matches!(&app.log[2], Entry::User(text) if text == "same queued message"));
    assert!(matches!(
        &app.log[3],
        Entry::QueuedUser {
            cancelled: true,
            ..
        }
    ));
    assert!(matches!(app.log.last(), Some(Entry::Assistant(text)) if text == "new reply"));
    assert_eq!(app.log.len(), 5);
    assert!(render(&app, 100, 40).contains("cancelled"));
}

#[tokio::test]
async fn control_enter_forces_a_draft_or_flushes_the_inbox() {
    use crossterm::event::{KeyCode, KeyModifiers};
    use makedo_session::{SessionCommand, SessionHandle};
    let (commands, mut received) = tokio::sync::mpsc::channel(4);
    let (_events, events) = tokio::sync::mpsc::channel(4);
    let mut handle = SessionHandle { commands, events };
    let mut app = App::default();
    app.busy = true;
    app.insert_str("urgent change");
    crate::handle_key(KeyCode::Enter, KeyModifiers::CONTROL, &mut app, &mut handle).await;
    assert!(
        matches!(received.recv().await, Some(SessionCommand::Steer(text)) if text == "urgent change")
    );
    assert!(app.input.is_empty());
    crate::handle_key(KeyCode::Enter, KeyModifiers::CONTROL, &mut app, &mut handle).await;
    assert!(matches!(received.recv().await, Some(SessionCommand::Steer(text)) if text.is_empty()));
    app.insert_str("ordinary change");
    crate::handle_key(KeyCode::Enter, KeyModifiers::NONE, &mut app, &mut handle).await;
    assert!(
        matches!(received.recv().await, Some(SessionCommand::Prompt(text)) if text == "ordinary change")
    );
    crate::handle_key(KeyCode::Enter, KeyModifiers::SHIFT, &mut app, &mut handle).await;
    assert_eq!(app.input, "\n");
    assert!(received.try_recv().is_err());
}

#[test]
fn models_dropdown_lists_providers() {
    let mut app = App::default();
    app.providers =
        crate::commands::provider_choices(&makedo_providers::catalog::Catalog::default());
    app.modal = crate::app::Modal::Providers { sel: 0 };
    let screen = render(&app, 120, 40);
    assert!(screen.contains("Anthropic"), "провайдер Anthropic не виден");
    assert!(screen.contains("DeepSeek"), "провайдер DeepSeek не виден");
}

#[tokio::test]
async fn effort_picker_sends_selection_and_displays_only_confirmed_value() {
    use crate::app::Modal;
    use makedo_types::event::SessionEvent;
    let mut app = App::default();
    app.apply(SessionEvent::ModelChanged {
        model: "qwen3.8".into(),
        effort_levels: vec!["low".into(), "medium".into(), "xhigh".into()],
        effort: Some("low".into()),
    });
    let (commands, mut received) = tokio::sync::mpsc::channel(4);
    let (_events, events) = tokio::sync::mpsc::channel(4);
    let mut handle = makedo_session::SessionHandle { commands, events };
    crate::handle_slash("effort", &mut app, &mut handle).await;
    assert!(matches!(app.modal, Modal::Effort { sel: 0 }));
    let screen = render(&app, 110, 24);
    assert!(
        screen.contains("reasoning effort")
            && screen.contains("xhigh")
            && screen.contains("medium")
    );
    assert_eq!(app.effort_levels, ["low", "medium", "xhigh"]);
    app.modal = Modal::Effort { sel: 2 };
    crate::handle_modal_key(crossterm::event::KeyCode::Enter, &mut app, &mut handle).await;
    assert!(
        matches!(received.recv().await, Some(makedo_session::SessionCommand::SetEffort(value)) if value == "xhigh")
    );
    assert_eq!(app.effort.as_deref(), Some("low"));
    app.apply(SessionEvent::ModelChanged {
        model: "qwen3.8".into(),
        effort_levels: app.effort_levels.clone(),
        effort: Some("xhigh".into()),
    });
    assert!(render(&app, 110, 24).contains("effort xhigh"));
    crate::handle_slash("effort medium", &mut app, &mut handle).await;
    assert!(
        matches!(received.recv().await, Some(makedo_session::SessionCommand::SetEffort(value)) if value == "medium")
    );
    app.apply(SessionEvent::ModelChanged {
        model: "sonnet".into(),
        effort_levels: Vec::new(),
        effort: None,
    });
    crate::handle_slash("effort", &mut app, &mut handle).await;
    assert!(matches!(app.modal, Modal::None));
    assert!(!render(&app, 110, 24).contains("effort xhigh"));
}

#[test]
fn custom_provider_and_selected_model_remain_visible_in_short_terminal() {
    let custom = serde_json::from_str(r#"{"qwen-vast":{"api":"http://127.0.0.1:8000/v1","env":["QWEN_VAST_TOKEN"],"models":{"qwen3.8":{"context":65536,"output":8192,"in":0,"out":0,"tools":true}}}}"#).unwrap();
    let mut app = App::default();
    app.providers = crate::commands::provider_choices(&custom);
    let idx = app
        .providers
        .iter()
        .position(|p| p.kind == "qwen-vast")
        .unwrap();
    assert_eq!(app.providers[idx].env, "QWEN_VAST_TOKEN");
    app.modal = crate::app::Modal::Providers { sel: idx };
    assert!(render(&app, 100, 16).contains("❯ qwen-vast"));
    let mut models: Vec<_> = (0..30).map(|i| format!("model-{i}")).collect();
    models.push("qwen3.8".to_owned());
    app.modal = crate::app::Modal::Models {
        provider: idx,
        models,
        sel: 30,
    };
    assert!(render(&app, 100, 16).contains("❯ qwen3.8"));
}

#[test]
fn everything_shares_one_left_edge() {
    // ЕДИНЫЙ левый край (grok-модель): фон плашки, ответ, статус, футер, рамка
    // инпута — на col 2. Стрелки-приглашения `❯` (в плашке И в инпуте) — на col 4,
    // т.к. обе визуально в «контейнере» с внутренним отступом.
    let mut app = App::default();
    app.busy = true;
    app.log.push(crate::app::Entry::User("вопрос".into()));
    app.log.push(crate::app::Entry::Assistant("ответ".into()));
    let backend = TestBackend::new(80, 24);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| draw(f, &app, 0)).unwrap();
    let buf = term.backend().buffer();
    let plate = crate::theme::plate_bg();
    let text_col = |y: u16| (0..80u16).find(|&x| buf[(x, y)].symbol() != " ");
    let plate_col = |y: u16| (0..80u16).find(|&x| buf[(x, y)].bg == plate);
    // строка плашки: фон на col 2, а ❯ инсетнут на col 4 (не липнет к краю)
    let plate_text_y = (0..24u16)
        .find(|&y| plate_col(y) == Some(2) && text_col(y) == Some(4))
        .expect("нет строки плашки: bg@2 + ❯@4");
    // ответ модели — тоже на col 2
    let answer_y = (0..24u16)
        .find(|&y| {
            let row: String = (0..80).map(|x| buf[(x, y)].symbol()).collect();
            row.contains("ответ")
        })
        .unwrap();
    assert_eq!(text_col(answer_y), Some(2), "ответ не на col 2");
    // рамка инпута на том же col 2
    let input_border_y = (0..24u16)
        .find(|&y| buf[(2, y)].symbol() == "╭")
        .expect("нет рамки инпута на col 2");
    // футер тоже на col 2
    let footer_y = (0..24u16)
        .find(|&y| {
            let row: String = (0..80).map(|x| buf[(x, y)].symbol()).collect();
            row.contains("Enter")
        })
        .unwrap();
    assert_eq!(text_col(footer_y), Some(2), "футер не на col 2");
    assert!(
        plate_text_y < answer_y && answer_y < input_border_y,
        "порядок сломан: плашка→ответ→инпут"
    );
}

#[test]
fn theme_paints_background_and_blue_headings() {
    // фон темы залит по всему кадру, а markdown-заголовок — синим (grok)
    let mut app = App::default();
    app.busy = true;
    app.log
        .push(crate::app::Entry::Assistant("## Заголовок".into()));
    let backend = TestBackend::new(80, 20);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| draw(f, &app, 0)).unwrap();
    let buf = term.backend().buffer();
    // угловая ячейка (вне текста) — фон темы, а не дефолт терминала
    assert_eq!(buf[(0, 0)].bg, crate::theme::bg(), "фон темы не залит");
    // ячейка заголовка окрашена в HEADING
    let head_cell = (0..20u16).find_map(|y| {
        (0..80u16).find_map(|x| {
            (buf[(x, y)].symbol() == "З" && buf[(x, y)].fg == crate::theme::heading()).then_some(())
        })
    });
    assert!(head_cell.is_some(), "заголовок не окрашен в синий HEADING");
}

#[test]
fn theme_picker_lists_omarchy_themes() {
    let mut app = App::default();
    app.modal = crate::app::Modal::Themes {
        sel: 0,
        original: 0,
    };
    let backend = TestBackend::new(120, 40);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| draw(f, &app, 0)).unwrap();
    let buf = term.backend().buffer();
    let screen: String = (0..40)
        .map(|y| (0..120).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.contains("grok"), "нет grok в списке тем");
    assert!(screen.contains("tokyo-night"), "нет tokyo-night");
    assert!(screen.contains("gruvbox"), "нет gruvbox");
    assert!(
        screen.contains("live preview"),
        "нет подсказки live preview"
    );
    // фон дропдауна — из темы (иначе тёмный прямоугольник на светлой теме).
    // берём строку со словом grok и проверяем её середину.
    let grok_y = (0..40u16)
        .find(|&y| {
            (0..120)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .contains("grok")
        })
        .unwrap();
    assert_eq!(
        buf[(60, grok_y)].bg,
        crate::theme::bg(),
        "фон дропдауна не из темы"
    );
}

#[test]
fn blank_line_between_answer_and_tools() {
    // между ответом модели и списком тул-коллов — пустая строка (воздух)
    let mut app = App::default();
    app.busy = true;
    app.log
        .push(crate::app::Entry::Assistant("ANSWERLINE".into()));
    app.log.push(crate::app::Entry::Tool {
        call_id: "test-call".into(),
        name: "bash".into(),
        status: crate::app::ToolStatus::Ok,
        preview: "TOOLPREVIEW".into(),
    });
    let screen = render(&app, 100, 24);
    let rows: Vec<&str> = screen.lines().collect();
    let ans_y = rows.iter().position(|r| r.contains("ANSWERLINE")).unwrap();
    let tool_y = rows.iter().position(|r| r.contains("bash")).unwrap();
    assert!(
        tool_y > ans_y + 1,
        "нет пустой строки между ответом и тулом"
    );
    assert!(
        rows[ans_y + 1].trim().is_empty(),
        "строка после ответа не пуста"
    );
}

#[test]
fn sticky_question_pins_to_top_when_scrolled() {
    // длинный ответ: плашка вопроса уходит выше окна, но вопрос ПИНИТСЯ к верху
    // (grok-стиль), а внизу виден хвост ответа.
    let mut app = App::default();
    app.busy = true;
    app.log.push(crate::app::Entry::User("STICKYQ".into()));
    let ans = (0..40)
        .map(|i| format!("ans{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.log.push(crate::app::Entry::Assistant(ans));
    let screen = render(&app, 100, 20);
    let rows: Vec<&str> = screen.lines().collect();
    let q_y = rows
        .iter()
        .position(|r| r.contains("STICKYQ"))
        .expect("вопрос не запинен");
    let last_y = rows
        .iter()
        .rposition(|r| r.contains("ans39"))
        .expect("хвост ответа не виден");
    assert!(
        q_y <= 3,
        "запиненный вопрос должен быть у верхнего края ({q_y})"
    );
    assert!(q_y < last_y, "вопрос выше ответа");
    // при этом реальная плашка (её пустой bg-верх) НЕ в кадре — только пин-строка
    assert_eq!(
        rows.iter().filter(|r| r.contains("STICKYQ")).count(),
        1,
        "вопрос показан один раз (пин), без дубля плашки"
    );
}

#[test]
fn doctrine_picker_lists_choices() {
    let mut app = App::default();
    app.modal = crate::app::Modal::Doctrine { sel: 1 };
    let screen = render(&app, 120, 40);
    assert!(screen.contains("ddd"), "нет ddd");
    assert!(screen.contains("hexagonal"), "нет hexagonal");
    assert!(screen.contains("Domain-Driven"), "нет описания DDD");
}

#[test]
fn multiline_input_box_grows_and_shows_lines() {
    let mut app = App::default();
    app.busy = false;
    app.input = "first line\nsecond line".into();
    app.cursor = app.input.chars().count();
    let screen = render(&app, 100, 24);
    assert!(
        screen.contains("first line"),
        "первая строка ввода не видна"
    );
    assert!(
        screen.contains("second line"),
        "вторая строка ввода не видна"
    );
}

#[test]
fn long_input_wraps_and_cursor_tracks_terminal_cells() {
    use ratatui::backend::Backend;
    // 50 columns -> 40 text cells after margins, border, padding and prompt.
    for (text, column, first_cell) in [
        ("я".repeat(41), 1, "я"),
        (format!("{}界e\u{301}", "a".repeat(39)), 3, "界"),
        (format!("{}👩‍💻X", "a".repeat(39)), 3, "👩‍💻"),
    ] {
        let mut app = App::default();
        app.log.push(crate::app::Entry::Assistant("history".into()));
        app.insert_str(&text);
        let mut terminal = Terminal::new(TestBackend::new(50, 24)).unwrap();
        terminal.draw(|f| draw(f, &app, 0)).unwrap();
        let cursor = terminal.backend_mut().get_cursor_position().unwrap();
        let buffer = terminal.backend().buffer();
        let top = (0..24).find(|&y| buffer[(2, y)].symbol() == "╭").unwrap();
        let bottom = (top..24).find(|&y| buffer[(2, y)].symbol() == "╰").unwrap();
        assert_eq!(bottom - top, 3, "two content rows, plus borders");
        assert_eq!(buffer[(6, top + 2)].symbol(), first_cell);
        assert_eq!((cursor.x, cursor.y), (6 + column, top + 2));
        assert_eq!(
            app.input, text,
            "soft wraps must not change the submitted text"
        );
    }
}

#[test]
fn tall_draft_scrolls_to_cursor_resizes_and_shrinks_after_deletion() {
    use ratatui::backend::Backend;
    let mut app = App::default();
    app.log.push(crate::app::Entry::Assistant("history".into()));
    let draft = format!("HEAD\n{}\nTAIL", "длинный текст ".repeat(120));
    app.insert_str(&draft);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    for width in [80, 40, 120] {
        terminal.backend_mut().resize(width, 24);
        terminal
            .resize(ratatui::layout::Rect::new(0, 0, width, 24))
            .unwrap();
        terminal.draw(|f| draw(f, &app, 0)).unwrap();
        let cursor = terminal.backend_mut().get_cursor_position().unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer[(cursor.x - 1, cursor.y)].symbol(),
            "L",
            "cursor must follow the visible tail"
        );
        assert!(cursor.x < width - 2 && cursor.y < 22);
        let screen = render(&app, width, 24);
        assert!(screen.contains("TAIL") && screen.contains("Enter"));
        assert!(
            !screen.contains("HEAD"),
            "scroll the draft, not just clamp its cursor"
        );
    }
    app.cursor = 0;
    let screen = render(&app, 80, 24);
    assert!(screen.contains("HEAD") && !screen.contains("TAIL"));
    assert_eq!(app.input, draft);
    app.clear_input();
    app.insert_str("short");
    let screen = render(&app, 80, 24);
    let rows: Vec<_> = screen.lines().collect();
    let top = rows.iter().position(|row| row.contains('╭')).unwrap();
    let bottom = rows.iter().position(|row| row.contains('╰')).unwrap();
    assert_eq!(bottom - top, 2, "shrink back to one content row");
}

#[test]
fn pasted_api_key_stays_out_of_prompt_and_screen() {
    let mut app = App::default();
    app.input = "existing draft".into();
    app.modal = crate::app::Modal::ApiKey {
        provider: 0,
        model: "test".into(),
        buf: String::new(),
    };
    crate::handle_paste("secret-key-value\n", &mut app);
    assert_eq!(app.input, "existing draft");
    assert!(
        matches!(&app.modal, crate::app::Modal::ApiKey { buf, .. } if buf == "secret-key-value")
    );
    // ApiKey rendering requires a provider catalog; the ordinary prompt must never contain the key.
    app.modal = crate::app::Modal::None;
    assert!(!render(&app, 80, 24).contains("secret-key-value"));
}

#[test]
fn concurrent_same_name_tools_finish_their_own_rows() {
    use crate::app::{Entry, ToolStatus};
    use makedo_types::event::SessionEvent;
    let mut app = App::default();
    for id in ["first", "second"] {
        app.apply(SessionEvent::ToolStarted {
            call_id: id.into(),
            name: "read".into(),
        });
    }
    app.apply(SessionEvent::ToolFinished {
        call_id: "first".into(),
        name: "read".into(),
        is_error: false,
        preview: "first result".into(),
    });
    assert!(
        matches!(&app.log[0], Entry::Tool { call_id, status: ToolStatus::Ok, preview, .. } if call_id == "first" && preview == "first result")
    );
    assert!(
        matches!(&app.log[1], Entry::Tool { call_id, status: ToolStatus::Running, .. } if call_id == "second")
    );
}

#[test]
fn pipeline_groups_agents_and_keeps_current_step_visible() {
    use makedo_types::event::{SessionEvent, TodoEntry, TodoState};
    let mut app = App::default();
    for event in crate::pipeline::demo_events() {
        app.apply(event);
    }
    let screen = render(&app, 140, 36);
    assert!(screen.contains("AGENTS"), "{screen}");
    assert!(screen.contains("● code-review │"), "{screen}");
    assert!(screen.contains("└─ ✓ Анализ → ◐ Отчёт"), "{screen}");
    assert!(screen.contains("✓ UI-аудит │"), "{screen}");
    app.agent_selected = Some(1);
    app.apply(SessionEvent::AgentEvent {
        id: "backend".into(),
        event: Box::new(SessionEvent::ToolLog {
            call_id: "check".into(),
            name: "bash".into(),
            path: "/tmp/build.log".into(),
        }),
    });
    let expanded = render(&app, 140, 36);
    assert!(expanded.contains("makedo/backend"), "{expanded}");
    assert!(expanded.contains("/tmp/build.log"), "{expanded}");
    let narrow = render(&app, 80, 24);
    assert!(!narrow.contains("AGENTS"), "{narrow}");
    assert!(narrow.contains("Agents: 2 running / 3 total"), "{narrow}");
    app.agents[0].todos = (0..10)
        .map(|i| TodoEntry {
            content: if i == 8 {
                "当前步骤🦀".into()
            } else {
                format!("Очень длинный этап {i}")
            },
            status: match i.cmp(&8) {
                std::cmp::Ordering::Less => TodoState::Completed,
                std::cmp::Ordering::Equal => TodoState::InProgress,
                std::cmp::Ordering::Greater => TodoState::Pending,
            },
        })
        .collect();
    let line = crate::pipeline::steps(&app.agents[0].todos, 34, 0, crate::pipeline::State::Running);
    assert!(line.width() <= 34);
    assert!(line.to_string().contains("当前步骤"));
    app.apply(SessionEvent::TurnFinished {
        cancelled: true,
        usage: makedo_types::event::TurnUsage::default(),
    });
    let cancelled = render(&app, 140, 36);
    assert!(
        !cancelled.contains('◐'),
        "cancelled work still spins: {cancelled}"
    );
    for size in [(40, 12), (20, 8)] {
        let _ = render(&app, size.0, size.1);
    }
    if let Ok(path) = std::env::var("MAKEDO_RENDER_DUMP") {
        std::fs::write(path, expanded).unwrap();
    }
}

#[tokio::test]
async fn typing_yolo_in_permission_modal_enables_session_instead_of_single_approval() {
    use crossterm::event::{KeyCode, KeyModifiers};
    use makedo_types::event::SessionEvent;
    let (commands, mut received) = tokio::sync::mpsc::channel(8);
    let (_events, events) = tokio::sync::mpsc::channel(8);
    let mut handle = makedo_session::SessionHandle { commands, events };
    let mut app = App::default();
    app.model = "test-model".into();
    app.input = "draft preserved".into();
    app.apply(SessionEvent::PermissionAsk {
        request_id: 42,
        description: "write file".into(),
    });
    for c in "YOLO".chars() {
        crate::handle_key(KeyCode::Char(c), KeyModifiers::NONE, &mut app, &mut handle).await;
        assert!(
            received.try_recv().is_err(),
            "typing Y must not approve a single call"
        );
    }
    assert!(render(&app, 120, 32).contains("❯ YOLO"));
    crate::handle_key(KeyCode::Enter, KeyModifiers::NONE, &mut app, &mut handle).await;
    assert!(
        matches!(received.recv().await, Some(makedo_session::SessionCommand::Prompt(text)) if text == "YOLO")
    );
    app.apply(SessionEvent::PermissionsChanged { allow_all: true });
    assert!(app.permission.is_none() && app.allow_all);
    assert_eq!(app.input, "draft preserved");
    assert!(render(&app, 120, 32).contains("· YOLO"));
}

#[test]
fn busy_status_is_compact_and_completion_summary_stays_unchanged() {
    use makedo_types::event::{SessionEvent, TurnUsage};
    let mut app = App::default();
    app.apply(SessionEvent::PromptAccepted {
        text: "push changes".into(),
        queued_id: None,
    });
    app.apply(SessionEvent::ToolStarted {
        call_id: "bash".into(),
        name: "bash".into(),
    });
    app.turn_started = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(9));
    let screen = render(&app, 100, 24);
    let line = screen
        .lines()
        .find(|line| line.contains("bash · 9s"))
        .unwrap();
    assert!(line.trim().chars().count() <= 13, "{line}");
    assert!(!line.contains("Esc") && !line.contains('│'));
    app.apply(SessionEvent::ToolQueued {
        call_id: "next".into(),
        name: "read".into(),
    });
    assert!(render(&app, 100, 24).contains("1 queued"));
    app.apply(SessionEvent::TurnFinished {
        cancelled: false,
        usage: TurnUsage {
            input_tokens: 120,
            output_tokens: 30,
            cache_read_tokens: 60,
            ..TurnUsage::default()
        },
    });
    assert!(render(&app, 100, 24).contains("ready — 120in/30out tok, cache 50%"));
    let sp = crate::spinner::resolve(crate::spinner::ProcessKind::ToolRun);
    for tick in 0..20 {
        assert_eq!(
            ratatui::text::Span::raw(crate::spinner::compact(sp, tick)).width(),
            1
        );
    }
}
