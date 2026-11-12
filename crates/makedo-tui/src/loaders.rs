//! Braille-спиннеры из cli-loaders/unicode-animations (MIT, см. NOTICE) —
//! только те, что реально показывает TUI.

#[derive(Debug, Clone, Copy)]
pub struct Spinner {
    pub name: &'static str,
    pub frames: &'static [&'static str],
    pub interval_ms: u64,
}

pub const BRAILLE: Spinner = Spinner {
    name: "braille",
    frames: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
    interval_ms: 80,
};

pub const BRAILLEWAVE: Spinner = Spinner {
    name: "braillewave",
    frames: &[
        "⠁⠂⠄⡀",
        "⠂⠄⡀⢀",
        "⠄⡀⢀⠠",
        "⡀⢀⠠⠐",
        "⢀⠠⠐⠈",
        "⠠⠐⠈⠁",
        "⠐⠈⠁⠂",
        "⠈⠁⠂⠄",
    ],
    interval_ms: 100,
};

pub const BREATHE: Spinner = Spinner {
    name: "breathe",
    frames: &[
        "⠀", "⠂", "⠌", "⡑", "⢕", "⢝", "⣫", "⣟", "⣿", "⣟", "⣫", "⢝", "⢕", "⡑", "⠌", "⠂", "⠀",
    ],
    interval_ms: 100,
};

pub const COLUMNS: Spinner = Spinner {
    name: "columns",
    frames: &[
        "⡀⠀⠀",
        "⡄⠀⠀",
        "⡆⠀⠀",
        "⡇⠀⠀",
        "⣇⠀⠀",
        "⣧⠀⠀",
        "⣷⠀⠀",
        "⣿⠀⠀",
        "⣿⡀⠀",
        "⣿⡄⠀",
        "⣿⡆⠀",
        "⣿⡇⠀",
        "⣿⣇⠀",
        "⣿⣧⠀",
        "⣿⣷⠀",
        "⣿⣿⠀",
        "⣿⣿⡀",
        "⣿⣿⡄",
        "⣿⣿⡆",
        "⣿⣿⡇",
        "⣿⣿⣇",
        "⣿⣿⣧",
        "⣿⣿⣷",
        "⣿⣿⣿",
        "⣿⣿⣿",
        "⠀⠀⠀",
    ],
    interval_ms: 60,
};

pub const SNAKE: Spinner = Spinner {
    name: "snake",
    frames: &[
        "⣁⡀", "⣉⠀", "⡉⠁", "⠉⠉", "⠈⠙", "⠀⠛", "⠐⠚", "⠒⠒", "⠖⠂", "⠶⠀", "⠦⠄", "⠤⠤", "⠠⢤", "⠀⣤", "⢀⣠",
        "⣀⣀",
    ],
    interval_ms: 80,
};
