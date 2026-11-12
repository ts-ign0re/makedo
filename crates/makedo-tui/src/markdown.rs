//! Минимальный markdown → ratatui `Line`. Самописный (pulldown-cmark тяжёлый,
//! нам нужны заголовки/bold/code/списки/таблицы/hr — ~150 строк). Ответ модели
//! рендерится читаемо, а не сырым текстом.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme;

/// Разбирает markdown-текст в стилизованные строки для лога шириной `width`.
#[must_use]
pub fn render(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut in_code_block = false;
    let mut table: Vec<String> = Vec::new(); // буфер строк таблицы `|...|`
    for raw in text.lines() {
        let line = raw;
        // ``` — переключение блока кода
        if line.trim_start().starts_with("```") {
            flush_table(&mut table, &mut out, width);
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            // подсветка синтаксиса + фон-панель, чтобы код визуально группировался
            out.push(highlight_code(line, width));
            continue;
        }
        let t = line.trim();
        // таблица: копим подряд идущие `|...|` строки, рендерим блоком при разрыве
        if t.starts_with('|') && t.contains('|') {
            table.push(t.to_owned());
            continue;
        }
        flush_table(&mut table, &mut out, width);
        // горизонтальная линия
        if t == "---" || t == "***" || t == "___" {
            out.push(Line::styled(
                "─".repeat(40),
                Style::default().fg(theme::border()),
            ));
            continue;
        }
        // заголовки #, ##, ### — лазурно-синий жирный (как в grok)
        if let Some(rest) = t.strip_prefix("### ") {
            out.push(Line::from(Span::styled(
                rest.to_owned(),
                Style::default()
                    .fg(theme::heading())
                    .add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if let Some(rest) = t.strip_prefix("## ") {
            out.push(Line::from(Span::styled(
                rest.to_owned(),
                Style::default()
                    .fg(theme::heading())
                    .add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if let Some(rest) = t.strip_prefix("# ") {
            out.push(Line::from(Span::styled(
                rest.to_owned(),
                Style::default()
                    .fg(theme::heading())
                    .add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        // маркированный список - / *
        if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            let indent = &line[..line.len() - line.trim_start().len()];
            let mut spans = vec![
                Span::raw(indent.to_owned()),
                Span::styled("• ", Style::default().fg(theme::blue())),
            ];
            spans.extend(inline(rest));
            out.push(Line::from(spans));
            continue;
        }
        // нумерованный список 1. 2.
        if let Some(dot) = t.find(". ")
            && t[..dot].chars().all(|c| c.is_ascii_digit())
            && !t[..dot].is_empty()
        {
            let mut spans = vec![Span::styled(
                format!("{}. ", &t[..dot]),
                Style::default().fg(theme::blue()),
            )];
            spans.extend(inline(&t[dot + 2..]));
            out.push(Line::from(spans));
            continue;
        }
        // обычный абзац — с инлайн-разметкой
        if line.is_empty() {
            out.push(Line::raw(""));
        } else {
            out.push(Line::from(inline(line)));
        }
    }
    flush_table(&mut table, &mut out, width);
    out
}

/// Сбрасывает накопленный буфер таблицы в отрендеренные строки (если он не пуст).
fn flush_table(table: &mut Vec<String>, out: &mut Vec<Line<'static>>, width: usize) {
    if table.is_empty() {
        return;
    }
    out.extend(render_table(&std::mem::take(table), width));
}

/// Рендерит markdown-таблицу в рамке с выровненными колонками (grok-стиль).
/// Ширину колонок ужимаем, чтобы влезть в `width`; длинные ячейки переносятся.
fn render_table(rows_raw: &[String], width: usize) -> Vec<Line<'static>> {
    // разбор ячеек; строку-разделитель |---|---| пропускаем
    let mut rows: Vec<Vec<String>> = Vec::new();
    for r in rows_raw {
        let cells: Vec<String> = r
            .trim()
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_owned())
            .collect();
        let is_sep = cells
            .iter()
            .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':')));
        if !is_sep {
            rows.push(cells);
        }
    }
    if rows.is_empty() {
        return Vec::new();
    }
    let ncols = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
    for r in &mut rows {
        while r.len() < ncols {
            r.push(String::new());
        }
    }
    // натуральная ширина колонок (в символах)
    let mut colw = vec![1usize; ncols];
    for r in &rows {
        for (c, cell) in r.iter().enumerate() {
            colw[c] = colw[c].max(cell.chars().count());
        }
    }
    // умещаем в доступную ширину: overhead = разделители + паддинги
    let overhead = 3 * ncols + 1;
    let avail = width.saturating_sub(overhead).max(ncols);
    while colw.iter().sum::<usize>() > avail {
        // ужимаем самую широкую колонку
        let mi = (0..ncols).max_by_key(|&i| colw[i]).unwrap_or(0);
        if colw[mi] <= 1 {
            break;
        }
        colw[mi] -= 1;
    }
    let border = Style::default().fg(theme::border());
    let mut out = Vec::new();
    out.push(border_line(&colw, "╭", "┬", "╮", border));
    for (ri, r) in rows.iter().enumerate() {
        // каждую ячейку переносим по ширине её колонки; высота ряда = макс
        let wrapped: Vec<Vec<String>> = r
            .iter()
            .enumerate()
            .map(|(c, cell)| wrap_cell(cell, colw[c]))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        let cell_style = if ri == 0 {
            Style::default()
                .fg(theme::strong())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::fg())
        };
        for h in 0..height {
            let mut spans = vec![Span::styled("│", border)];
            for (c, cw) in colw.iter().enumerate() {
                let piece = wrapped[c].get(h).cloned().unwrap_or_default();
                let pad = cw.saturating_sub(piece.chars().count());
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    format!("{piece}{}", " ".repeat(pad)),
                    cell_style,
                ));
                spans.push(Span::raw(" "));
                spans.push(Span::styled("│", border));
            }
            out.push(Line::from(spans));
        }
        if ri == 0 {
            out.push(border_line(&colw, "├", "┼", "┤", border)); // под заголовком
        }
    }
    out.push(border_line(&colw, "╰", "┴", "╯", border));
    out
}

/// Горизонтальная граница таблицы из `─` с угловыми/тройниковыми символами.
fn border_line(colw: &[usize], left: &str, mid: &str, right: &str, style: Style) -> Line<'static> {
    let mut s = String::from(left);
    for (i, w) in colw.iter().enumerate() {
        if i > 0 {
            s.push_str(mid);
        }
        s.push_str(&"─".repeat(w + 2));
    }
    s.push_str(right);
    Line::styled(s, style)
}

/// Посимвольный перенос содержимого ячейки под ширину `w`.
fn wrap_cell(cell: &str, w: usize) -> Vec<String> {
    if w == 0 {
        return vec![String::new()];
    }
    let chars: Vec<char> = cell.chars().collect();
    if chars.len() <= w {
        return vec![cell.to_owned()];
    }
    chars.chunks(w).map(|c| c.iter().collect()).collect()
}

/// Ключевые слова — объединение популярных языков (Rust/TS/JS/Python/Go/Java/C).
/// Язык не парсим: общий набор красит достаточно, чтобы код читался.
fn is_keyword(w: &str) -> bool {
    matches!(
        w,
        "fn" | "let"
            | "const"
            | "mut"
            | "pub"
            | "struct"
            | "enum"
            | "impl"
            | "trait"
            | "use"
            | "mod"
            | "self"
            | "super"
            | "crate"
            | "async"
            | "await"
            | "move"
            | "ref"
            | "dyn"
            | "where"
            | "match"
            | "if"
            | "else"
            | "for"
            | "while"
            | "loop"
            | "break"
            | "continue"
            | "return"
            | "in"
            | "as"
            | "type"
            | "static"
            | "unsafe"
            | "extern"
            | "function"
            | "var"
            | "val"
            | "class"
            | "def"
            | "import"
            | "from"
            | "export"
            | "default"
            | "public"
            | "private"
            | "protected"
            | "void"
            | "int"
            | "float"
            | "double"
            | "char"
            | "bool"
            | "string"
            | "true"
            | "false"
            | "null"
            | "nil"
            | "None"
            | "Some"
            | "Ok"
            | "Err"
            | "new"
            | "this"
            | "package"
            | "interface"
            | "implements"
            | "extends"
            | "throw"
            | "try"
            | "catch"
            | "finally"
            | "switch"
            | "case"
            | "yield"
            | "lambda"
            | "and"
            | "or"
            | "not"
            | "is"
            | "del"
            | "pass"
            | "with"
            | "elif"
            | "do"
            | "then"
    )
}

/// Число: начинается с цифры, дальше буквы/цифры/точка (int, float, `0xff`).
fn is_number(w: &str) -> bool {
    w.starts_with(|c: char| c.is_ascii_digit())
        && w.chars().all(|c| c.is_alphanumeric() || c == '.')
}

fn flush_word(word: &mut String, spans: &mut Vec<Span<'static>>, bg: Style) {
    if word.is_empty() {
        return;
    }
    let w = std::mem::take(word);
    let st = if is_keyword(&w) {
        bg.fg(theme::keyword())
    } else if is_number(&w) {
        bg.fg(theme::number())
    } else {
        bg.fg(theme::fg())
    };
    spans.push(Span::styled(w, st));
}

/// Подсветка одной строки кода: строки, комментарии, числа, ключевые слова — на
/// фоне-панели `CODE_BG` во всю ширину `width` (ровный прямоугольник, не рваный).
fn highlight_code(line: &str, width: usize) -> Line<'static> {
    let bg = Style::default().bg(theme::code_bg());
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let first_ns = chars.iter().position(|c| !c.is_whitespace());
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut word = String::new();
    let mut i = 0usize;
    while i < n {
        let c = chars[i];
        // строковые литералы "..." '...' `...`
        if matches!(c, '"' | '\'' | '`') {
            flush_word(&mut word, &mut spans, bg);
            let quote = c;
            let mut s = String::from(c);
            i += 1;
            while i < n {
                let ch = chars[i];
                s.push(ch);
                i += 1;
                if ch == '\\' && i < n {
                    s.push(chars[i]); // экранированный символ внутри строки
                    i += 1;
                } else if ch == quote {
                    break;
                }
            }
            spans.push(Span::styled(s, bg.fg(theme::string())));
            continue;
        }
        // line-комментарии: `//` везде, `#` только в начале строки и не `#[` (Rust-атрибут)
        let line_comment = (c == '/' && i + 1 < n && chars[i + 1] == '/')
            || (c == '#' && Some(i) == first_ns && chars.get(i + 1) != Some(&'['));
        if line_comment {
            flush_word(&mut word, &mut spans, bg);
            spans.push(Span::styled(
                chars[i..].iter().collect::<String>(),
                bg.fg(theme::comment()),
            ));
            break;
        }
        // идентификаторы/числа копим в слово
        if c.is_alphanumeric() || c == '_' || c == '.' {
            word.push(c);
            i += 1;
            continue;
        }
        // пунктуация/пробел — как есть (с фоном панели)
        flush_word(&mut word, &mut spans, bg);
        spans.push(Span::styled(c.to_string(), bg.fg(theme::fg())));
        i += 1;
    }
    flush_word(&mut word, &mut spans, bg);
    // добиваем строку фоном до полной ширины — панель кода ровным прямоугольником
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), bg));
    }
    Line::from(spans).style(bg)
}

/// Инлайн-разметка строки: `**bold**` и `` `code` `` → стилизованные спаны.
fn inline(s: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '`' => {
                flush(&mut buf, &mut spans);
                let mut code = String::new();
                for ch in chars.by_ref() {
                    if ch == '`' {
                        break;
                    }
                    code.push(ch);
                }
                spans.push(Span::styled(code, Style::default().fg(theme::blue())));
            }
            '*' if chars.peek() == Some(&'*') => {
                chars.next(); // второй *
                flush(&mut buf, &mut spans);
                let mut bold = String::new();
                while let Some(ch) = chars.next() {
                    if ch == '*' && chars.peek() == Some(&'*') {
                        chars.next();
                        break;
                    }
                    bold.push(ch);
                }
                spans.push(Span::styled(
                    bold,
                    Style::default()
                        .fg(theme::strong())
                        .add_modifier(Modifier::BOLD),
                ));
            }
            _ => buf.push(c),
        }
    }
    flush(&mut buf, &mut spans);
    spans
}

fn flush(buf: &mut String, spans: &mut Vec<Span<'static>>) {
    if !buf.is_empty() {
        spans.push(Span::raw(std::mem::take(buf)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn headings_bold_code_lists() {
        let md = "## Заголовок\n**жирный** и `код`\n- пункт\n1. первый\n| a | b |";
        let lines = render(md, 80);
        assert_eq!(plain(&lines[0]), "Заголовок");
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(plain(&lines[1]), "жирный и код");
        // bold-спан отдельно и жирный
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|s| s.content == "жирный" && s.style.add_modifier.contains(Modifier::BOLD))
        );
        assert!(plain(&lines[2]).starts_with("• "));
        assert!(plain(&lines[3]).starts_with("1. "));
        // таблица теперь рендерится в рамке: есть строка ячеек `│ a │ b │`
        assert!(
            lines
                .iter()
                .any(|l| plain(l).contains("│ a") && plain(l).contains('b')),
            "таблица не отрендерилась в рамке"
        );
    }

    #[test]
    fn table_renders_boxed_and_fits_width() {
        let md = "| Col | Value |\n|---|---|\n| a | 1 |\n| b | 2 |";
        let lines = render(md, 40);
        let joined: String = lines.iter().map(plain).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains('╭') && joined.contains('╯'),
            "нет рамки таблицы"
        );
        assert!(
            joined.contains("Col") && joined.contains("Value"),
            "нет заголовка"
        );
        assert!(
            joined.contains("│ a") && joined.contains("│ b"),
            "нет ячеек"
        );
        // каждая строка не шире width
        assert!(
            lines.iter().all(|l| plain(l).chars().count() <= 40),
            "таблица шире width"
        );
    }

    #[test]
    fn code_block_verbatim() {
        let lines = render("```\nlet x = 1;\n```", 80);
        assert_eq!(plain(&lines[0]).trim_end(), "let x = 1;");
    }

    #[test]
    fn code_block_highlights_tokens() {
        let lines = render("```rust\nlet s = \"hi\"; // note\n```", 80);
        let l = &lines[0];
        assert_eq!(plain(l).trim_end(), "let s = \"hi\"; // note");
        let colored = |txt: &str, col| {
            l.spans
                .iter()
                .any(|s| s.content == txt && s.style.fg == Some(col))
        };
        assert!(colored("let", theme::keyword()), "ключевое слово не синее");
        assert!(colored("\"hi\"", theme::string()), "строка не зелёная");
        assert!(colored("// note", theme::comment()), "комментарий не серый");
        // фон-панель на всей строке
        assert_eq!(l.style.bg, Some(theme::code_bg()), "нет фона панели кода");
    }
}
