//! Каталог провайдеров/моделей — база models.dev (механика opencode, PLAN §3.9):
//! embedded-снапшот как fallback + кэш в `~/.makedo/cache/models.json` с
//! перезагрузкой по mtime, TTL-автообновление с межпроцессным lock-файлом.
//! Снапшот несёт ЦЕНЫ КЭША (`cr`/`cw`) — из них считается реальная экономия
//! prompt-кэша в TUI (принцип: экономика пользователя — приоритет).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

const SNAPSHOT: &str = include_str!("../data/models-snapshot.json");
/// Свежесть кэша: младше часа — не обновляем (freshness-check opencode).
const FRESH: Duration = Duration::from_hours(1);
/// Lock-файл старше этого — от умершего процесса, забираем.
const LOCK_STALE: Duration = Duration::from_mins(5);

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderEntry {
    pub api: String,
    #[serde(default)]
    pub env: Vec<String>,
    pub models: BTreeMap<String, ModelInfo>,
    /// Явные уровни reasoning для конкретных моделей этого endpoint.
    #[serde(default)]
    pub effort: BTreeMap<String, crate::effort::Profile>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ModelInfo {
    /// Контекстное окно (токены); 0 = неизвестно.
    pub context: u64,
    /// Максимум output-токенов; 0 = неизвестно.
    pub output: u64,
    /// $/1M input-токенов.
    #[serde(rename = "in")]
    pub cost_in: f64,
    /// $/1M output-токенов.
    #[serde(rename = "out")]
    pub cost_out: f64,
    /// $/1M кэшированных input-токенов (cache read) — обычно в 10–50× дешевле.
    #[serde(rename = "cr", default)]
    pub cost_cache_read: f64,
    /// $/1M записи в кэш (cache write; 0 = бесплатно/неизвестно).
    #[serde(rename = "cw", default)]
    pub cost_cache_write: f64,
    /// Поддерживает tool calling.
    pub tools: bool,
}

pub type Catalog = BTreeMap<String, ProviderEntry>;

fn parse(text: &str) -> Option<Catalog> {
    serde_json::from_str(text).ok()
}

fn mtime_secs(p: &Path) -> u64 {
    p.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

fn custom_file(cache_dir: &Path) -> PathBuf {
    cache_dir.parent().unwrap_or(cache_dir).join("models.json")
}

/// Пользовательские провайдеры из `~/.makedo/models.json`: refresh их не меняет.
pub fn load_custom(cache_dir: Option<&Path>) -> Catalog {
    cache_dir
        .and_then(|d| std::fs::read_to_string(custom_file(d)).ok())
        .and_then(|text| parse(&text))
        .unwrap_or_default()
}

struct CachedCatalog {
    path: Option<PathBuf>,
    modified: (Option<SystemTime>, Option<SystemTime>),
    catalog: Arc<Catalog>,
}

/// Каталог с перезагрузкой: кэш-файл перечитывается при смене mtime
/// (фоновый refresh или другой процесс обновили — мы это видим без рестарта).
///
/// # Panics
/// Только если embedded-снапшот невалиден — невозможно по построению (тест ниже).
pub fn load(cache_dir: Option<&Path>) -> Arc<Catalog> {
    static CACHE: Mutex<Option<CachedCatalog>> = Mutex::new(None);
    let cache_file = cache_dir.map(|d| d.join("models.json"));
    let modified = |path: &Path| path.metadata().and_then(|m| m.modified()).ok();
    let current_mtime = (
        cache_file.as_deref().and_then(modified),
        cache_dir.and_then(|d| modified(&custom_file(d))),
    );
    let mut guard = match CACHE.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(cached) = guard.as_ref()
        && cached.path == cache_file
        && cached.modified == current_mtime
    {
        return Arc::clone(&cached.catalog);
    }
    let loaded = cache_file
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| parse(&t));
    #[allow(clippy::expect_used)] // embedded снапшот валиден по построению
    let mut catalog =
        loaded.unwrap_or_else(|| parse(SNAPSHOT).expect("embedded models snapshot is valid"));
    catalog.extend(load_custom(cache_dir));
    let catalog = Arc::new(catalog);
    *guard = Some(CachedCatalog {
        path: cache_file,
        modified: current_mtime,
        catalog: Arc::clone(&catalog),
    });
    catalog
}

/// Поиск модели: сначала у названного `provider`, затем по всему каталогу —
/// канонический провайдер (id входит в имя модели) предпочитается агрегаторам.
#[must_use]
pub fn find_model<'a>(
    catalog: &'a Catalog,
    provider: Option<&'a str>,
    model: &str,
) -> Option<(&'a str, ModelInfo)> {
    if let Some(p) = provider
        && let Some(m) = catalog.get(p).and_then(|e| e.models.get(model))
    {
        return Some((p, *m));
    }
    let canonical = catalog
        .iter()
        .find(|(pid, e)| model.contains(pid.as_str()) && e.models.contains_key(model));
    canonical
        .or_else(|| catalog.iter().find(|(_, e)| e.models.contains_key(model)))
        .and_then(|(pid, e)| e.models.get(model).map(|m| (pid.as_str(), *m)))
}

/// Кэш достаточно свеж? (по mtime, как у opencode)
#[must_use]
pub fn is_fresh(cache_dir: &Path) -> bool {
    let m = mtime_secs(&cache_dir.join("models.json"));
    if m == 0 {
        return false;
    }
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(m))
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < FRESH)
}

/// Межпроцессный lock через `create_new` lock-файла (два makedo не качают разом).
/// `false` = лок держит другой живой процесс — просто пропускаем refresh.
fn try_lock(cache_dir: &Path) -> Option<LockGuard> {
    let path = cache_dir.join(".models.lock");
    std::fs::create_dir_all(cache_dir).ok()?;
    for _ in 0..2 {
        if std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .is_ok()
        {
            return Some(LockGuard { path });
        }
        let age = SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_secs(mtime_secs(&path)))
            .and_then(|t| SystemTime::now().duration_since(t).ok());
        if age.is_some_and(|a| a > LOCK_STALE) {
            let _ = std::fs::remove_file(&path); // лок мёртвого процесса
            continue;
        }
        return None;
    }
    None
}

struct LockGuard {
    path: std::path::PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Фоновое обновление: сразу (если кэш несвеж) и далее раз в час.
/// Ошибки сети молча игнорируются — embedded-снапшот всегда есть.
pub fn spawn_background_refresh(cache_dir: std::path::PathBuf) {
    tokio::spawn(async move {
        loop {
            if !is_fresh(&cache_dir) {
                let _ = refresh(&cache_dir).await;
            }
            tokio::time::sleep(Duration::from_hours(1)).await;
        }
    });
}

/// Обновляет кэш с models.dev (фоновое обновление и `makedo models --refresh`).
pub async fn refresh(cache_dir: &Path) -> Result<usize, crate::ProviderError> {
    #[derive(Deserialize)]
    struct FullModel {
        #[serde(default)]
        limit: Limits,
        #[serde(default)]
        cost: Costs,
        #[serde(default)]
        tool_call: bool,
    }
    #[derive(Deserialize, Default)]
    struct Limits {
        #[serde(default)]
        context: u64,
        #[serde(default)]
        output: u64,
    }
    #[derive(Deserialize, Default)]
    struct Costs {
        #[serde(default)]
        input: f64,
        #[serde(default)]
        output: f64,
        #[serde(default)]
        cache_read: f64,
        #[serde(default)]
        cache_write: f64,
    }
    #[derive(Deserialize)]
    struct FullProvider {
        #[serde(default)]
        api: String,
        #[serde(default)]
        env: Vec<String>,
        #[serde(default)]
        models: BTreeMap<String, FullModel>,
    }
    let Some(_lock) = try_lock(cache_dir) else {
        return Ok(0); // другой процесс уже качает
    };
    let text = reqwest::get("https://models.dev/api.json")
        .await?
        .error_for_status()?
        .text()
        .await?;
    let full: BTreeMap<String, FullProvider> = serde_json::from_str(&text)
        .map_err(|e| crate::ProviderError::Protocol(format!("models.dev parse: {e}")))?;
    let mut count = 0usize;
    let mut compact = serde_json::Map::new();
    for (pid, p) in full {
        if p.models.is_empty() {
            continue;
        }
        let mut models = serde_json::Map::new();
        for (mid, m) in p.models {
            count += 1;
            models.insert(
                mid,
                serde_json::json!({
                    "context": m.limit.context, "output": m.limit.output,
                    "in": m.cost.input, "out": m.cost.output,
                    "cr": m.cost.cache_read, "cw": m.cost.cache_write,
                    "tools": m.tool_call,
                }),
            );
        }
        compact.insert(
            pid,
            serde_json::json!({"api": p.api, "env": p.env, "models": models}),
        );
    }
    // атомарная запись: tmp + rename (читатели не видят полфайла)
    let tmp = cache_dir.join(format!(".models.{}.tmp", std::process::id()));
    let final_path = cache_dir.join("models.json");
    std::fs::write(&tmp, serde_json::to_vec(&compact).unwrap_or_default())
        .and_then(|()| std::fs::rename(&tmp, &final_path))
        .map_err(|e| crate::ProviderError::Protocol(format!("cache write: {e}")))?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn embedded_snapshot_has_cache_prices() {
        let cat = parse(SNAPSHOT).unwrap();
        assert!(cat.len() > 100);
        let (pid, info) = find_model(&cat, None, "deepseek-v4-flash").unwrap();
        assert_eq!(pid, "deepseek");
        assert!(info.context >= 100_000);
        assert!(info.tools);
        // цены кэша — ядро экономики: cache read радикально дешевле input
        assert!(info.cost_cache_read > 0.0);
        assert!(info.cost_cache_read < info.cost_in / 10.0);
    }

    #[test]
    fn lock_excludes_and_recovers_stale() {
        let d = std::env::temp_dir().join(format!("makedo-cat-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let g1 = try_lock(&d);
        assert!(g1.is_some());
        assert!(
            try_lock(&d).is_none(),
            "второй лок при живом первом не даётся"
        );
        drop(g1);
        assert!(try_lock(&d).is_some(), "после drop лок снова доступен");
    }

    #[test]
    fn load_reloads_on_mtime_change() {
        let d = std::env::temp_dir().join(format!("makedo-cat-reload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        // без кэш-файла — embedded
        let c1 = load(Some(&d));
        assert!(c1.len() > 100);
        // подкладываем кэш с одним провайдером и другим mtime
        std::fs::write(
            d.join("models.json"),
            r#"{"test-prov":{"api":"https://t","env":["T_KEY"],"models":{"m1":{"context":1,"output":1,"in":0,"out":0,"cr":0,"cw":0,"tools":true}}}}"#,
        )
        .unwrap();
        let c2 = load(Some(&d));
        assert!(c2.contains_key("test-prov"), "кэш-файл перечитан по mtime");
    }

    #[test]
    fn custom_provider_survives_cache_replacement_and_reloads() {
        let root = std::env::temp_dir().join(format!("makedo-custom-cat-{}", std::process::id()));
        let cache = root.join("cache");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&cache).unwrap();
        assert!(!load(Some(&cache)).contains_key("qwen-vast"));
        let custom = root.join("models.json");
        std::fs::write(&custom, r#"{"qwen-vast":{"api":"http://127.0.0.1:8000/v1","env":["QWEN_VAST_TOKEN"],"models":{"qwen3.8":{"context":65536,"output":8192,"in":0,"out":0,"tools":true}}}}"#).unwrap();
        assert!(load(Some(&cache)).contains_key("qwen-vast"));
        // Refresh заменяет скачанный кэш целиком, пользовательская запись остаётся.
        std::fs::write(cache.join("models.json"), "{}").unwrap();
        let catalog = load(Some(&cache));
        let entry = &catalog["qwen-vast"];
        assert_eq!(entry.api, "http://127.0.0.1:8000/v1");
        assert_eq!(entry.env, ["QWEN_VAST_TOKEN"]);
        assert_eq!(
            find_model(&catalog, Some("qwen-vast"), "qwen3.8")
                .unwrap()
                .1
                .context,
            65536
        );
        std::fs::remove_file(custom).unwrap();
        assert!(!load(Some(&cache)).contains_key("qwen-vast"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
