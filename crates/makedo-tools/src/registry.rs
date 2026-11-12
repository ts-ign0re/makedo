//! Реестр тулов: `BTreeMap` (lookup по имени + алфавитная выдача манифеста
//! даром — стабильность prompt-кэша, паттерн opencode), кэш JSON-схем при
//! регистрации. Дубликат имени — громкая ошибка, не перезапись.

use std::collections::BTreeMap;
use std::sync::Arc;

use schemars::JsonSchema;
use serde_json::Value;

use crate::{Tool, ToolDyn};

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("duplicate tool name `{0}`")]
    Duplicate(String),
    #[error(transparent)]
    InvalidName(#[from] makedo_types::NameError),
}

struct Entry {
    tool: Arc<dyn ToolDyn>,
    schema: Value,
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("name", &self.tool.name())
            .finish_non_exhaustive()
    }
}

/// Определение тула для манифеста модели.
#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Default)]
pub struct Registry {
    entries: BTreeMap<String, Entry>,
}

impl Registry {
    pub fn register<T: Tool>(&mut self, tool: T) -> Result<(), RegistryError> {
        let name = makedo_types::ToolId::new(tool.name())?.as_str().to_owned();
        let schema = schema_for::<T::Args>();
        let entry = Entry {
            tool: Arc::new(tool),
            schema,
        };
        if self.entries.contains_key(&name) {
            return Err(RegistryError::Duplicate(name));
        }
        self.entries.insert(name, entry);
        Ok(())
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn ToolDyn>> {
        self.entries.get(name).map(|e| Arc::clone(&e.tool))
    }

    /// Манифест модели — отсортирован по имени (стабильность prompt-кэша).
    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.entries
            .iter()
            .map(|(name, e)| ToolDefinition {
                name: name.clone(),
                description: e.tool.description().to_owned(),
                input_schema: e.schema.clone(),
            })
            .collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// draft-07 c инлайном сабсхем (провайдеры не переваривают `$ref` — обе базы
/// инлайнят) + гарантия `type/properties/required` (Azure/VSCode-класс серверов
/// шлют пустые схемы; провайдеры их отвергают).
fn schema_for<T: JsonSchema>() -> Value {
    let settings = schemars::generate::SchemaSettings::draft07().with(|s| {
        s.inline_subschemas = true;
    });
    let mut root = settings
        .into_generator()
        .into_root_schema_for::<T>()
        .to_value();
    if let Some(obj) = root.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
        obj.entry("type")
            .or_insert_with(|| Value::String("object".into()));
        obj.entry("properties")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        obj.entry("required")
            .or_insert_with(|| Value::Array(vec![]));
    }
    root
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::{ToolCtx, ToolError};

    #[derive(serde::Deserialize, JsonSchema)]
    struct EchoArgs {
        /// Text to echo back
        text: String,
    }

    struct Echo;
    impl Tool for Echo {
        type Args = EchoArgs;
        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &'static str {
            "Echo text back"
        }
        fn is_read_only(&self) -> bool {
            true
        }
        async fn run(&self, _ctx: &ToolCtx, args: EchoArgs) -> Result<String, ToolError> {
            Ok(args.text)
        }
    }

    fn ctx() -> ToolCtx {
        ToolCtx::new(std::env::temp_dir(), std::env::temp_dir())
    }

    #[test]
    fn duplicate_is_loud() {
        let mut r = Registry::default();
        r.register(Echo).unwrap();
        assert!(matches!(r.register(Echo), Err(RegistryError::Duplicate(n)) if n == "echo"));
    }

    #[test]
    fn schema_shape() {
        let mut r = Registry::default();
        r.register(Echo).unwrap();
        let defs = r.definitions();
        let s = &defs[0].input_schema;
        assert_eq!(s["type"], "object");
        assert!(s["properties"]["text"].is_object());
        assert_eq!(s["required"][0], "text");
        assert!(s.get("$schema").is_none());
    }

    #[tokio::test]
    async fn dispatch_and_invalid_args() {
        let mut r = Registry::default();
        r.register(Echo).unwrap();
        let tool = r.get("echo").unwrap();
        let out = tool
            .run_json(&ctx(), serde_json::json!({"text": "hi"}))
            .await
            .unwrap();
        assert_eq!(out, "hi");
        let err = tool
            .run_json(&ctx(), serde_json::json!({"wrong": 1}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }
}
