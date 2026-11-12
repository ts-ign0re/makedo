//! MCP-клиент makedo (PLAN §3.2): rmcp, meta-tools вместо схем в манифесте
//! (стабильный KV-кэш), потолок строки stdio, рестарт с backoff.
//! Каждый сервер — слот за `tokio::Mutex`: connect под локом даёт single-flight
//! handshake без отдельной стейт-машины.
// ponytail: полная стейт-машина grok-build (Notify-парковка, generation) — если
// mutex-подход упрётся в latency на большом числе серверов

pub mod mcp_json;
pub mod meta;
pub mod transport;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use makedo_config::McpServerConfig;
use makedo_types::{QualifiedToolName, ServerName};
use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, ClientRequest, ContentBlock, ServerResult,
};
use rmcp::service::PeerRequestOptions;
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const CONNECT_CONCURRENCY: usize = 4;
/// Backoff рестарта stdio-серверов (числа grok-build).
const RESTART_BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(4),
    Duration::from_secs(16),
];

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("unknown MCP server `{0}`")]
    UnknownServer(String),
    #[error("server `{server}` is not connected: {reason}")]
    NotConnected { server: String, reason: String },
    #[error("MCP call failed: {0}")]
    Call(String),
}

/// Определение MCP-тула в каталоге. Схема НЕ попадает в манифест модели —
/// только в результаты `search_tool`.
#[derive(Debug, Clone)]
pub struct CatalogTool {
    pub qualified: QualifiedToolName,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerStatus {
    Connecting,
    Ready { tools: usize },
    Failed(String),
}

type Client = RunningService<rmcp::RoleClient, Handler>;

// large_enum_variant: слотов — единицы (по числу серверов), Box лишний
#[allow(clippy::large_enum_variant)]
enum Slot {
    Idle,
    Ready {
        client: Client,
        _guard: Option<transport::ChildGuard>,
    },
    Failed(String),
}

struct Server {
    name: ServerName,
    config: McpServerConfig,
    slot: Mutex<Slot>,
    status: std::sync::Mutex<ServerStatus>,
    restarts: AtomicU64,
}

/// Хэндлер нотификаций: `tools/list_changed` → инвалидация каталога.
#[derive(Clone)]
struct Handler {
    catalog_generation: Arc<AtomicU64>,
}

impl rmcp::ClientHandler for Handler {
    fn on_tool_list_changed(
        &self,
        _ctx: rmcp::service::NotificationContext<rmcp::RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        // коалесинг не нужен: инкремент атомарен, пересборка каталога — лениво
        // при следующем search_tool (burst из 100 нотификаций = 100 инкрементов, 1 пересборка)
        self.catalog_generation.fetch_add(1, Ordering::Relaxed);
        std::future::ready(())
    }
}

pub struct McpManager {
    servers: BTreeMap<String, Arc<Server>>,
    /// Каталог: (generation, тулы). Пересборка лениво по несовпадению generation.
    catalog: Mutex<(u64, Arc<Vec<CatalogTool>>)>,
    generation: Arc<AtomicU64>,
}

impl std::fmt::Debug for McpManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpManager")
            .field("servers", &self.servers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl McpManager {
    #[must_use]
    pub fn new(configs: &BTreeMap<String, McpServerConfig>) -> Arc<Self> {
        let generation = Arc::new(AtomicU64::new(1));
        let servers = configs
            .iter()
            .filter(|(_, c)| c.enabled)
            .filter_map(|(name, config)| {
                let server_name = ServerName::new(name.clone()).ok()?;
                Some((
                    name.clone(),
                    Arc::new(Server {
                        name: server_name,
                        config: config.clone(),
                        slot: Mutex::new(Slot::Idle),
                        status: std::sync::Mutex::new(ServerStatus::Connecting),
                        restarts: AtomicU64::new(0),
                    }),
                ))
            })
            .collect();
        Arc::new(Self {
            servers,
            catalog: Mutex::new((0, Arc::new(Vec::new()))),
            generation,
        })
    }

    /// Подключает все серверы (потолок параллелизма 4; наши машины слабее, чем у grok).
    /// Progressive: не блокирует первый промпт — вызывается фоном.
    pub async fn connect_all(self: &Arc<Self>) {
        let mut set = tokio::task::JoinSet::new();
        let sem = Arc::new(tokio::sync::Semaphore::new(CONNECT_CONCURRENCY));
        for server in self.servers.values() {
            let server = Arc::clone(server);
            let this = Arc::clone(self);
            let sem = Arc::clone(&sem);
            set.spawn(async move {
                let _permit = sem.acquire().await;
                this.ensure_connected(&server).await;
            });
        }
        while set.join_next().await.is_some() {}
    }

    /// Single-flight connect: слот под мьютексом, конкуренты ждут результата.
    async fn ensure_connected(&self, server: &Arc<Server>) {
        let mut slot = server.slot.lock().await;
        if matches!(*slot, Slot::Ready { .. }) {
            return;
        }
        *server.status.lock().unwrap_or_else(PoisonError::into_inner) = ServerStatus::Connecting;
        let timeout = Duration::from_secs(server.config.startup_timeout_s);
        let result = tokio::time::timeout(timeout, self.connect_one(server)).await;
        match result {
            Ok(Ok((client, guard, tools))) => {
                *server.status.lock().unwrap_or_else(PoisonError::into_inner) =
                    ServerStatus::Ready { tools };
                *slot = Slot::Ready {
                    client,
                    _guard: guard,
                };
                self.generation.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Err(e)) => {
                *server.status.lock().unwrap_or_else(PoisonError::into_inner) =
                    ServerStatus::Failed(e.clone());
                *slot = Slot::Failed(e);
            }
            Err(_) => {
                let e = format!("startup timeout ({}s)", server.config.startup_timeout_s);
                *server.status.lock().unwrap_or_else(PoisonError::into_inner) =
                    ServerStatus::Failed(e.clone());
                *slot = Slot::Failed(e);
            }
        }
    }

    async fn connect_one(
        &self,
        server: &Arc<Server>,
    ) -> Result<(Client, Option<transport::ChildGuard>, usize), String> {
        let handler = Handler {
            catalog_generation: Arc::clone(&self.generation),
        };
        let (client, guard) = if let Some(command) = &server.config.command {
            let (guard, pair) =
                transport::spawn_stdio(command, &server.config.env, server.config.cwd.as_deref())
                    .map_err(|e| e.to_string())?;
            let client = handler.serve(pair).await.map_err(|e| e.to_string())?;
            (client, Some(guard))
        } else if let Some(url) = &server.config.url {
            // HTTP-клиент rmcp собран с rustls без встроенного криптопровайдера:
            // без этой строки любой https-сервер падает. Повторная установка — no-op.
            let _ = rustls::crypto::ring::default_provider().install_default();
            let mut config = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url.clone());
            for (name, value) in &server.config.headers {
                config.custom_headers.insert(
                    name.parse()
                        .map_err(|_| format!("invalid HTTP header name: {name}"))?,
                    value
                        .parse()
                        .map_err(|_| format!("invalid HTTP header value for {name}"))?,
                );
            }
            for value in config.custom_headers.values_mut() {
                value.set_sensitive(true);
            }
            let t = StreamableHttpClientTransport::from_config(config);
            let client = handler.serve(t).await.map_err(|e| e.to_string())?;
            (client, None)
        } else {
            return Err("server config needs either `command` or `url`".to_owned());
        };
        let tools = client.list_all_tools().await.map_err(|e| e.to_string())?;
        Ok((client, guard, tools.len()))
    }

    /// Каталог с ленивой пересборкой по generation (никаких пересборок на запрос).
    // guard-ы слотов обязаны жить на время list_all_tools — линт функционально-скоуповый
    #[allow(clippy::significant_drop_tightening)]
    pub async fn catalog(&self) -> Arc<Vec<CatalogTool>> {
        let current = self.generation.load(Ordering::Relaxed);
        let mut cached = self.catalog.lock().await;
        if cached.0 == current {
            return Arc::clone(&cached.1);
        }
        let mut tools = Vec::new();
        for server in self.servers.values() {
            let slot = server.slot.lock().await;
            let Slot::Ready { client, .. } = &*slot else {
                continue;
            };
            let Ok(listed) = client.list_all_tools().await else {
                continue;
            };
            for t in listed {
                match QualifiedToolName::new(server.name.clone(), t.name.as_ref()) {
                    Ok(qualified) => {
                        // коллизия qualified-имён невозможна: BTreeMap серверов + инвариант имени
                        tools.push(CatalogTool {
                            qualified,
                            description: t.description.unwrap_or_default().into_owned(),
                            input_schema: serde_json::Value::Object((*t.input_schema).clone()),
                        });
                    }
                    Err(e) => {
                        // громко в статус, не молча (баг opencode)
                        *server.status.lock().unwrap_or_else(PoisonError::into_inner) =
                            ServerStatus::Failed(format!("invalid tool name skipped: {e}"));
                    }
                }
            }
        }
        *cached = (current, Arc::new(tools));
        Arc::clone(&cached.1)
    }

    /// `true`, если какие-то серверы ещё подключаются (для `status: partial`).
    #[must_use]
    pub fn any_connecting(&self) -> bool {
        self.servers.values().any(|s| {
            matches!(
                *s.status.lock().unwrap_or_else(PoisonError::into_inner),
                ServerStatus::Connecting
            )
        })
    }

    #[must_use]
    pub fn statuses(&self) -> BTreeMap<String, ServerStatus> {
        self.servers
            .iter()
            .map(|(n, s)| {
                (
                    n.clone(),
                    s.status
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone(),
                )
            })
            .collect()
    }

    /// Вызов MCP-тула. Ошибка транспорта → пометка Failed + фоновый рестарт с backoff.
    // guard слота живёт на время call_tool — сериализация с connect намеренная
    #[allow(clippy::significant_drop_tightening)]
    pub async fn call(
        self: &Arc<Self>,
        name: &QualifiedToolName,
        args: serde_json::Value,
    ) -> Result<String, McpError> {
        self.call_with_cancel(name, args, &CancellationToken::new())
            .await
    }

    #[allow(clippy::significant_drop_tightening)]
    pub async fn call_with_cancel(
        self: &Arc<Self>,
        name: &QualifiedToolName,
        args: serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<String, McpError> {
        let args = normalize_arguments(args)?;
        let server = self
            .servers
            .get(name.server().as_str())
            .ok_or_else(|| McpError::UnknownServer(name.server().as_str().to_owned()))?;
        let timeout = Duration::from_secs(server.config.tool_timeout_s);
        let result = {
            let slot = tokio::select! {
                () = cancel.cancelled() => return Err(McpError::Call("cancelled".into())),
                slot = server.slot.lock() => slot,
            };
            let Slot::Ready { client, .. } = &*slot else {
                let reason = match &*slot {
                    Slot::Failed(e) => e.clone(),
                    _ => "still connecting".to_owned(),
                };
                return Err(McpError::NotConnected {
                    server: name.server().as_str().to_owned(),
                    reason,
                });
            };
            let mut params = CallToolRequestParams::new(name.tool().to_owned());
            params.arguments = Some(args);
            let mut handle = tokio::select! {
                () = cancel.cancelled() => return Err(McpError::Call("cancelled".into())),
                result = tokio::time::timeout(timeout, client.send_cancellable_request(
                    ClientRequest::CallToolRequest(CallToolRequest::new(params)), PeerRequestOptions::no_options(),
                )) => result.map_err(|_| McpError::Call("timed out sending request".into()))?
                    .map_err(|e| McpError::Call(e.to_string()))?,
            };
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    let _ = tokio::time::timeout(Duration::from_secs(1), handle.cancel(Some("user cancelled".into()))).await;
                    return Err(McpError::Call("cancelled".into()));
                }
                () = tokio::time::sleep(timeout) => {
                    let _ = tokio::time::timeout(Duration::from_secs(1), handle.cancel(Some("tool timeout".into()))).await;
                    return Err(McpError::Call(format!("timed out after {}s", timeout.as_secs())));
                }
                response = &mut handle.rx => response.unwrap_or(Err(rmcp::service::ServiceError::TransportClosed)),
            }
        };
        match result {
            Err(e) => {
                self.schedule_restart(server);
                Err(McpError::Call(e.to_string()))
            }
            Ok(ServerResult::CallToolResult(res)) => {
                let text = render_content(&res.content);
                if res.is_error.unwrap_or(false) {
                    Err(McpError::Call(text))
                } else {
                    Ok(text)
                }
            }
            Ok(_) => Err(McpError::Call("unexpected response to tools/call".into())),
        }
    }

    /// Рестарт с backoff [1, 4, 16] c; после исчерпания — парковка в Failed
    /// (в opencode рестарта нет вообще — упавший сервер мёртв до ручного вмешательства).
    fn schedule_restart(self: &Arc<Self>, server: &Arc<Server>) {
        #[allow(clippy::cast_possible_truncation)] // рестартов — единицы
        let attempt = server.restarts.fetch_add(1, Ordering::Relaxed) as usize;
        let Some(delay) = RESTART_BACKOFF.get(attempt).copied() else {
            *server.status.lock().unwrap_or_else(PoisonError::into_inner) =
                ServerStatus::Failed("gave up after 3 restarts".to_owned());
            return;
        };
        let this = Arc::clone(self);
        let server = Arc::clone(server);
        tokio::spawn(async move {
            {
                let mut slot = server.slot.lock().await;
                *slot = Slot::Failed("restarting".to_owned());
            }
            tokio::time::sleep(delay).await;
            this.ensure_connected(&server).await;
            if matches!(
                *server.status.lock().unwrap_or_else(PoisonError::into_inner),
                ServerStatus::Ready { .. }
            ) {
                server.restarts.store(0, Ordering::Relaxed);
            }
        });
    }
}

/// Текстовые блоки склеиваются; остальное — заметкой (изображения — M3).
fn render_content(content: &[ContentBlock]) -> String {
    let mut out = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text(t) => out.push(t.text.clone()),
            other => out.push(format!(
                "[non-text content: {}]",
                serde_json::to_string(other).unwrap_or_else(|_| "?".into())
            )),
        }
    }
    out.join("\n\n")
}

/// Some small models double-encode the object. Accept only deterministic repairs.
fn normalize_arguments(
    value: serde_json::Value,
) -> Result<serde_json::Map<String, serde_json::Value>, McpError> {
    let value = match value {
        serde_json::Value::Null => serde_json::json!({}),
        serde_json::Value::String(text) => serde_json::from_str(&text)
            .map_err(|_| McpError::Call("arguments must be a JSON object".into()))?,
        value => value,
    };
    match value {
        serde_json::Value::Object(object) => Ok(object),
        _ => Err(McpError::Call("arguments must be a JSON object".into())),
    }
}
