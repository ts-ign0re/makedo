//! Явные OpenAI-compatible профили; выбранный effort хранится отдельно от модели по умолчанию.

use std::collections::BTreeMap;
use std::path::Path;

use makedo_config::ProviderConfig;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    pub levels: Vec<String>,
    pub default: String,
}

#[derive(Debug, Default, Clone)]
pub struct Settings {
    pub levels: Vec<String>,
    pub selected: Option<String>,
    key: String,
}

impl Settings {
    #[must_use]
    pub fn load(cfg: &ProviderConfig, global: Option<&Path>) -> Self {
        // Другие диалекты получат свои адаптеры, когда для них появится поддержка.
        if matches!(cfg.kind.as_str(), "anthropic" | "google" | "google-vertex")
            || cfg.kind.ends_with("-cli")
        {
            return Self::default();
        }
        let catalog = crate::catalog::load(global.map(|d| d.join("cache")).as_deref());
        let Some(entry) = catalog.get(&cfg.kind) else {
            return Self::default();
        };
        let Some(profile) = entry.effort.get(&cfg.model) else {
            return Self::default();
        };
        let base = cfg.base_url.clone().unwrap_or_else(|| {
            if cfg.kind == "ollama" {
                crate::ollama_base()
            } else {
                entry.api.clone()
            }
        });
        let base = crate::normalize_openai_base(&base);
        // Профиль другого endpoint нельзя применять только по совпадению имени модели.
        if base != crate::normalize_openai_base(&entry.api)
            || profile.levels.is_empty()
            || !profile.levels.contains(&profile.default)
            || profile.levels.iter().any(|level| level.trim().is_empty())
        {
            return Self::default();
        }
        let key = serde_json::to_string(&(&cfg.kind, &base, &cfg.model)).unwrap_or_default();
        let selected = global
            .and_then(|dir| read_saved(dir).ok())
            .and_then(|mut saved| saved.remove(&key))
            .filter(|value| profile.levels.contains(value))
            .unwrap_or_else(|| profile.default.clone());
        Self {
            levels: profile.levels.clone(),
            selected: Some(selected),
            key,
        }
    }

    pub fn set(&mut self, value: &str, global: Option<&Path>) -> Result<(), String> {
        if self.levels.is_empty() {
            return Err("No effort levels configured for this model.".to_owned());
        }
        if !self.levels.iter().any(|level| level == value) {
            return Err(format!(
                "Unsupported effort '{value}'. Choose: {}",
                self.levels.join(", ")
            ));
        }
        if let Some(dir) = global {
            let save = || -> std::io::Result<()> {
                let mut saved = read_saved(dir)?;
                saved.insert(self.key.clone(), value.to_owned());
                std::fs::create_dir_all(dir)?;
                let tmp = dir.join(format!(".effort.{}.tmp", std::process::id()));
                let bytes = serde_json::to_vec_pretty(&saved).map_err(std::io::Error::other)?;
                std::fs::write(&tmp, bytes)?;
                std::fs::rename(tmp, dir.join("effort.json"))
            };
            save().map_err(|e| format!("Could not save effort: {e}"))?;
        }
        self.selected = Some(value.to_owned());
        Ok(())
    }
}

fn read_saved(dir: &Path) -> std::io::Result<BTreeMap<String, String>> {
    match std::fs::read(dir.join("effort.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn qwen_effort_is_validated_persisted_and_bound_to_endpoint_and_model() {
        let dir = std::env::temp_dir().join(format!("makedo-effort-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = "[provider]\nkind = 'claude-cli'\nmodel = 'sonnet'\n";
        std::fs::write(dir.join("config.toml"), config).unwrap();
        std::fs::write(dir.join("models.json"), r#"{"qwen-vast":{"api":"http://localhost:8000/v1","env":["QWEN_VAST_TOKEN"],"models":{},"effort":{"qwen3.8":{"levels":["low","medium","xhigh"],"default":"low"}}}}"#).unwrap();
        let mut cfg = ProviderConfig {
            kind: "qwen-vast".into(),
            model: "qwen3.8".into(),
            ..ProviderConfig::default()
        };
        let mut settings = Settings::load(&cfg, Some(&dir));
        assert_eq!(settings.levels, ["low", "medium", "xhigh"]);
        assert_eq!(settings.selected.as_deref(), Some("low"));
        for value in ["low", "medium", "xhigh"] {
            settings.set(value, Some(&dir)).unwrap();
            assert_eq!(
                Settings::load(&cfg, Some(&dir)).selected.as_deref(),
                Some(value)
            );
        }
        assert!(
            settings
                .set("high", Some(&dir))
                .unwrap_err()
                .contains("low, medium, xhigh")
        );
        assert_eq!(
            Settings::load(&cfg, Some(&dir)).selected.as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml")).unwrap(),
            config
        );
        cfg.model = "another-model".into();
        assert!(Settings::load(&cfg, Some(&dir)).selected.is_none());
        cfg.model = "qwen3.8".into();
        cfg.base_url = Some("http://localhost:9000/v1".into());
        assert!(Settings::load(&cfg, Some(&dir)).levels.is_empty());
        cfg.kind = "claude-cli".into();
        assert!(Settings::load(&cfg, Some(&dir)).levels.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
