//! File-backed command execution shared by bash and the verification gate.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::Deserialize;

use crate::{Tool, ToolCtx, ToolError};

pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
const PREVIEW_BYTES: usize = 8_000;
const KILL_GRACE: Duration = Duration::from_secs(3);

#[derive(Debug, Default, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    /// Return OK/FAIL and a log path; use for builds, tests and application runs.
    #[default]
    Status,
    /// Return bounded output too; use when the output itself is needed (rg, git diff).
    Content,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BashArgs {
    pub command: String,
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub output: OutputMode,
    /// Use machine-wide heavy slots. Defaults to true for status, false for content.
    pub heavy: Option<bool>,
}

#[derive(Debug)]
pub struct BashTool;

#[derive(Debug, PartialEq, Eq)]
pub enum CommandStatus {
    Exited(Option<i32>),
    TimedOut,
    Cancelled,
    BackgroundStopped,
}

#[derive(Debug)]
pub struct CommandResult {
    pub status: CommandStatus,
    pub log: PathBuf,
    pub elapsed: Duration,
    pub heavy_used: bool,
    pub heavy_requested: bool,
}

impl CommandResult {
    pub fn success(&self) -> bool {
        self.status == CommandStatus::Exited(Some(0))
    }

    pub fn summary(&self) -> String {
        let status = match self.status {
            CommandStatus::Exited(Some(0)) => "OK | exit=0".to_owned(),
            CommandStatus::Exited(Some(code)) => format!("FAIL | exit={code}"),
            CommandStatus::Exited(None) => "FAIL | terminated by signal".to_owned(),
            CommandStatus::TimedOut => "TIMEOUT | command killed".to_owned(),
            CommandStatus::Cancelled => "CANCELLED | command killed".to_owned(),
            CommandStatus::BackgroundStopped => {
                "FAIL | background children outlived shell and were killed; use foreground commands"
                    .to_owned()
            }
        };
        let heavy = if self.heavy_used {
            " | heavy=on"
        } else if self.heavy_requested {
            " | heavy=unavailable"
        } else {
            ""
        };
        format!(
            "{status} | {:.1}s | log={}{}",
            self.elapsed.as_secs_f64(),
            serde_json::to_string(&self.log.to_string_lossy()).unwrap_or_default(),
            heavy
        )
    }
}

/// Decode the log field in the runner's first-line status, before any content preview.
pub fn output_log(text: &str) -> Option<PathBuf> {
    let (_, value) = text.lines().next()?.split_once(" | log=")?;
    let path = serde_json::Deserializer::from_str(value)
        .into_iter::<String>()
        .next()?
        .ok()?;
    Some(PathBuf::from(path))
}

/// Resolve once per run from PATH, including explicit user installations.
pub fn heavy_binary() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|p| p.join("heavy"))
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// stdout/stderr share a file from spawn onwards, including on timeout/cancel.
/// No pipe capture or output-sized allocation; heavy coordinates other checkouts too.
pub async fn run_command(
    ctx: &ToolCtx,
    command: &str,
    timeout: Duration,
    heavy: bool,
) -> Result<CommandResult, ToolError> {
    let (log, file) = crate::artifact::create(&ctx.spill_dir.join("commands"), "command", "log")
        .map_err(|e| ToolError::Failed(format!("Cannot create command log: {e}")))?;
    let err = file
        .try_clone()
        .map_err(|e| ToolError::Failed(format!("Cannot clone log handle: {e}")))?;
    let heavy_bin = heavy.then(heavy_binary).flatten();
    let mut cmd = if let Some(bin) = &heavy_bin {
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(["--", "sh", "-c"]);
        cmd
    } else {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c");
        cmd
    };
    if let Some(shims) = &ctx.shim_dir {
        cmd.env(
            "PATH",
            format!(
                "{}:{}",
                shims.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
    }
    let start = Instant::now();
    let mut child = cmd
        .arg(command)
        .current_dir(&ctx.cwd)
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(err)
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            ToolError::Failed(format!("Cannot spawn command: {e}; log={}", log.display()))
        })?;
    let pid = child.id().unwrap_or(0);
    let status = tokio::select! {
        biased;
        () = ctx.cancel.cancelled() => {
            terminate(pid, &mut child).await;
            CommandStatus::Cancelled
        }
        () = tokio::time::sleep(timeout) => {
            terminate(pid, &mut child).await;
            CommandStatus::TimedOut
        }
        res = child.wait() => {
            let code = res.map_err(|e| ToolError::Failed(format!("wait failed: {e}; log={}", log.display())))?.code();
            let background = pid != 0 && std::process::Command::new("kill").args(["-0", &format!("-{pid}")]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
            if background { terminate(pid, &mut child).await; CommandStatus::BackgroundStopped }
            else { CommandStatus::Exited(code) }
        },
    };
    Ok(CommandResult {
        status,
        log,
        elapsed: start.elapsed(),
        heavy_used: heavy_bin.is_some(),
        heavy_requested: heavy,
    })
}

fn preview(path: &Path, cap: usize) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let cap = cap.min(PREVIEW_BYTES);
    let start = size.saturating_sub(cap as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(cap as u64).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(if start > 0 {
        format!("[last {cap} bytes; search the log for earlier output]\n{text}")
    } else {
        text.into_owned()
    })
}

impl Tool for BashTool {
    type Args = BashArgs;
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &'static str {
        "Run foreground sh -c in cwd (detached children are terminated). ALL stdout/stderr goes to a log file. Default output=status: OK/FAIL, exit and log path only; use for builds/tests/application runs. On failure grep the log then read relevant lines. Use output=content for data queries (rg, git diff); returns at most 8000 bytes. Heavy machine-wide slots are used by default for status mode when heavy is installed; set heavy=true for any costly command. Timeout 120s, max 600s."
    }
    async fn run(&self, ctx: &ToolCtx, args: BashArgs) -> Result<String, ToolError> {
        let result = run_command(
            ctx,
            &args.command,
            Duration::from_millis(
                args.timeout_ms
                    .unwrap_or(DEFAULT_TIMEOUT_MS)
                    .min(MAX_TIMEOUT_MS),
            ),
            args.heavy.unwrap_or(args.output == OutputMode::Status),
        )
        .await?;
        let mut text = result.summary();
        if args.output == OutputMode::Content {
            let content = preview(&result.log, ctx.capture_bytes.unwrap_or(PREVIEW_BYTES))
                .map_err(|e| ToolError::Failed(format!("{text}; cannot read log: {e}")))?;
            text.push('\n');
            text.push_str(&content);
        }
        if result.success() {
            Ok(text)
        } else {
            Err(ToolError::Failed(text))
        }
    }
}

async fn terminate(pid: u32, child: &mut tokio::process::Child) {
    if pid == 0 {
        let _ = child.kill().await;
        return;
    }
    let signal = |sig: &str| {
        let _ = std::process::Command::new("kill")
            .args([sig, &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    };
    signal("-TERM");
    // Reap the leader, but still kill remaining descendants after the grace period.
    let _ = tokio::time::timeout(KILL_GRACE, child.wait()).await;
    signal("-KILL");
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::large_futures)]
    use super::*;
    fn ctx() -> ToolCtx {
        ToolCtx {
            capture_bytes: Some(1024),
            ..ToolCtx::new(
                std::env::temp_dir(),
                std::env::temp_dir().join("makedo-command-tests"),
            )
        }
    }
    #[tokio::test]
    async fn preserves_full_output_but_returns_only_status() {
        let c = ctx();
        let r = run_command(
            &c,
            "printf begin; head -c 6000000 /dev/zero; printf end >&2",
            Duration::from_secs(10),
            false,
        )
        .await
        .unwrap();
        assert!(r.success());
        assert!(std::fs::metadata(&r.log).unwrap().len() > 6_000_000);
        assert!(!r.summary().contains("begin"));
        assert!(preview(&r.log, 1024).unwrap().ends_with("end"));
        std::fs::remove_file(r.log).unwrap();
    }
    #[tokio::test]
    async fn failures_and_timeout_keep_diagnostics() {
        let c = ctx();
        let r = run_command(
            &c,
            "echo failure >&2; exit 3",
            Duration::from_secs(5),
            false,
        )
        .await
        .unwrap();
        assert_eq!(r.status, CommandStatus::Exited(Some(3)));
        assert!(std::fs::read_to_string(&r.log).unwrap().contains("failure"));
        std::fs::remove_file(r.log).unwrap();
        let r = run_command(
            &c,
            "echo before; sleep 30",
            Duration::from_millis(100),
            false,
        )
        .await
        .unwrap();
        assert_eq!(r.status, CommandStatus::TimedOut);
        assert!(std::fs::read_to_string(&r.log).unwrap().contains("before"));
        std::fs::remove_file(r.log).unwrap();
    }
    #[tokio::test]
    async fn cancellation_keeps_log() {
        let c = ctx();
        let cancel = c.cancel.clone();
        let task = tokio::spawn(async move {
            run_command(&c, "echo before; sleep 30", Duration::from_secs(30), false)
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
        let r = task.await.unwrap();
        assert_eq!(r.status, CommandStatus::Cancelled);
        assert!(std::fs::read_to_string(&r.log).unwrap().contains("before"));
        std::fs::remove_file(r.log).unwrap();
    }
    #[tokio::test]
    async fn bash_default_is_quiet_and_failure_is_error() {
        let args = |cmd: &str| {
            serde_json::from_value(serde_json::json!({"command":cmd,"heavy":false})).unwrap()
        };
        let ok = BashTool.run(&ctx(), args("echo hidden")).await.unwrap();
        assert!(ok.starts_with("OK |"));
        assert!(!ok.contains("hidden"));
        let err = BashTool
            .run(&ctx(), args("echo hidden; exit 4"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exit=4"));
    }
}

#[cfg(test)]
mod heavy_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[tokio::test]
    async fn installed_heavy_supports_nested_runs_and_log_paths_with_spaces() {
        if heavy_binary().is_none() {
            return;
        }
        let root = std::env::temp_dir().join("makedo logs with spaces");
        let ctx = ToolCtx::new(std::env::temp_dir(), root);
        let result = run_command(
            &ctx,
            "heavy -- sh -c 'printf nested-ok'",
            Duration::from_secs(5),
            true,
        )
        .await
        .unwrap();
        assert!(result.success());
        assert!(result.heavy_used);
        assert_eq!(std::fs::read_to_string(&result.log).unwrap(), "nested-ok");
        assert_eq!(output_log(&result.summary()).as_ref(), Some(&result.log));
        std::fs::remove_file(result.log).unwrap();
    }
}

#[cfg(test)]
mod foreground_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[tokio::test]
    async fn file_backed_output_does_not_orphan_background_jobs() {
        let ctx = ToolCtx::new(
            std::env::temp_dir(),
            std::env::temp_dir().join("makedo-foreground-test"),
        );
        let result = run_command(&ctx, "sleep 30 &", Duration::from_secs(5), false)
            .await
            .unwrap();
        assert_eq!(result.status, CommandStatus::BackgroundStopped);
        assert!(!result.success());
        std::fs::remove_file(result.log).unwrap();
    }
}
