//! Батч-исполнитель тул-коллов: потолок параллелизма `min(4, cores)` (нет ни у
//! grok-build, ни у opencode — на 8 GB обязателен), зависимости по canonical path
//! в порядке коллов (исключают deadlock), отмена через `CancellationToken`,
//! трункация каждого результата. PLAN §3.1.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use makedo_types::event::SessionEvent;
use tokio::sync::{Semaphore, watch};

use crate::registry::Registry;
use crate::{ToolCtx, ToolError, truncate};

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Debug)]
pub struct CallResult {
    pub call_id: String,
    /// Текст для модели: вывод тула либо model-facing текст ошибки.
    pub output: String,
    pub is_error: bool,
    pub spill_path: Option<PathBuf>,
}

/// Canonicalize before collapsing `..`: a symlink's parent is its target's parent.
pub fn lock_path(cwd: &std::path::Path, path: &std::path::Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    let mut resolved = loop {
        if let Ok(real) = ancestor.canonicalize() {
            break real;
        }
        let Some(part) = ancestor.components().next_back() else {
            break absolute.clone();
        };
        suffix.push(part.as_os_str().to_owned());
        let Some(parent) = ancestor.parent() else {
            break absolute.clone();
        };
        ancestor = parent;
    };
    for part in suffix.into_iter().rev() {
        if part == ".." {
            resolved.pop();
        } else if part != "." {
            resolved.push(part);
        }
    }
    resolved
}

/// Выполняет батч. Результаты — в порядке коллов (пары `tool_use`/`tool_result`
/// не рвутся: каждый колл получает результат, даже при отмене).
#[allow(clippy::too_many_lines)] // One ordered batch lifecycle: dependencies, live events, ordered results.
pub async fn run_batch(
    registry: Arc<Registry>,
    ctx: &ToolCtx,
    calls: Vec<ToolCall>,
    concurrency: usize,
) -> Vec<CallResult> {
    let identities: Vec<_> = calls
        .iter()
        .map(|c| (c.call_id.clone(), c.name.clone()))
        .collect();
    // Конфликтуют доступы к путям, которые хоть один не-read-only колл пишет.
    let mut path_locks = std::collections::HashSet::new();
    let mut call_paths: Vec<Vec<PathBuf>> = Vec::with_capacity(calls.len());
    for call in &calls {
        let paths: Vec<_> = registry
            .get(&call.name)
            .map_or_else(Vec::new, |tool| tool.access_paths_json(&call.args))
            .iter()
            .map(|p| lock_path(&ctx.cwd, p))
            .collect();
        if registry.get(&call.name).is_some_and(|t| !t.is_read_only()) {
            for p in &paths {
                path_locks.insert(p.clone());
            }
        }
        call_paths.push(paths);
    }

    let sem = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut join = tokio::task::JoinSet::new();
    let mut preceding: HashMap<PathBuf, watch::Receiver<bool>> = HashMap::new();
    for (idx, (call, mut paths)) in calls.into_iter().zip(call_paths).enumerate() {
        let registry = Arc::clone(&registry);
        let mut ctx = ctx.clone();
        if let Some(progress) = &mut ctx.progress {
            progress.call_id.clone_from(&call.call_id);
        }
        ctx.emit(SessionEvent::ToolQueued {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
        })
        .await;
        let sem = Arc::clone(&sem);
        // Deduplicate aliases before building backward-only dependencies.
        paths.sort();
        paths.dedup();
        let (done_tx, done_rx) = watch::channel(false);
        let mut dependencies = Vec::new();
        for path in paths.into_iter().filter(|p| path_locks.contains(p)) {
            if let Some(previous) = preceding.insert(path, done_rx.clone()) {
                dependencies.push(previous);
            }
        }
        join.spawn(async move {
            // Dependencies point only backwards in call order. Unrelated files stay parallel.
            for mut previous in dependencies {
                if !*previous.borrow() {
                    tokio::select! {
                        _ = previous.changed() => {},
                        () = ctx.cancel.cancelled() => {},
                    }
                }
            }
            let result = run_one(&registry, &ctx, &call, &sem).await;
            let _ = done_tx.send(true);
            (idx, call.call_id, call.name, result)
        });
    }

    let mut slots: Vec<Option<CallResult>> = Vec::new();
    slots.resize_with(join.len(), || None);
    while let Some(res) = join.join_next().await {
        if let Ok((idx, call_id, name, result)) = res {
            let out = match result {
                Ok(text) => {
                    let log = (name == "bash")
                        .then(|| crate::builtin::shell::output_log(&text))
                        .flatten();
                    let t = truncate::output(text, &ctx.spill_dir, &call_id);
                    CallResult {
                        call_id,
                        output: t.text,
                        is_error: false,
                        spill_path: log.or(t.spill_path),
                    }
                }
                Err(e) => {
                    let output = e.to_string();
                    let spill_path = (name == "bash")
                        .then(|| crate::builtin::shell::output_log(&output))
                        .flatten();
                    CallResult {
                        call_id,
                        output,
                        is_error: true,
                        spill_path,
                    }
                }
            };
            publish(ctx, &name, &out).await;
            slots[idx] = Some(out);
        }
        // Err(join) = паника таски; слот останется None и станет interrupted-результатом
    }
    let mut results = Vec::with_capacity(slots.len());
    for (slot, (call_id, name)) in slots.into_iter().zip(identities) {
        let result = if let Some(result) = slot {
            result
        } else {
            let result = CallResult {
                call_id,
                output: "Tool execution interrupted".into(),
                is_error: true,
                spill_path: None,
            };
            publish(ctx, &name, &result).await;
            result
        };
        results.push(result);
    }
    results
}

async fn publish(ctx: &ToolCtx, name: &str, result: &CallResult) {
    if let Some(path) = &result.spill_path {
        ctx.emit(SessionEvent::ToolLog {
            call_id: result.call_id.clone(),
            name: name.into(),
            path: path.display().to_string(),
        })
        .await;
    }
    ctx.emit(SessionEvent::ToolFinished {
        call_id: result.call_id.clone(),
        name: name.into(),
        is_error: result.is_error,
        preview: summarize(name, &result.output, result.is_error),
    })
    .await;
}

// линт функционально-скоуповый: guards и Arc тула обязаны жить до конца run_json
#[allow(clippy::significant_drop_tightening)]
async fn run_one(
    registry: &Registry,
    ctx: &ToolCtx,
    call: &ToolCall,
    sem: &Semaphore,
) -> Result<String, ToolError> {
    let tool = registry.get(&call.name);
    let Some(tool) = tool else {
        return Err(ToolError::InvalidArgs(format!(
            "Unknown tool `{}`. Use only tools from the manifest.",
            call.name
        )));
    };
    let _permit = sem.acquire().await.map_err(|_| ToolError::Cancelled)?;
    // отмена — ответственность самого тула через ctx.cancel: внешний select с
    // biased-cancel дропнул бы future тула в точке await, не дав bash убить свою
    // process group (утечка процессов). bash/grep/glob слушают ctx.cancel сами.
    if ctx.cancel.is_cancelled() {
        return Err(ToolError::Cancelled);
    }
    ctx.emit(SessionEvent::ToolStarted {
        call_id: call.call_id.clone(),
        name: call.name.clone(),
    })
    .await;
    tool.run_json(ctx, call.args.clone()).await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::Tool;
    use schemars::JsonSchema;
    use serde::Deserialize;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static PEAK: AtomicUsize = AtomicUsize::new(0);
    static RUNNING: AtomicUsize = AtomicUsize::new(0);

    #[derive(Debug, Deserialize, JsonSchema)]
    struct SlowArgs {
        #[serde(default)]
        path: Option<String>,
    }

    struct Slow {
        read_only: bool,
    }
    impl Tool for Slow {
        type Args = SlowArgs;
        fn name(&self) -> &'static str {
            if self.read_only { "slow_ro" } else { "slow_w" }
        }
        fn description(&self) -> &'static str {
            "test"
        }
        fn is_read_only(&self) -> bool {
            self.read_only
        }
        async fn run(&self, _ctx: &ToolCtx, _args: SlowArgs) -> Result<String, ToolError> {
            let now = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
            PEAK.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            RUNNING.fetch_sub(1, Ordering::SeqCst);
            Ok("done".into())
        }
        fn access_paths(&self, args: &SlowArgs) -> Vec<PathBuf> {
            args.path.iter().map(PathBuf::from).collect()
        }
    }

    fn ctx() -> ToolCtx {
        ToolCtx::new(
            std::env::temp_dir(),
            std::env::temp_dir().join("makedo-exec-spill"),
        )
    }

    #[tokio::test]
    async fn semaphore_caps_parallelism_and_write_locks_serialize() {
        let mut r = Registry::default();
        r.register(Slow { read_only: true }).unwrap();
        r.register(Slow { read_only: false }).unwrap();
        let r = Arc::new(r);
        // 6 read-only коллов, потолок 2 → пик ≤ 2
        let calls: Vec<ToolCall> = (0..6)
            .map(|i| ToolCall {
                call_id: format!("c{i}"),
                name: "slow_ro".into(),
                args: serde_json::json!({}),
            })
            .collect();
        let out = run_batch(Arc::clone(&r), &ctx(), calls, 2).await;
        assert_eq!(out.len(), 6);
        assert!(out.iter().all(|o| !o.is_error));
        assert!(PEAK.load(Ordering::SeqCst) <= 2);

        // два write-колла в один path сериализуются даже при потолке 4
        PEAK.store(0, Ordering::SeqCst);
        let calls: Vec<ToolCall> = (0..2)
            .map(|i| ToolCall {
                call_id: format!("w{i}"),
                name: "slow_w".into(),
                args: serde_json::json!({"path": "/tmp/same-file"}),
            })
            .collect();
        let out = run_batch(r, &ctx(), calls, 4).await;
        assert!(out.iter().all(|o| !o.is_error));
        assert_eq!(PEAK.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unknown_tool_is_model_facing_error() {
        let r = Arc::new(Registry::default());
        let out = run_batch(
            r,
            &ctx(),
            vec![ToolCall {
                call_id: "x".into(),
                name: "nope".into(),
                args: serde_json::json!({}),
            }],
            2,
        )
        .await;
        assert!(out[0].is_error);
        assert!(out[0].output.contains("Unknown tool"));
    }

    #[tokio::test]
    async fn cancellation_fails_pending_calls() {
        let mut r = Registry::default();
        r.register(Slow { read_only: true }).unwrap();
        let r = Arc::new(r);
        let c = ctx();
        c.cancel.cancel();
        let out = run_batch(
            r,
            &c,
            vec![ToolCall {
                call_id: "c".into(),
                name: "slow_ro".into(),
                args: serde_json::json!({}),
            }],
            2,
        )
        .await;
        assert!(out[0].is_error);
        assert!(out[0].output.contains("cancelled"));
    }
}

#[cfg(test)]
mod alias_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[tokio::test]
    async fn aliases_and_reads_observe_call_order() {
        let root = std::env::temp_dir().join(format!("makedo-alias-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("file"), "before").unwrap();
        let _ = std::fs::remove_file(root.join("alias"));
        std::os::unix::fs::symlink(root.join("file"), root.join("alias")).unwrap();
        assert_eq!(
            lock_path(&root, std::path::Path::new("file")),
            lock_path(&root, &root.join("alias"))
        );
        let mut registry = Registry::default();
        crate::builtin::register_all(&mut registry).unwrap();
        let ctx = ToolCtx::new(root.clone(), root.join("spill"));
        let read = registry
            .get("read")
            .unwrap()
            .run_json(&ctx, serde_json::json!({"file_path":"file"}))
            .await
            .unwrap();
        let version = read
            .lines()
            .next()
            .unwrap()
            .strip_prefix("version=")
            .unwrap();
        let calls = vec![
            ToolCall {
                call_id: "write".into(),
                name: "write".into(),
                args: serde_json::json!({"file_path":"./file","content":"after", "expected_version": version}),
            },
            ToolCall {
                call_id: "read".into(),
                name: "read".into(),
                args: serde_json::json!({"file_path":root.join("alias")}),
            },
        ];
        let result = run_batch(Arc::new(registry), &ctx, calls, 4).await;
        assert!(!result[0].is_error);
        assert!(result[1].output.contains("after"));
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// Компактное превью тула для TUI (grok-стиль): у длинного вывода — сводка-счётчик
/// (`220 lines`, `12 matches`), у короткого — первая строка как есть, у ошибки —
/// само сообщение. Иначе TUI заливается сырым `ls`/`read`/`bash`.
pub fn summarize(name: &str, output: &str, is_error: bool) -> String {
    let cut = |s: &str, n: usize| -> String {
        if s.chars().count() <= n {
            s.to_owned()
        } else {
            let mut o: String = s.chars().take(n.saturating_sub(1)).collect();
            o.push('…');
            o
        }
    };
    let trimmed = output.trim_end();
    if trimmed.trim().is_empty() {
        return if is_error {
            "error".into()
        } else {
            "ok".into()
        };
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    let first = lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(&"")
        .trim();
    // ошибка — показываем само сообщение (первую содержательную строку)
    if is_error {
        return cut(first, 72);
    }
    // короткий одностроковый вывод — как есть
    if lines.len() <= 1 {
        return cut(first, 72);
    }
    // многострочный — сводка-счётчик по типу тула
    let n = lines.len();
    match name {
        "read" | "cat" | "bash" => format!("{n} lines"),
        "grep" | "rg" => format!("{n} matches"),
        "glob" | "fd" | "ls" => format!("{n} entries"),
        // прочее многострочное — первая строка + сколько ещё
        _ => format!("{} (+{} more)", cut(first, 48), n - 1),
    }
}

#[cfg(test)]
mod progress_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::{Progress, Tool};
    use schemars::JsonSchema;
    use serde::Deserialize;
    use std::time::Duration;

    #[derive(Deserialize, JsonSchema)]
    struct DelayArgs {
        ms: u64,
    }
    struct Delay;
    impl Tool for Delay {
        type Args = DelayArgs;
        fn name(&self) -> &'static str {
            "delay"
        }
        fn description(&self) -> &'static str {
            "fixture"
        }
        async fn run(&self, _ctx: &ToolCtx, args: DelayArgs) -> Result<String, ToolError> {
            tokio::time::sleep(Duration::from_millis(args.ms)).await;
            Ok("ok".into())
        }
    }

    #[tokio::test]
    async fn completions_are_live_but_model_results_remain_in_call_order() {
        let (events, mut receiver) = tokio::sync::mpsc::channel(32);
        let ctx = ToolCtx {
            progress: Some(Progress {
                events,
                call_id: String::new(),
            }),
            cancel: tokio_util::sync::CancellationToken::new(),
            cwd: std::env::temp_dir(),
            spill_dir: std::env::temp_dir(),
            shim_dir: None,
            capture_bytes: None,
        };
        let mut registry = Registry::default();
        registry.register(Delay).unwrap();
        let calls = [("slow", 200), ("fast", 1)]
            .into_iter()
            .map(|(id, ms)| ToolCall {
                call_id: id.into(),
                name: "delay".into(),
                args: serde_json::json!({"ms":ms}),
            })
            .collect();
        let batch =
            tokio::spawn(async move { run_batch(Arc::new(registry), &ctx, calls, 2).await });
        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            if let SessionEvent::ToolFinished { call_id, .. } = event {
                assert_eq!(call_id, "fast");
                assert!(
                    !batch.is_finished(),
                    "completion waited for the entire batch"
                );
                break;
            }
        }
        let results = batch.await.unwrap();
        assert_eq!(results[0].call_id, "slow");
        assert_eq!(results[1].call_id, "fast");
    }
}
