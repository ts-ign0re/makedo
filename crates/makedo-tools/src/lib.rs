//! Tool runtime: типизированный трейт `Tool` (RPITIT, ноль боксинга на типизированном
//! пути), erasure-граница `ToolDyn`, реестр с кэшем схем, трункация со спиллом.
//! PLAN §3.1.

mod artifact;
pub mod builtin;
pub mod executor;
pub mod registry;
pub mod truncate;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

/// Контекст вызова. Отмена — реально подключённый `CancellationToken`
/// (grok-build построил и не использовал; у нас подключение — часть `DoD` M0).
#[derive(Debug, Clone)]
pub struct ToolCtx {
    /// UI-only events; never inserted into the model transcript.
    pub progress: Option<Progress>,
    pub cancel: CancellationToken,
    pub cwd: PathBuf,
    /// Директория спиллов больших выводов (`~/.makedo/spill/<session>`).
    pub spill_dir: PathBuf,
    /// Директория шимов (`rg`/`fd` → makedo) — подмешивается в PATH bash-тула.
    pub shim_dir: Option<PathBuf>,
    /// Потолок захвата вывода команды в память (из `Budget`); None = дефолт тула.
    pub capture_bytes: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub events: tokio::sync::mpsc::Sender<makedo_types::event::SessionEvent>,
    pub call_id: String,
}

impl ToolCtx {
    /// Контекст без UI-прогресса, шимов и лимита захвата: CLI-команды и тесты.
    #[must_use]
    pub fn new(cwd: PathBuf, spill_dir: PathBuf) -> Self {
        Self {
            progress: None,
            cancel: CancellationToken::new(),
            cwd,
            spill_dir,
            shim_dir: None,
            capture_bytes: None,
        }
    }

    pub async fn emit(&self, event: makedo_types::event::SessionEvent) {
        if let Some(progress) = &self.progress {
            let _ = progress.events.send(event).await;
        }
    }
}

/// Ошибки тулов. Текст, который увидит модель, — это API (PLAN §1.1):
/// менять формулировки можно только осознанно.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// Невалидные аргументы — модель получает текст и переписывает вызов.
    #[error("Invalid arguments: {0}. Rewrite the tool call to satisfy the schema.")]
    InvalidArgs(String),
    /// Ошибка исполнения, видимая модели (файл не найден, команда упала, …).
    #[error("{0}")]
    Failed(String),
    #[error("Tool execution was cancelled")]
    Cancelled,
}

/// Типизированный трейт. `run` — RPITIT с явным `Send`-баундом.
pub trait Tool: Send + Sync + 'static {
    type Args: DeserializeOwned + JsonSchema + Send;

    /// Проверяется один раз при регистрации (`Registry::register`).
    fn name(&self) -> &'static str;
    fn description(&self) -> &str;
    /// Read-only calls can run together unless a batch also writes the same path.
    fn is_read_only(&self) -> bool {
        false
    }
    fn run(
        &self,
        ctx: &ToolCtx,
        args: Self::Args,
    ) -> impl Future<Output = Result<String, ToolError>> + Send;
    // ponytail: вывод — String; типизированный Output + ToolStream — когда появится
    // второй тул с прогрессом (PLAN §3.1)

    /// File paths read or written by this call, used to serialize conflicting calls.
    fn access_paths(&self, _args: &Self::Args) -> Vec<PathBuf> {
        Vec::new()
    }
}

type DynFuture<'a> = Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>>;

/// Object-safe erasure-граница: JSON-декод аргументов + вызов типизированного `run`.
pub trait ToolDyn: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &str;
    fn is_read_only(&self) -> bool;
    fn run_json<'a>(&'a self, ctx: &'a ToolCtx, args: serde_json::Value) -> DynFuture<'a>;
    fn access_paths_json(&self, args: &serde_json::Value) -> Vec<PathBuf>;
}

impl<T: Tool> ToolDyn for T {
    fn name(&self) -> &'static str {
        Tool::name(self)
    }

    fn description(&self) -> &str {
        Tool::description(self)
    }

    fn is_read_only(&self) -> bool {
        Tool::is_read_only(self)
    }

    fn run_json<'a>(&'a self, ctx: &'a ToolCtx, args: serde_json::Value) -> DynFuture<'a> {
        Box::pin(async move {
            let args: T::Args =
                serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
            self.run(ctx, args).await
        })
    }

    fn access_paths_json(&self, args: &serde_json::Value) -> Vec<PathBuf> {
        serde_json::from_value::<T::Args>(args.clone())
            .map(|a| self.access_paths(&a))
            .unwrap_or_default()
    }
}
