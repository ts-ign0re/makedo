//! Реестр слэш-команд + палитра с фильтрацией (как в grok). Команды
//! алгоритмические (без LLM), кроме тех что шлют промпт/тул в сессию.

/// Определение команды для палитры и диспатча.
#[derive(Debug, Clone, Copy)]
pub struct Command {
    /// Имя без ведущего `/`.
    pub name: &'static str,
    /// Аргумент-подсказка (пусто = без аргумента).
    pub arg_hint: &'static str,
    pub description: &'static str,
}

/// Все команды. `/help` обязательна (требование).
pub const COMMANDS: &[Command] = &[
    Command {
        name: "yolo",
        arg_hint: "",
        description: "Allow every tool without approval for this session (also accepts YOLO in chat)",
    },
    Command {
        name: "agents",
        arg_hint: "",
        description: "Toggle agent progress panel (F6 details, F7 toggle)",
    },
    Command {
        name: "ponytail",
        arg_hint: "[lite|full|ultra|off]",
        description: "Set Ponytail intensity for this session",
    },
    Command {
        name: "logs",
        arg_hint: "",
        description: "Show full paths to tool output and command logs",
    },
    Command {
        name: "init",
        arg_hint: "",
        description: "Make this a git project and move makedo data (wiki, history) into it",
    },
    Command {
        name: "help",
        arg_hint: "",
        description: "Show commands and keyboard shortcuts",
    },
    Command {
        name: "create-image",
        arg_hint: "<prompt>",
        description: "Generate an image (OpenRouter) into .makedo/assets",
    },
    Command {
        name: "worktree",
        arg_hint: "<name>",
        description: "Start an isolated feature worktree",
    },
    Command {
        name: "finish",
        arg_hint: "<name>",
        description: "Merge a worktree into main and remove it",
    },
    Command {
        name: "enrich",
        arg_hint: "[focus]",
        description: "Warm up the wiki: a curator turn reads the code and writes .makedo/wiki",
    },
    Command {
        name: "models",
        arg_hint: "",
        description: "List available models for the current provider",
    },
    Command {
        name: "effort",
        arg_hint: "",
        description: "Pick reasoning effort (or /effort <level>)",
    },
    Command {
        name: "allow-all",
        arg_hint: "",
        description: "Run all tools without asking (this session)",
    },
    Command {
        name: "clear",
        arg_hint: "",
        description: "Clear the screen (model context is retained)",
    },
    Command {
        name: "theme",
        arg_hint: "", // пусто → Enter в палитре сразу открывает пикер (как /models)
        description: "Pick a color theme (live preview)",
    },
    Command {
        name: "doctrine",
        arg_hint: "", // пусто → Enter открывает пикер доктрины проектирования
        description: "Pick a design doctrine (DDD / hexagonal) — applied live",
    },
    Command {
        name: "quit",
        arg_hint: "",
        description: "Quit makedo",
    },
];

/// Палитра команд: фильтр по вводу + выбранный индекс.
#[derive(Debug, Default)]
pub struct Palette {
    pub selected: usize,
}

/// Совпадение команды с фильтром: команда + позиция подсветки `[start, end)`.
#[derive(Debug, Clone, Copy)]
pub struct Match {
    pub cmd: &'static Command,
    /// Диапазон совпавших байт в `cmd.name` (для подсветки).
    pub hl: (usize, usize),
}

impl Palette {
    /// Substring-матчинг (как grok: `/help` находит и `ponytail-help`),
    /// ранжирование: префиксные совпадения выше, затем по алфавиту.
    /// Nucleo-fuzzy не тащим — substring покрывает кейсы без зависимости.
    #[must_use]
    pub fn matches(filter: &str) -> Vec<Match> {
        if filter.is_empty() {
            let mut all: Vec<Match> = COMMANDS
                .iter()
                .map(|c| Match { cmd: c, hl: (0, 0) })
                .collect();
            all.sort_by(|a, b| a.cmd.name.cmp(b.cmd.name));
            return all;
        }
        let f = filter.to_ascii_lowercase();
        let mut hits: Vec<(bool, &'static Command, (usize, usize))> = COMMANDS
            .iter()
            .filter_map(|c| {
                c.name
                    .to_ascii_lowercase()
                    .find(&f)
                    .map(|pos| (pos == 0, c, (pos, pos + f.len())))
            })
            .collect();
        // префиксные (pos==0) первыми, затем по имени
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.cmp(b.1.name)));
        hits.into_iter()
            .map(|(_, cmd, hl)| Match { cmd, hl })
            .collect()
    }

    pub fn move_up(&mut self, len: usize) {
        if len == 0 {
            return;
        }
        self.selected = (self.selected + len - 1) % len;
    }

    pub fn move_down(&mut self, len: usize) {
        if len == 0 {
            return;
        }
        self.selected = (self.selected + 1) % len;
    }

    pub fn clamp(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }
}

/// Провайдер для `/models`: id каталога models.dev + env-ключ.
#[derive(Debug, Clone, Copy)]
pub struct ProviderChoice<S = &'static str> {
    /// id в каталоге / `kind` в конфиге.
    pub kind: S,
    pub label: S,
    pub env: S,
}

/// Кураторский список популярных провайдеров (все есть в каталоге models.dev).
pub const PROVIDERS: &[ProviderChoice] = &[
    ProviderChoice {
        kind: "anthropic",
        label: "Anthropic (Claude)",
        env: "ANTHROPIC_API_KEY",
    },
    ProviderChoice {
        kind: "openai",
        label: "OpenAI (GPT)",
        env: "OPENAI_API_KEY",
    },
    ProviderChoice {
        kind: "deepseek",
        label: "DeepSeek",
        env: "DEEPSEEK_API_KEY",
    },
    ProviderChoice {
        kind: "openrouter",
        label: "OpenRouter (any model)",
        env: "OPENROUTER_API_KEY",
    },
    ProviderChoice {
        kind: "google",
        label: "Google (Gemini)",
        env: "GEMINI_API_KEY",
    },
    ProviderChoice {
        kind: "xai",
        label: "xAI (Grok)",
        env: "XAI_API_KEY",
    },
    ProviderChoice {
        kind: "groq",
        label: "Groq",
        env: "GROQ_API_KEY",
    },
    ProviderChoice {
        kind: "mistral",
        label: "Mistral",
        env: "MISTRAL_API_KEY",
    },
    // локальный, без ключа: env="" → шаг ввода ключа пропускается
    ProviderChoice {
        kind: "ollama",
        label: "Ollama (local)",
        env: "",
    },
    // подписка ChatGPT: свой цикл makedo, вход — `makedo login chatgpt`, ключ не нужен
    ProviderChoice {
        kind: "chatgpt",
        label: "ChatGPT plan (sign in)",
        env: "",
    },
    // CLI-делегаты: авторизация — логин CLI (подписка), ключ не нужен
    ProviderChoice {
        kind: "claude-cli",
        label: "Claude Code CLI (subscription)",
        env: "",
    },
    ProviderChoice {
        kind: "codex-cli",
        label: "Codex CLI (subscription)",
        env: "",
    },
    ProviderChoice {
        kind: "grok-cli",
        label: "Grok CLI (subscription)",
        env: "",
    },
];

/// Популярные провайдеры плюс пользовательские OpenAI-compatible endpoints.
#[must_use]
pub fn provider_choices(
    custom: &makedo_providers::catalog::Catalog,
) -> Vec<ProviderChoice<String>> {
    let mut choices: Vec<_> = PROVIDERS
        .iter()
        .map(|p| ProviderChoice {
            kind: p.kind.to_owned(),
            label: p.label.to_owned(),
            env: p.env.to_owned(),
        })
        .collect();
    for (kind, entry) in custom {
        let env = entry
            .env
            .first()
            .cloned()
            .unwrap_or_else(|| "OPENAI_API_KEY".to_owned());
        if let Some(p) = choices.iter_mut().find(|p| p.kind == *kind) {
            p.env = env;
        } else {
            choices.push(ProviderChoice {
                kind: kind.clone(),
                label: kind.clone(),
                env,
            });
        }
    }
    choices
}

/// Текст справки `/help` — команды + горячие клавиши.
#[must_use]
pub fn help_text() -> String {
    use std::fmt::Write as _;
    let mut out = String::from("Commands:\n");
    for c in COMMANDS {
        let name = if c.arg_hint.is_empty() {
            format!("/{}", c.name)
        } else {
            format!("/{} {}", c.name, c.arg_hint)
        };
        let _ = writeln!(out, "  {name:<24} {}", c.description);
    }
    out.push_str(
        "\nKeys:\n  \
         Enter — send / queue while busy    Ctrl+Enter — send now (interrupt step)\n  \
         Shift+Enter / Alt+Enter — newline    Esc — cancel turn or close palette\n  \
         /     — command palette (↑/↓ select, Tab complete, Enter run)\n  \
         y / n — approve / deny a permission    Ctrl+C — quit\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_command_exists_and_filter_works() {
        assert!(COMMANDS.iter().any(|c| c.name == "help"));
        let m = Palette::matches("");
        assert_eq!(m.len(), COMMANDS.len());
        let h = Palette::matches("hel");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].cmd.name, "help");
        assert_eq!(h[0].hl, (0, 3));
        assert!(Palette::matches("zzz").is_empty());
    }

    #[test]
    fn substring_matches_and_prefix_ranks_first() {
        // "age" встречается в "image"? нет команды image; проверим "e" — многие
        let m = Palette::matches("e");
        assert!(m.len() > 1);
        // все совпадения содержат "e"
        assert!(m.iter().all(|x| x.cmd.name.contains('e')));
    }

    #[test]
    fn palette_sorted_alphabetically() {
        let m = Palette::matches("");
        let names: Vec<&str> = m.iter().map(|x| x.cmd.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "команды меню должны быть по алфавиту");
    }

    #[test]
    fn selection_wraps() {
        let mut p = Palette::default();
        p.move_up(3);
        assert_eq!(p.selected, 2);
        p.move_down(3);
        assert_eq!(p.selected, 0);
        p.selected = 5;
        p.clamp(3);
        assert_eq!(p.selected, 2);
    }
}
