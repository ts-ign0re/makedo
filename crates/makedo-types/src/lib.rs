//! Общий словарь workspace: newtype-инварианты имён, BM25, frontmatter.
//! Зависит только от serde/thiserror (PLAN §2).

pub mod bm25;
pub mod event;
pub mod resources;

use std::collections::BTreeMap;

use serde::Serialize;

/// Плоский frontmatter `--- key: value ... ---` (никакого YAML): строки с
/// отступом продолжают предыдущее значение, ведущий `>` отбрасывается.
/// Возвращает поля и тело; `None` — блока нет.
// ponytail: вложенный YAML не поддерживается; `serde_yaml` — если реально понадобится
#[must_use]
pub fn frontmatter(text: &str) -> Option<(BTreeMap<String, String>, &str)> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in rest[..end].lines() {
        if line.starts_with([' ', '\t']) {
            if let Some(value) = current.as_ref().and_then(|key| fields.get_mut(key)) {
                if !value.is_empty() {
                    value.push(' ');
                }
                value.push_str(line.trim());
            }
        } else if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_owned();
            fields.insert(
                key.clone(),
                value.trim().trim_start_matches('>').trim().to_owned(),
            );
            current = Some(key);
        }
    }
    Some((fields, rest[end + 4..].trim_start_matches('\n')))
}

/// Разделитель qualified-имени MCP-тула: `server__tool`.
pub const MCP_NAME_DELIMITER: &str = "__";

/// Максимальная длина имени тула — общий знаменатель провайдеров
/// (Gemini: первая буква/underscore; Anthropic/OpenAI: без точек; 64 символа).
pub const MAX_TOOL_NAME_LEN: usize = 64;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NameError {
    #[error("invalid name `{0}`: must match ^[A-Za-z_][A-Za-z0-9_-]{{0,63}}$")]
    Invalid(String),
    #[error("server name `{0}` must not contain `__` (reserved as the MCP name delimiter)")]
    DelimiterInServer(String),
    #[error("qualified tool name `{0}` exceeds {MAX_TOOL_NAME_LEN} chars")]
    TooLong(String),
}

/// `^[A-Za-z_][A-Za-z0-9_-]{0,63}$` — без regex-крейта (политика зависимостей, PLAN §1.2).
fn is_valid_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let head_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    head_ok
        && s.len() <= MAX_TOOL_NAME_LEN
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Имя встроенного тула. Инвариант: валидный идентификатор БЕЗ `__`
/// (`__` зарезервирован за MCP — типом закрывает `contains("__")`-эвристику grok-build).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ToolId(String);

impl ToolId {
    pub fn new(s: impl Into<String>) -> Result<Self, NameError> {
        let s = s.into();
        if !is_valid_ident(&s) {
            return Err(NameError::Invalid(s));
        }
        if s.contains(MCP_NAME_DELIMITER) {
            return Err(NameError::DelimiterInServer(s));
        }
        Ok(Self(s))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Имя MCP-сервера. Инвариант: валидный идентификатор без `__`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ServerName(String);

impl ServerName {
    pub fn new(s: impl Into<String>) -> Result<Self, NameError> {
        ToolId::new(s).map(|id| Self(id.0))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Qualified-имя MCP-тула `server__tool`. Конструируется только из валидных частей;
/// суммарная длина ограничена лимитом провайдеров.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QualifiedToolName {
    server: ServerName,
    tool: String,
    full: String,
}

impl QualifiedToolName {
    pub fn new(server: ServerName, tool: impl Into<String>) -> Result<Self, NameError> {
        let tool = tool.into();
        if !is_valid_ident(&tool) {
            return Err(NameError::Invalid(tool));
        }
        let full = format!("{}{}{}", server.as_str(), MCP_NAME_DELIMITER, tool);
        if full.len() > MAX_TOOL_NAME_LEN {
            return Err(NameError::TooLong(full));
        }
        Ok(Self { server, tool, full })
    }

    /// Разбор имени, пришедшего от модели. Границей считается ПЕРВОЕ `__`
    /// (имена серверов `__` не содержат по инварианту `ServerName`).
    pub fn parse(s: &str) -> Result<Self, NameError> {
        let (server, tool) = s
            .split_once(MCP_NAME_DELIMITER)
            .ok_or_else(|| NameError::Invalid(s.to_owned()))?;
        Self::new(ServerName::new(server)?, tool)
    }

    #[must_use]
    pub fn server(&self) -> &ServerName {
        &self.server
    }

    #[must_use]
    pub fn tool(&self) -> &str {
        &self.tool
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.full
    }
}

impl std::fmt::Display for QualifiedToolName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.full)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn frontmatter_fields_folding_and_body() {
        let (fm, body) =
            frontmatter("---\nname: x\ndescription: >\n  one\n  two\n---\n\nbody\n").unwrap();
        assert_eq!(fm["name"], "x");
        assert_eq!(fm["description"], "one two");
        assert_eq!(body, "body\n");
        assert!(frontmatter("no block\n").is_none());
    }

    #[test]
    fn ident_validation() {
        assert!(ToolId::new("read_file").is_ok());
        assert!(ToolId::new("_x").is_ok());
        assert!(ToolId::new("9bad").is_err());
        assert!(ToolId::new("").is_err());
        assert!(ToolId::new("a".repeat(65)).is_err());
        assert!(ToolId::new("has.dot").is_err());
        // `__` зарезервирован за MCP
        assert!(ToolId::new("sneaky__tool").is_err());
        assert!(ServerName::new("my__server").is_err());
    }

    #[test]
    fn qualified_roundtrip() {
        let q = QualifiedToolName::parse("github__create_issue").unwrap();
        assert_eq!(q.server().as_str(), "github");
        assert_eq!(q.tool(), "create_issue");
        assert_eq!(q.as_str(), "github__create_issue");
        // tool с одиночными underscore внутри — ок, граница всегда первая `__`
        let q = QualifiedToolName::parse("srv__a_b_c").unwrap();
        assert_eq!(q.tool(), "a_b_c");
        assert!(QualifiedToolName::parse("no_delimiter").is_err());
        assert!(QualifiedToolName::parse("__tool").is_err());
        assert!(QualifiedToolName::parse("srv__").is_err());
    }
}
