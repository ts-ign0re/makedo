//! Маппинг `процесс → спиннер` (PLAN §3.6).

use crate::loaders::{self, Spinner};

/// Виды процессов, у каждого — свой спиннер (требование: «используй разные»).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessKind {
    LlmStream,
    ToolRun,
    ToolBatch,
    Idle,
}

/// ASCII-fallback для терминалов без braille (старые окружения — наша аудитория).
pub const ASCII: Spinner = Spinner {
    name: "ascii",
    frames: &["|", "/", "-", "\\"],
    interval_ms: 100,
};

/// Терминал переваривает braille? Детект по локали + ручной флаг `MAKEDO_ASCII`.
#[must_use]
pub fn unicode_ok() -> bool {
    if std::env::var_os("MAKEDO_ASCII").is_some() {
        return false;
    }
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .is_none_or(|v| v.to_uppercase().contains("UTF"))
}

/// Спиннер процесса; ASCII, если терминал не тянет braille.
#[must_use]
pub fn resolve(kind: ProcessKind) -> Spinner {
    if !unicode_ok() {
        return ASCII;
    }
    match kind {
        ProcessKind::LlmStream => loaders::BRAILLEWAVE,
        ProcessKind::ToolRun => loaders::SNAKE,
        ProcessKind::ToolBatch => loaders::COLUMNS,
        ProcessKind::Idle => loaders::BREATHE,
    }
}

/// Кадр спиннера для монотонного счётчика тиков.
#[must_use]
pub fn frame(sp: Spinner, tick: u64) -> &'static str {
    let len = sp.frames.len().max(1);
    #[allow(clippy::cast_possible_truncation)] // индекс < len ≤ десятки
    sp.frames[(tick % len as u64) as usize]
}

/// Compact status/tool rows reserve exactly one terminal cell for animation.
pub fn compact(sp: Spinner, tick: u64) -> &'static str {
    let value = frame(sp, tick);
    if ratatui::text::Span::raw(value).width() == 1 {
        value
    } else {
        frame(loaders::BRAILLE, tick)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_process_gets_its_own_spinner() {
        let kinds = [
            ProcessKind::LlmStream,
            ProcessKind::ToolRun,
            ProcessKind::ToolBatch,
            ProcessKind::Idle,
        ];
        let names: std::collections::BTreeSet<&str> =
            kinds.iter().map(|k| resolve(*k).name).collect();
        assert_eq!(names.len(), kinds.len());
    }

    #[test]
    fn frame_cycles() {
        let sp = loaders::BRAILLE;
        assert_eq!(frame(sp, 0), sp.frames[0]);
        assert_eq!(frame(sp, sp.frames.len() as u64), sp.frames[0]);
    }
}
