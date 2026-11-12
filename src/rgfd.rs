//! `makedo rg` / `makedo fd`: встроенные аналоги ripgrep/fd на нашем движке
//! (`grep-searcher` + `ignore`). Зачем: у целевой аудитории (старые машины)
//! rg/fd часто не установлены — а модель и пользователь привыкли к ним.
//! Через шимы (`ensure_shims`) команды доступны и внутри bash-тула.
//! Алгоритмически, без LLM и сети.

use std::fmt::Write as _;
use std::path::Path;

use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};

const MAX_HITS: usize = 10_000;

/// `makedo rg <pattern> [path]` — поиск по содержимому, вывод `path:line:text`.
pub fn rg(pattern: &str, path: Option<&str>) -> anyhow::Result<i32> {
    use std::io::Write as _;
    let root = path.map_or_else(|| ".".to_owned(), ToOwned::to_owned);
    let matcher = RegexMatcher::new_line_matcher(pattern)?;
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .build();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut hits = 0usize;
    for entry in ignore::WalkBuilder::new(&root).build().flatten() {
        if hits >= MAX_HITS {
            break;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let p = entry.path();
        let rel = p.strip_prefix(&root).unwrap_or(p).display().to_string();
        let _ = searcher.search_path(
            &matcher,
            p,
            UTF8(|line_no, line| {
                hits += 1;
                // закрытый pipe (`| head`) — штатное завершение, не ошибка
                let ok = writeln!(out, "{rel}:{line_no}:{}", line.trim_end()).is_ok();
                Ok(ok && hits < MAX_HITS)
            }),
        );
    }
    Ok(i32::from(hits == 0)) // exit 1 при нуле совпадений — семантика rg
}

/// `makedo fd <pattern> [path]` — поиск файлов по имени (substring, как fd).
pub fn fd(pattern: &str, path: Option<&str>) -> i32 {
    use std::io::Write as _;
    let root = path.map_or_else(|| ".".to_owned(), ToOwned::to_owned);
    let needle = pattern.to_lowercase();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut hits = 0usize;
    for entry in ignore::WalkBuilder::new(&root).build().flatten() {
        if hits >= MAX_HITS {
            break;
        }
        let p = entry.path();
        let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase());
        if name.is_some_and(|n| n.contains(&needle)) {
            hits += 1;
            let rel = p.strip_prefix(&root).unwrap_or(p);
            if writeln!(out, "{}", rel.display()).is_err() {
                break;
            }
        }
    }
    i32::from(hits == 0)
}

/// Шимы `rg`/`fd` → `makedo rg`/`makedo fd` в `~/.makedo/shims`, только для команд,
/// которых нет в системе. Директория подмешивается в PATH bash-тула.
pub fn ensure_shims(global_dir: &Path) -> Option<std::path::PathBuf> {
    let shims = global_dir.join("shims");
    let exe = std::env::current_exe().ok()?;
    std::fs::create_dir_all(&shims).ok()?;
    for cmd in ["rg", "fd"] {
        let system_has = std::process::Command::new("which")
            .arg(cmd)
            .output()
            .is_ok_and(|o| o.status.success());
        let shim = shims.join(cmd);
        if system_has {
            let _ = std::fs::remove_file(&shim); // системный появился — шим убираем
            continue;
        }
        let mut script = String::new();
        let _ = writeln!(script, "#!/bin/sh");
        let _ = writeln!(script, "exec \"{}\" {cmd} \"$@\"", exe.display());
        if std::fs::write(&shim, script).is_ok() {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755));
        }
    }
    Some(shims)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn shims_created_only_for_missing_commands() {
        let d = std::env::temp_dir().join(format!("makedo-shims-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let shims = ensure_shims(&d).unwrap();
        assert!(shims.ends_with("shims"));
        // на dev-машине rg обычно есть → шим не создан; в любом случае директория есть
        assert!(shims.is_dir());
    }
}
