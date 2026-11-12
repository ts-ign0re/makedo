//! Конфиг makedo: TOML, merge `~/.makedo/config.toml` → `.makedo/config.toml`.
//! Семантика merge: проект побеждает ЦЕЛИКОМ по записи — секция или запись `[mcp.<name>]`
//! замещается, без глубокого field-merge (предсказуемость важнее гибкости, PLAN §3.7).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
}

/// Итоговый конфиг после merge. Неизвестные ключи не фатальны.
// ponytail: сбор unknown-ключей в diagnostics — в `makedo doctor` (M3)
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub provider: ProviderConfig,
    pub skills: SkillsConfig,
    pub mcp: BTreeMap<String, McpServerConfig>,
    pub tui: TuiConfig,
    pub verify: VerifyConfig,
    pub memory: MemoryConfig,
}

/// Local semantic retrieval; the chat provider is independent of this model.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    pub embedding_url: String,
    pub embedding_model: String,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            embedding_url: "http://127.0.0.1:11434/api/embed".into(),
            embedding_model: "embeddinggemma".into(),
        }
    }
}

/// Verification gate (CORE §достроить-2): команды самопроверки проекта.
/// Турн с правками не считается завершённым, пока они не зелёные (2 раунда).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct VerifyConfig {
    /// Shell-команды (`sh -c`), например `["cargo test", "cargo clippy"]`.
    /// Пусто = гейт выключен.
    pub cmds: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// `anthropic` | `openai-compat`
    pub kind: String,
    pub model: String,
    /// Для `openai-compat` (`xAI`, `OpenRouter`, `Ollama`). Для `anthropic` — дефолтный эндпоинт.
    pub base_url: Option<String>,
    /// Имя env-переменной с ключом; сам ключ в конфиге не живёт (секреты — PLAN §3.7).
    pub api_key_env: Option<String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: "anthropic".to_owned(),
            model: "claude-sonnet-5".to_owned(),
            base_url: None,
            api_key_env: None,
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// `"lite" | "full" | "ultra"` или `false`.
    pub ponytail: PonytailLevel,
    /// Доктрина проектирования: `"ddd" | "hexagonal"` или `false`/`"none"`.
    pub doctrine: Doctrine,
}

/// Доктрина проектирования систем — встроенный набор правил (skill), который
/// агент соблюдает. Строгость адаптивная (эскалирует по сложности домена).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Doctrine {
    /// Без доктрины — только ponytail.
    #[default]
    None,
    /// Domain-Driven Design: доменный язык → вики, агрегаты, доменные события.
    Ddd,
    /// Hexagonal (ports & adapters): домен без зависимостей от фреймворков.
    Hexagonal,
}

impl Doctrine {
    /// Имя embedded-скилла доктрины (`None` — доктрина не активна).
    #[must_use]
    pub fn skill_name(self) -> Option<&'static str> {
        match self {
            Doctrine::None => None,
            Doctrine::Ddd => Some("doctrine-ddd"),
            Doctrine::Hexagonal => Some("doctrine-hexagonal"),
        }
    }

    /// Каноничное имя для конфига/CLI (`none`/`ddd`/`hexagonal`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Doctrine::None => "none",
            Doctrine::Ddd => "ddd",
            Doctrine::Hexagonal => "hexagonal",
        }
    }

    /// Разбор имени в доктрину (`false`/пусто/`none` → None).
    #[must_use]
    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "false" | "off" | "" => Some(Doctrine::None),
            "ddd" => Some(Doctrine::Ddd),
            "hexagonal" | "hex" => Some(Doctrine::Hexagonal),
            _ => None,
        }
    }

    /// Список для пикера: (доктрина, имя, короткое описание для новичка).
    #[must_use]
    pub fn choices() -> [(Doctrine, &'static str, &'static str); 3] {
        [
            (
                Doctrine::None,
                "none",
                "No design doctrine — just ponytail (keep it minimal)",
            ),
            (
                Doctrine::Ddd,
                "ddd",
                "Domain-Driven Design — model your world, clean core, decisions in the wiki",
            ),
            (
                Doctrine::Hexagonal,
                "hexagonal",
                "Hexagonal (ports & adapters) — business logic isolated from frameworks/DB/UI",
            ),
        ]
    }
}

impl<'de> Deserialize<'de> for Doctrine {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Flag(bool),
            Name(String),
        }
        match Raw::deserialize(de)? {
            Raw::Flag(false) => Ok(Self::None),
            Raw::Flag(true) => Ok(Self::Ddd),
            Raw::Name(s) => Self::from_name(&s).ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "skills.doctrine: expected ddd|hexagonal|none, got `{s}`"
                ))
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PonytailLevel {
    Off,
    Lite,
    #[default]
    Full,
    Ultra,
}

impl<'de> Deserialize<'de> for PonytailLevel {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Flag(bool),
            Level(String),
        }
        match Raw::deserialize(de)? {
            Raw::Flag(false) => Ok(Self::Off),
            Raw::Flag(true) => Ok(Self::Full),
            Raw::Level(s) => match s.as_str() {
                "lite" => Ok(Self::Lite),
                "full" => Ok(Self::Full),
                "ultra" => Ok(Self::Ultra),
                other => Err(serde::de::Error::custom(format!(
                    "skills.ponytail: expected lite|full|ultra|false, got `{other}`"
                ))),
            },
        }
    }
}

/// Запись `[mcp.<name>]`: stdio (command) либо remote (url). PLAN §3.2.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub command: Option<Vec<String>>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub enabled: bool,
    pub startup_timeout_s: u64,
    pub tool_timeout_s: u64,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            command: None,
            env: BTreeMap::new(),
            cwd: None,
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
            startup_timeout_s: 30,
            tool_timeout_s: 60,
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    pub theme: Option<String>,
}

/// Слой для merge: каждая секция опциональна — «присутствует» значит «замещает».
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Layer {
    provider: Option<ProviderConfig>,
    skills: Option<SkillsConfig>,
    mcp: BTreeMap<String, McpServerConfig>,
    tui: Option<TuiConfig>,
    verify: Option<VerifyConfig>,
    memory: Option<MemoryConfig>,
}

/// Корень данных makedo для проекта. **git-репо → `<project>/.makedo`** (данные
/// рядом с кодом, вики версионируется); **нет git → `~/.makedo/projects/<name>-<hash>`**
/// (изолированно по пути проекта). Всё — вики, история, спиллы — под этим корнем.
#[must_use]
pub fn data_dir(project_root: &Path) -> PathBuf {
    let project_root = self::project_root(project_root);
    if project_root.join(".git").exists() {
        return project_root.join(".makedo");
    }
    let name = project_root
        .file_name()
        .map_or_else(|| "root".to_owned(), |s| s.to_string_lossy().into_owned());
    // дешёвый детерминированный хэш пути (djb2) — различает одноимённые папки
    let hash = project_root
        .to_string_lossy()
        .bytes()
        .fold(5381u64, |h, b| {
            h.wrapping_mul(33).wrapping_add(u64::from(b))
        });
    global_dir()
        .unwrap_or_else(|| PathBuf::from(".makedo"))
        .join("projects")
        .join(format!("{name}-{hash:x}"))
}

/// Данные проекта хранятся внутри самого проекта (git-режим)?
#[must_use]
pub fn data_is_in_project(project_root: &Path) -> bool {
    self::project_root(project_root).join(".git").exists()
}

/// Resolve from subdirectories, while keeping linked worktrees separate for code and wiki.
pub fn project_root(cwd: &Path) -> PathBuf {
    if cwd.join(".git").exists() {
        return cwd.to_owned();
    }
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map_or_else(
            || cwd.to_owned(),
            |out| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()),
        )
}

/// `~/.makedo` (или `$MAKEDO_HOME` для тестов/нестандартных установок).
#[must_use]
pub fn global_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("MAKEDO_HOME") {
        return Some(PathBuf::from(p));
    }
    // ponytail: HOME достаточно — таргеты только unix; крейт dirs — если появится Windows
    std::env::var("HOME")
        .ok()
        .map(|h| Path::new(&h).join(".makedo"))
}

/// Кэш каталога моделей: `~/.makedo/cache`.
#[must_use]
pub fn cache_dir() -> Option<PathBuf> {
    global_dir().map(|d| d.join("cache"))
}

/// Загрузка с merge: global → project. Отсутствующие файлы — не ошибка.
pub fn load(project_root: &Path) -> Result<Config, ConfigError> {
    load_from(global_dir().as_deref(), &self::project_root(project_root))
}

/// Явная версия для тестов и нестандартных раскладок (без чтения env).
pub fn load_from(global: Option<&Path>, project_root: &Path) -> Result<Config, ConfigError> {
    let mut layers = Vec::new();
    if let Some(dir) = global
        && let Some(l) = read_layer(&dir.join("config.toml"))?
    {
        layers.push(l);
    }
    if let Some(l) = read_layer(&project_root.join(".makedo").join("config.toml"))? {
        layers.push(l);
    }
    let mut cfg = Config::default();
    for layer in layers {
        if let Some(p) = layer.provider {
            cfg.provider = p;
        }
        if let Some(s) = layer.skills {
            cfg.skills = s;
        }
        if let Some(t) = layer.tui {
            cfg.tui = t;
        }
        if let Some(v) = layer.verify {
            cfg.verify = v;
        }
        if let Some(memory) = layer.memory {
            cfg.memory = memory;
        }
        // записи mcp замещаются поимённо, целиком
        cfg.mcp.extend(layer.mcp);
    }
    Ok(cfg)
}

fn read_layer(path: &Path) -> Result<Option<Layer>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ConfigError::Io {
                path: path.to_owned(),
                source: e,
            });
        }
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|e| ConfigError::Parse {
            path: path.to_owned(),
            source: Box::new(e),
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn parse(s: &str) -> Layer {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn defaults() {
        let cfg = Config::default();
        assert_eq!(cfg.provider.kind, "anthropic");
        assert_eq!(cfg.skills.ponytail, PonytailLevel::Full);
    }

    #[test]
    fn ponytail_forms() {
        let l = parse("[skills]\nponytail = \"ultra\"");
        assert_eq!(l.skills.unwrap().ponytail, PonytailLevel::Ultra);
        let l = parse("[skills]\nponytail = false");
        assert_eq!(l.skills.unwrap().ponytail, PonytailLevel::Off);
    }

    #[test]
    fn merge_project_wins_per_record() {
        let root = std::env::temp_dir().join(format!("makedo-cfg-test-{}", std::process::id()));
        let makedo_home = root.join("home");
        let project = root.join("proj");
        std::fs::create_dir_all(&makedo_home).unwrap();
        std::fs::create_dir_all(project.join(".makedo")).unwrap();
        std::fs::write(
            makedo_home.join("config.toml"),
            r#"
[provider]
kind = "openai-compat"
model = "global-model"

[mcp.github]
command = ["npx", "server-github"]

[mcp.linear]
url = "https://global.example"
"#,
        )
        .unwrap();
        std::fs::write(
            project.join(".makedo/config.toml"),
            r#"
[mcp.linear]
url = "https://project.example"
tool_timeout_s = 120
"#,
        )
        .unwrap();

        let cfg = load_from(Some(&makedo_home), &project).unwrap();

        assert_eq!(cfg.provider.model, "global-model");
        assert!(cfg.mcp.contains_key("github"));
        let linear = &cfg.mcp["linear"];
        assert_eq!(linear.url.as_deref(), Some("https://project.example"));
        assert_eq!(linear.tool_timeout_s, 120);
        std::fs::remove_dir_all(&root).ok();
    }
}

/// Вписывает пары `key = value` в секцию `config.toml` каталога `dir`, не трогая
/// остальные секции файла. Best-effort: нечитаемый файл заменяется новым.
fn persist(dir: &Path, section: &str, values: &[(&str, toml::Value)]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("config.toml");
    let mut doc: toml::Table = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default();
    let section = doc
        .entry(section)
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let Some(table) = section.as_table_mut() {
        for (key, value) in values {
            table.insert((*key).to_owned(), value.clone());
        }
    }
    std::fs::write(&path, toml::to_string(&doc).unwrap_or_default())
}

/// То же для глобального `~/.makedo/config.toml`; без домашней директории — no-op.
fn persist_global(section: &str, values: &[(&str, toml::Value)]) -> std::io::Result<()> {
    global_dir().map_or(Ok(()), |dir| persist(&dir, section, values))
}

/// Сохраняет выбор провайдера/модели в `[provider]` глобального конфига.
pub fn persist_provider(kind: &str, model: &str) -> std::io::Result<()> {
    persist_global(
        "provider",
        &[("kind", kind.into()), ("model", model.into())],
    )
}

/// Угадывает команды самопроверки по файлам проекта (онбординг verify gate).
#[must_use]
pub fn detect_verify_cmds(root: &Path) -> Vec<String> {
    let mut cmds = Vec::new();
    if root.join("Cargo.toml").exists() {
        cmds.push("cargo test".to_owned());
    }
    if root.join("package.json").exists()
        && std::fs::read_to_string(root.join("package.json")).is_ok_and(|s| s.contains("\"test\""))
    {
        cmds.push("npm test -s".to_owned());
    }
    if root.join("pyproject.toml").exists() || root.join("pytest.ini").exists() {
        cmds.push("pytest -q".to_owned());
    }
    if root.join("go.mod").exists() {
        cmds.push("go test ./...".to_owned());
    }
    cmds
}

/// Персистит verify-команды в конфиг ПРОЕКТА (`<root>/.makedo/config.toml`,
/// секция `[verify]`) — именно этот файл читает `load_from` как project-слой.
pub fn persist_verify_cmds(project_root: &Path, cmds: &[String]) -> std::io::Result<()> {
    persist(
        &project_root.join(".makedo"),
        "verify",
        &[("cmds", cmds.to_vec().into())],
    )
}

/// Сохраняет выбранную тему в `[tui] theme` глобального конфига.
pub fn persist_theme(name: &str) -> std::io::Result<()> {
    persist_global("tui", &[("theme", name.into())])
}

/// Сохраняет выбранную доктрину проектирования в `[skills] doctrine` конфига.
pub fn persist_doctrine(name: &str) -> std::io::Result<()> {
    persist_global("skills", &[("doctrine", name.into())])
}

#[cfg(test)]
mod persist_tests {
    #![allow(clippy::unwrap_used)]

    #[test]
    fn persist_updates_one_section_and_keeps_the_rest() {
        let dir = std::env::temp_dir().join(format!("makedo-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "[provider]\nkind = \"a\"\nbase_url = \"u\"\n[tui]\ntheme = \"nord\"\n",
        )
        .unwrap();
        super::persist(
            &dir,
            "provider",
            &[("kind", "b".into()), ("model", "m".into())],
        )
        .unwrap();
        super::persist(
            &dir,
            "verify",
            &[("cmds", vec!["cargo test".to_owned()].into())],
        )
        .unwrap();
        let doc: toml::Table =
            toml::from_str(&std::fs::read_to_string(dir.join("config.toml")).unwrap()).unwrap();
        assert_eq!(doc["provider"]["kind"].as_str(), Some("b"));
        assert_eq!(doc["provider"]["model"].as_str(), Some("m"));
        assert_eq!(doc["provider"]["base_url"].as_str(), Some("u"));
        assert_eq!(doc["tui"]["theme"].as_str(), Some("nord"));
        assert_eq!(doc["verify"]["cmds"][0].as_str(), Some("cargo test"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// Секреты: `~/.makedo/credentials.toml`, секция `[keys]` (`ENV_NAME = "value"`).
/// Отдельно от конфига, chmod 0600 (PLAN §3.7). Env всегда приоритетнее.
pub mod credentials {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn path() -> Option<PathBuf> {
        super::global_dir().map(|d| d.join("credentials.toml"))
    }

    #[must_use]
    pub fn get(env_name: &str) -> Option<String> {
        let text = std::fs::read_to_string(path()?).ok()?;
        let parsed: toml::Value = toml::from_str(&text).ok()?;
        parsed
            .get("keys")?
            .get(env_name)?
            .as_str()
            .map(ToOwned::to_owned)
    }

    fn set_owner_only(p: &std::path::Path) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600))
    }

    pub fn save(env_name: &str, value: &str) -> std::io::Result<()> {
        use std::fmt::Write as _;
        let Some(p) = path() else {
            return Err(std::io::Error::other("no ~/.makedo directory"));
        };
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut keys: BTreeMap<String, String> = std::fs::read_to_string(&p)
            .ok()
            .and_then(|t| toml::from_str::<toml::Value>(&t).ok())
            .and_then(|v| {
                v.get("keys").and_then(|k| {
                    k.as_table().map(|t| {
                        t.iter()
                            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                            .collect()
                    })
                })
            })
            .unwrap_or_default();
        keys.insert(env_name.to_owned(), value.to_owned());
        let mut out = String::from("# makedo credentials — chmod 0600, не коммитить\n[keys]\n");
        for (k, v) in &keys {
            let _ = writeln!(out, "{k} = {v:?}");
        }
        std::fs::write(&p, out)?;
        set_owner_only(&p)
    }
}
