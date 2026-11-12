//! Провайдеры LLM: Anthropic Messages, `OpenAI` Responses и совместимые API
//! (`xAI`, `OpenRouter`, `Ollama`). Все диалекты дают единый поток `LlmEvent`.

pub mod catalog;
pub mod chatgpt;
pub mod effort;

mod anthropic;
mod cli;
mod gemini;
mod http;
mod openai;
mod responses;
mod sse;

use std::pin::Pin;

use futures_util::Stream;
use serde::{Deserialize, Serialize};

pub use anthropic::Anthropic;
pub use cli::{AgentCli, cli_available, cli_models};
pub use gemini::Gemini;
pub use openai::OpenAiCompat;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("stream protocol error: {0}")]
    Protocol(String),
    #[error("missing api key: env `{0}` is not set")]
    MissingKey(String),
    /// Вход по подписке не выполнен или больше не действует; текст — для пользователя.
    #[error("{0}")]
    Auth(String),
}

impl ProviderError {
    /// Ошибка размера запроса требует изменения контекста, а не сетевого ретрая.
    pub fn is_context_overflow(&self) -> bool {
        let Self::Api { status, body } = self else {
            return false;
        };
        if *status == 413 {
            return true;
        }
        if !matches!(status, 400 | 422) {
            return false;
        }
        let body = body.to_ascii_lowercase();
        [
            "context_length_exceeded",
            "contextwindowexceedederror",
            "request exceeds the maximum number of tokens",
            "request size exceeds",
            "payload too large",
            "request entity too large",
            "exceeds the model context",
            "exceeds the maximum context",
            "input length exceeds",
            "maximum context length",
            "exceeds the context window",
            "prompt is too long",
            "input is too long",
            "input token count exceeds",
        ]
        .iter()
        .any(|needle| body.contains(needle))
    }

    /// HTTP-статусы уже обработаны транспортом; повторяем только обрывы стрима.
    pub fn is_stream_retryable(&self) -> bool {
        match self {
            Self::Http(e) => e.is_connect() || e.is_timeout() || e.is_body() || e.is_decode(),
            Self::Protocol(_) => true,
            Self::Api { .. } | Self::MissingKey(_) | Self::Auth(_) => false,
        }
    }
}

/// Сообщение истории в нейтральной форме (диалект-специфика — внутри провайдера).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    System { content: String },
    User { content: String },
    Assistant { content: Vec<AssistantPart> },
    ToolResult { call_id: String, content: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantPart {
    /// Exact Responses output for stateless replay, including opaque reasoning and item IDs.
    /// Other providers use the neutral text/tool parts instead.
    OpenAiOutput {
        items: Vec<serde_json::Value>,
    },
    Text {
        text: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        input: serde_json::Value,
    },
}

/// Определение тула для манифеста модели.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema (draft-07, inlined) — уже нормализованная.
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    /// Проверенное значение из профиля выбранной модели; None = не отправлять.
    pub reasoning_effort: Option<String>,
}

/// Единый поток событий от любого провайдера.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    TextDelta(String),
    /// Terminal Responses output; kept in history, never rendered as chat text.
    OpenAiOutput(Vec<serde_json::Value>),
    /// Начало тул-колла: id + имя.
    ToolCallStart {
        call_id: String,
        name: String,
    },
    /// Дельта JSON-аргументов текущего тул-колла.
    ToolCallDelta {
        call_id: String,
        args_delta: String,
    },
    /// Финал турна.
    Finished {
        reason: StopReason,
        usage: Usage,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Stop,
    ToolCalls,
    MaxTokens,
    Other,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    /// ВСЕ input-токены, включая кэшированные (семантика нормализована по
    /// диалектам: у OpenAI/DeepSeek так и есть, у Anthropic маппер складывает).
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Input-токены, отданные из кэша (дешёвые), ⊆ `input_tokens`. Ядро
    /// экономики: пользователь не должен вылетать из кэша по нашей вине (M3.5).
    pub cache_read_tokens: u64,
    /// Токены, записанные в кэш (Anthropic `cache_creation`).
    pub cache_write_tokens: u64,
    /// Реальная стоимость, если её сообщил сам провайдер (CLI-делегаты:
    /// `total_cost_usd` из result-события). None = считаем по каталогу.
    pub cost_usd: Option<f64>,
}

pub type EventStream = Pin<Box<dyn Stream<Item = Result<LlmEvent, ProviderError>> + Send>>;

/// Шов с двумя реальными реализациями с первого дня (PLAN §2).
pub trait Provider: Send + Sync {
    /// Открывает стрим завершения. Отмена — drop стрима (пробрасывается в HTTP abort).
    fn stream(&self, req: CompletionRequest) -> EventStream;
}

/// Провайдер-заглушка для первого запуска, когда модель ещё не выбрана.
/// Любой промпт отвечает подсказкой открыть `/models` — не роняет TUI.
#[derive(Debug)]
pub struct Unconfigured;

impl Provider for Unconfigured {
    fn stream(&self, _req: CompletionRequest) -> EventStream {
        Box::pin(futures_util::stream::once(async {
            Err(ProviderError::MissingKey(
                "no model configured — press / and pick `models` to choose a provider".to_owned(),
            ))
        }))
    }
}

/// Фабрика из конфига. `kind` — любой id провайдера из каталога models.dev
/// (`deepseek`, `openrouter`, `xai`, `groq`, …): `base_url` и имя env-ключа
/// берутся из каталога, диалект — anthropic / gemini / openai-compat.
// ponytail: диалекты bedrock/azure/copilot — M3.5 часть 2
pub fn from_config(
    cfg: &makedo_config::ProviderConfig,
) -> Result<Box<dyn Provider>, ProviderError> {
    // Ollama — локальный, БЕЗ ключа и вне каталога models.dev: openai-compat на
    // localhost:11434/v1. Ключ-заглушка (сервер его игнорирует).
    if cfg.kind == "ollama" {
        let base = cfg.base_url.clone().unwrap_or_else(ollama_base);
        return Ok(Box::new(OpenAiCompat::new(
            "ollama".to_owned(),
            Some(base),
            &cfg.kind,
        )));
    }
    // подписка ChatGPT: собственный OAuth-вход makedo (`makedo login chatgpt`), ключа нет
    if cfg.kind == "chatgpt" {
        return Ok(Box::new(chatgpt::ChatGpt::new()?));
    }
    // CLI-делегаты — без ключа: авторизация у самого CLI (подписка)
    if cfg.kind == "claude-cli" {
        return Ok(Box::new(AgentCli::claude()));
    }
    if cfg.kind == "codex-cli" {
        return Ok(Box::new(AgentCli::codex()));
    }
    if cfg.kind == "grok-cli" {
        return Ok(Box::new(AgentCli::grok()));
    }
    let catalog = catalog::load(makedo_config::cache_dir().as_deref());
    let entry = catalog.get(&cfg.kind);
    let key_env = cfg
        .api_key_env
        .clone()
        .or_else(|| entry.and_then(|e| e.env.first().cloned()))
        .unwrap_or_else(|| match cfg.kind.as_str() {
            "anthropic" => "ANTHROPIC_API_KEY".to_owned(),
            _ => "OPENAI_API_KEY".to_owned(),
        });
    let api_key = std::env::var(&key_env)
        .ok()
        .or_else(|| makedo_config::credentials::get(&key_env))
        .ok_or_else(|| ProviderError::MissingKey(key_env.clone()))?;
    let dialect = match cfg.kind.as_str() {
        "anthropic" => "anthropic",
        "google" | "google-vertex" => "gemini",
        _ => "openai",
    };
    let base_url = cfg
        .base_url
        .clone()
        .or_else(|| entry.map(|e| e.api.clone()).filter(|a| !a.is_empty()))
        // openai-compat записи каталога дают корень API без /v1 — нормализуем
        .map(|u| {
            let u = u.trim_end_matches('/');
            if dialect == "openai" {
                normalize_openai_base(u)
            } else {
                u.to_owned()
            }
        });
    Ok(match dialect {
        "anthropic" => Box::new(Anthropic::new(api_key, base_url)),
        "gemini" => Box::new(Gemini::new(api_key, base_url)),
        _ => Box::new(OpenAiCompat::new(api_key, base_url, &cfg.kind)),
    })
}

fn normalize_openai_base(url: &str) -> String {
    let url = url.trim_end_matches('/');
    if url.ends_with("/v1") || url.ends_with("/api") {
        url.to_owned()
    } else {
        format!("{url}/v1")
    }
}

/// База OpenAI-compat API локального Ollama: `$OLLAMA_HOST` (или localhost:11434) + `/v1`.
#[must_use]
pub fn ollama_base() -> String {
    let host = std::env::var("OLLAMA_HOST")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "http://localhost:11434".to_owned());
    normalize_ollama_host(&host)
}

/// Нормализует `OLLAMA_HOST` в OpenAI-compat базу: дописывает схему и `/v1`.
fn normalize_ollama_host(host: &str) -> String {
    // OLLAMA_HOST бывает без схемы ("127.0.0.1:11434") — дописываем http://
    let with_scheme = if host.starts_with("http") {
        host.to_owned()
    } else {
        format!("http://{host}")
    };
    format!("{}/v1", with_scheme.trim_end_matches('/'))
}

/// Установленные локально модели Ollama (`GET /api/tags`). Пустой вектор, если
/// демон не запущен — вызывающая сторона показывает подсказку.
pub async fn ollama_models() -> Vec<String> {
    let url = format!("{}/api/tags", ollama_base().trim_end_matches("/v1"));
    let Ok(resp) = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
    else {
        return Vec::new();
    };
    let Ok(json) = resp.json::<serde_json::Value>().await else {
        return Vec::new();
    };
    let mut models: Vec<String> = json["models"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    models.sort();
    models
}

#[cfg(test)]
mod ollama_tests {
    use super::normalize_ollama_host;

    #[test]
    fn ollama_host_normalization() {
        // без схемы → http://, с /v1 на конце
        assert_eq!(
            normalize_ollama_host("127.0.0.1:11434"),
            "http://127.0.0.1:11434/v1"
        );
        // со схемой — не трогаем схему
        assert_eq!(
            normalize_ollama_host("http://localhost:11434"),
            "http://localhost:11434/v1"
        );
        // https и хвостовой слэш
        assert_eq!(
            normalize_ollama_host("https://box:9999/"),
            "https://box:9999/v1"
        );
    }
}
