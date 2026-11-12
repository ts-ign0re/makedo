//! Цветовые темы. Переключаемые в рантайме (`/theme <name>`): выбранная палитра
//! хранится в атомике-индексе (безопасно при миграции tokio-задачи между потоками,
//! в отличие от `thread_local`). Доступ — через функции-акцессоры `theme::bg()` и т.п.

use std::sync::atomic::{AtomicU8, Ordering};

use ratatui::style::Color;

/// Полная палитра UI. Один источник всех цветов.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub bg: Color,
    pub fg: Color,
    pub dim: Color,
    pub blue: Color,
    pub heading: Color,
    pub strong: Color,
    pub plate_bg: Color,
    pub strip_bg: Color,
    pub border: Color,
    pub green: Color,
    pub yellow: Color,
    pub red: Color,
    pub cyan: Color,
    pub code_bg: Color,
    pub keyword: Color,
    pub string: Color,
    pub number: Color,
    pub comment: Color,
}

/// makedo — ФИРМЕННАЯ дефолтная тема: synthwave/outrun-киберпанк. Глубокий индиго,
/// горячий розовый акцент, циан и фиолет.
pub static MAKEDO: Palette = Palette {
    bg: Color::Rgb(20, 16, 31),         // deep indigo
    fg: Color::Rgb(214, 207, 230),      // lilac-gray
    dim: Color::Rgb(138, 126, 166),     // muted violet (WCAG AA 5.0:1 на bg)
    blue: Color::Rgb(255, 93, 177),     // hot pink (главный акцент: ❯, ссылки, буллеты)
    heading: Color::Rgb(180, 142, 255), // violet (заголовки)
    strong: Color::Rgb(240, 235, 250),  // near-white lilac
    plate_bg: Color::Rgb(32, 26, 48),   // панель плашки
    strip_bg: Color::Rgb(25, 20, 38),
    border: Color::Rgb(58, 42, 82),    // dim violet
    green: Color::Rgb(126, 255, 196),  // mint (✓, notice)
    yellow: Color::Rgb(255, 204, 102), // amber (running, allow-all)
    red: Color::Rgb(255, 69, 96),      // neon red (ошибки, deny)
    cyan: Color::Rgb(54, 224, 240),    // cyan (спиннер)
    code_bg: Color::Rgb(16, 12, 26),
    keyword: Color::Rgb(180, 142, 255), // violet
    string: Color::Rgb(126, 255, 196),  // mint
    number: Color::Rgb(255, 204, 102),  // amber
    comment: Color::Rgb(138, 126, 166), // WCAG AA 5.15:1 на code_bg
};

/// grok — тёмная лазурно-синяя (снята с grok build).
pub static GROK: Palette = Palette {
    bg: Color::Rgb(13, 13, 16),
    fg: Color::Rgb(200, 200, 206),
    dim: Color::Rgb(112, 114, 122),
    blue: Color::Rgb(88, 152, 232),
    heading: Color::Rgb(96, 160, 240),
    strong: Color::Rgb(232, 232, 238),
    plate_bg: Color::Rgb(28, 30, 36),
    strip_bg: Color::Rgb(20, 21, 25),
    border: Color::Rgb(48, 50, 58),
    green: Color::Rgb(122, 186, 128),
    yellow: Color::Rgb(214, 182, 106),
    red: Color::Rgb(224, 108, 108),
    cyan: Color::Rgb(94, 176, 214),
    code_bg: Color::Rgb(22, 23, 28),
    keyword: Color::Rgb(120, 162, 242),
    string: Color::Rgb(152, 200, 142),
    number: Color::Rgb(220, 170, 120),
    comment: Color::Rgb(110, 114, 122),
};

// --- набор тем omarchy (github.com/basecamp/omarchy), канонические палитры ---

/// tokyo-night — тёмная сине-фиолетовая.
pub static TOKYO_NIGHT: Palette = Palette {
    bg: Color::Rgb(26, 27, 38),
    fg: Color::Rgb(192, 202, 245),
    dim: Color::Rgb(86, 95, 137),
    blue: Color::Rgb(122, 162, 247),
    heading: Color::Rgb(125, 207, 255),
    strong: Color::Rgb(200, 208, 247),
    plate_bg: Color::Rgb(36, 40, 59),
    strip_bg: Color::Rgb(31, 35, 53),
    border: Color::Rgb(41, 46, 66),
    green: Color::Rgb(158, 206, 106),
    yellow: Color::Rgb(224, 175, 104),
    red: Color::Rgb(247, 118, 142),
    cyan: Color::Rgb(125, 207, 255),
    code_bg: Color::Rgb(30, 32, 48),
    keyword: Color::Rgb(187, 154, 247),
    string: Color::Rgb(158, 206, 106),
    number: Color::Rgb(255, 158, 100),
    comment: Color::Rgb(86, 95, 137),
};

/// catppuccin — тёмная (Mocha).
pub static CATPPUCCIN: Palette = Palette {
    bg: Color::Rgb(30, 30, 46),
    fg: Color::Rgb(205, 214, 244),
    dim: Color::Rgb(108, 112, 134),
    blue: Color::Rgb(137, 180, 250),
    heading: Color::Rgb(137, 180, 250),
    strong: Color::Rgb(205, 214, 244),
    plate_bg: Color::Rgb(49, 50, 68),
    strip_bg: Color::Rgb(41, 44, 60),
    border: Color::Rgb(69, 71, 90),
    green: Color::Rgb(166, 227, 161),
    yellow: Color::Rgb(249, 226, 175),
    red: Color::Rgb(243, 139, 168),
    cyan: Color::Rgb(148, 226, 213),
    code_bg: Color::Rgb(24, 24, 37),
    keyword: Color::Rgb(203, 166, 247),
    string: Color::Rgb(166, 227, 161),
    number: Color::Rgb(250, 179, 135),
    comment: Color::Rgb(108, 112, 134),
};

/// catppuccin-latte — светлая.
pub static CATPPUCCIN_LATTE: Palette = Palette {
    bg: Color::Rgb(239, 241, 245),
    fg: Color::Rgb(76, 79, 105),
    dim: Color::Rgb(140, 143, 161),
    blue: Color::Rgb(30, 102, 245),
    heading: Color::Rgb(30, 102, 245),
    strong: Color::Rgb(48, 50, 68),
    plate_bg: Color::Rgb(204, 208, 218),
    strip_bg: Color::Rgb(230, 233, 239),
    border: Color::Rgb(188, 192, 204),
    green: Color::Rgb(64, 160, 43),
    yellow: Color::Rgb(223, 142, 29),
    red: Color::Rgb(210, 15, 57),
    cyan: Color::Rgb(23, 146, 153),
    code_bg: Color::Rgb(230, 233, 239),
    keyword: Color::Rgb(136, 57, 239),
    string: Color::Rgb(64, 160, 43),
    number: Color::Rgb(254, 100, 11),
    comment: Color::Rgb(140, 143, 161),
};

/// everforest — тёмная зелёно-тёплая.
pub static EVERFOREST: Palette = Palette {
    bg: Color::Rgb(45, 53, 59),
    fg: Color::Rgb(211, 198, 170),
    dim: Color::Rgb(133, 146, 137),
    blue: Color::Rgb(127, 187, 179),
    heading: Color::Rgb(167, 192, 128),
    strong: Color::Rgb(232, 222, 196),
    plate_bg: Color::Rgb(52, 63, 68),
    strip_bg: Color::Rgb(47, 56, 62),
    border: Color::Rgb(71, 82, 88),
    green: Color::Rgb(167, 192, 128),
    yellow: Color::Rgb(219, 188, 127),
    red: Color::Rgb(230, 126, 128),
    cyan: Color::Rgb(131, 192, 146),
    code_bg: Color::Rgb(39, 46, 51),
    keyword: Color::Rgb(230, 152, 117),
    string: Color::Rgb(167, 192, 128),
    number: Color::Rgb(214, 153, 182),
    comment: Color::Rgb(133, 146, 137),
};

/// gruvbox — тёмная ретро-тёплая.
pub static GRUVBOX: Palette = Palette {
    bg: Color::Rgb(40, 40, 40),
    fg: Color::Rgb(235, 219, 178),
    dim: Color::Rgb(146, 131, 116),
    blue: Color::Rgb(131, 165, 152),
    heading: Color::Rgb(250, 189, 47),
    strong: Color::Rgb(251, 241, 199),
    plate_bg: Color::Rgb(60, 56, 54),
    strip_bg: Color::Rgb(50, 48, 47),
    border: Color::Rgb(80, 73, 69),
    green: Color::Rgb(184, 187, 38),
    yellow: Color::Rgb(250, 189, 47),
    red: Color::Rgb(251, 73, 52),
    cyan: Color::Rgb(142, 192, 124),
    code_bg: Color::Rgb(29, 32, 33),
    keyword: Color::Rgb(251, 73, 52),
    string: Color::Rgb(184, 187, 38),
    number: Color::Rgb(211, 134, 155),
    comment: Color::Rgb(146, 131, 116),
};

/// kanagawa — тёмная приглушённая (Wave).
pub static KANAGAWA: Palette = Palette {
    bg: Color::Rgb(31, 31, 40),
    fg: Color::Rgb(220, 215, 186),
    dim: Color::Rgb(114, 113, 105),
    blue: Color::Rgb(126, 156, 216),
    heading: Color::Rgb(127, 180, 202),
    strong: Color::Rgb(233, 229, 204),
    plate_bg: Color::Rgb(42, 42, 55),
    strip_bg: Color::Rgb(37, 37, 48),
    border: Color::Rgb(54, 54, 70),
    green: Color::Rgb(152, 187, 108),
    yellow: Color::Rgb(230, 195, 132),
    red: Color::Rgb(228, 104, 118),
    cyan: Color::Rgb(122, 168, 159),
    code_bg: Color::Rgb(22, 22, 29),
    keyword: Color::Rgb(149, 127, 184),
    string: Color::Rgb(152, 187, 108),
    number: Color::Rgb(255, 160, 102),
    comment: Color::Rgb(114, 113, 105),
};

/// matte-black — почти монохромная тёмная (фирменная omarchy).
pub static MATTE_BLACK: Palette = Palette {
    bg: Color::Rgb(10, 10, 10),
    fg: Color::Rgb(208, 208, 208),
    dim: Color::Rgb(106, 106, 106),
    blue: Color::Rgb(200, 200, 200),
    heading: Color::Rgb(240, 240, 240),
    strong: Color::Rgb(245, 245, 245),
    plate_bg: Color::Rgb(26, 26, 26),
    strip_bg: Color::Rgb(20, 20, 20),
    border: Color::Rgb(51, 51, 51),
    green: Color::Rgb(160, 192, 160),
    yellow: Color::Rgb(200, 184, 144),
    red: Color::Rgb(208, 128, 128),
    cyan: Color::Rgb(160, 184, 192),
    code_bg: Color::Rgb(18, 18, 18),
    keyword: Color::Rgb(176, 176, 176),
    string: Color::Rgb(160, 176, 144),
    number: Color::Rgb(192, 160, 128),
    comment: Color::Rgb(106, 106, 106),
};

/// nord — тёмная холодная сине-серая.
pub static NORD: Palette = Palette {
    bg: Color::Rgb(46, 52, 64),
    fg: Color::Rgb(216, 222, 233),
    dim: Color::Rgb(76, 86, 106),
    blue: Color::Rgb(129, 161, 193),
    heading: Color::Rgb(136, 192, 208),
    strong: Color::Rgb(236, 239, 244),
    plate_bg: Color::Rgb(59, 66, 82),
    strip_bg: Color::Rgb(52, 59, 73),
    border: Color::Rgb(67, 76, 94),
    green: Color::Rgb(163, 190, 140),
    yellow: Color::Rgb(235, 203, 139),
    red: Color::Rgb(191, 97, 106),
    cyan: Color::Rgb(136, 192, 208),
    code_bg: Color::Rgb(39, 44, 54),
    keyword: Color::Rgb(129, 161, 193),
    string: Color::Rgb(163, 190, 140),
    number: Color::Rgb(180, 142, 173),
    comment: Color::Rgb(76, 86, 106),
};

/// osaka-jade — тёмная нефритово-зелёная (оригинал omarchy).
pub static OSAKA_JADE: Palette = Palette {
    bg: Color::Rgb(17, 28, 24),
    fg: Color::Rgb(195, 211, 204),
    dim: Color::Rgb(92, 114, 104),
    blue: Color::Rgb(107, 184, 150),
    heading: Color::Rgb(127, 208, 168),
    strong: Color::Rgb(220, 239, 230),
    plate_bg: Color::Rgb(24, 39, 31),
    strip_bg: Color::Rgb(19, 32, 26),
    border: Color::Rgb(44, 64, 52),
    green: Color::Rgb(107, 184, 150),
    yellow: Color::Rgb(208, 184, 120),
    red: Color::Rgb(216, 128, 128),
    cyan: Color::Rgb(111, 192, 176),
    code_bg: Color::Rgb(14, 24, 19),
    keyword: Color::Rgb(127, 208, 168),
    string: Color::Rgb(160, 200, 144),
    number: Color::Rgb(208, 168, 120),
    comment: Color::Rgb(92, 114, 104),
};

/// ristretto — тёмная кофейно-тёплая (Monokai Pro Ristretto).
pub static RISTRETTO: Palette = Palette {
    bg: Color::Rgb(44, 37, 37),
    fg: Color::Rgb(255, 241, 243),
    dim: Color::Rgb(148, 138, 139),
    blue: Color::Rgb(133, 218, 204),
    heading: Color::Rgb(249, 204, 108),
    strong: Color::Rgb(255, 241, 243),
    plate_bg: Color::Rgb(64, 56, 56),
    strip_bg: Color::Rgb(54, 46, 46),
    border: Color::Rgb(91, 83, 83),
    green: Color::Rgb(173, 218, 120),
    yellow: Color::Rgb(249, 204, 108),
    red: Color::Rgb(253, 104, 131),
    cyan: Color::Rgb(133, 218, 204),
    code_bg: Color::Rgb(36, 31, 31),
    keyword: Color::Rgb(253, 104, 131),
    string: Color::Rgb(173, 218, 120),
    number: Color::Rgb(243, 141, 112),
    comment: Color::Rgb(148, 138, 139),
};

/// rose-pine — тёмная приглушённо-розово-фиолетовая.
pub static ROSE_PINE: Palette = Palette {
    bg: Color::Rgb(25, 23, 36),
    fg: Color::Rgb(224, 222, 244),
    dim: Color::Rgb(110, 106, 134),
    blue: Color::Rgb(49, 116, 143),
    heading: Color::Rgb(196, 167, 231),
    strong: Color::Rgb(224, 222, 244),
    plate_bg: Color::Rgb(31, 29, 46),
    strip_bg: Color::Rgb(26, 24, 38),
    border: Color::Rgb(38, 35, 58),
    green: Color::Rgb(156, 207, 216),
    yellow: Color::Rgb(246, 193, 119),
    red: Color::Rgb(235, 111, 146),
    cyan: Color::Rgb(156, 207, 216),
    code_bg: Color::Rgb(20, 18, 31),
    keyword: Color::Rgb(196, 167, 231),
    string: Color::Rgb(246, 193, 119),
    number: Color::Rgb(235, 188, 186),
    comment: Color::Rgb(110, 106, 134),
};

/// Имена тем по индексу — фирменная `makedo` (дефолт) + grok + набор omarchy.
pub const NAMES: [&str; 13] = [
    "makedo",
    "grok",
    "tokyo-night",
    "catppuccin",
    "catppuccin-latte",
    "everforest",
    "gruvbox",
    "kanagawa",
    "matte-black",
    "nord",
    "osaka-jade",
    "ristretto",
    "rose-pine",
];
static PALETTES: [&Palette; 13] = [
    &MAKEDO,
    &GROK,
    &TOKYO_NIGHT,
    &CATPPUCCIN,
    &CATPPUCCIN_LATTE,
    &EVERFOREST,
    &GRUVBOX,
    &KANAGAWA,
    &MATTE_BLACK,
    &NORD,
    &OSAKA_JADE,
    &RISTRETTO,
    &ROSE_PINE,
];
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// Переключает тему по имени; `false`, если имя неизвестно.
pub fn set(name: &str) -> bool {
    if let Some(i) = NAMES.iter().position(|n| n.eq_ignore_ascii_case(name)) {
        #[allow(clippy::cast_possible_truncation)]
        CURRENT.store(i as u8, Ordering::Relaxed);
        true
    } else {
        false
    }
}

/// Имя текущей темы.
#[must_use]
pub fn name() -> &'static str {
    NAMES[CURRENT.load(Ordering::Relaxed) as usize]
}

#[must_use]
fn cur() -> &'static Palette {
    PALETTES[CURRENT.load(Ordering::Relaxed) as usize]
}

// Акцессоры — код обращается к цветам через них, а не к константам.
macro_rules! accessors {
    ($($field:ident),* $(,)?) => {$(
        #[must_use]
        pub fn $field() -> Color {
            cur().$field
        }
    )*};
}
accessors!(
    bg, fg, dim, blue, heading, strong, plate_bg, strip_bg, border, green, yellow, red, cyan,
    code_bg, keyword, string, number, comment,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_palettes_align() {
        assert_eq!(NAMES.len(), PALETTES.len());
        // неизвестное имя не переключает (и не мутирует глобал)
        assert!(!set("definitely-not-a-theme"));
        // дефолт — grok
        assert!(NAMES.contains(&name()));
    }
}
