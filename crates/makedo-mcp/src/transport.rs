//! Stdio-транспорт с потолком длины строки: rmcp 3.1 читает `read_until` без
//! лимита — враждебный/сломанный сервер с гигантской строкой без `\n` растит
//! буфер до OOM (та же дыра, что в grok-build). `CappedReader` рвёт соединение
//! чистой ошибкой (PLAN §3.2). Child живёт в своей process group и убивается
//! вместе с внуками на drop.

use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};

/// 10 MB на одну JSON-RPC строку — щедро для тул-результатов, фатально для OOM.
pub const MAX_LINE_BYTES: usize = 10 * 1024 * 1024;

/// AsyncRead-обёртка: считает байты с последнего `\n`; превышение = `io::Error`.
#[derive(Debug)]
pub struct CappedReader<R> {
    inner: R,
    run: usize,
    cap: usize,
    /// Ошибка отдаётся на СЛЕДУЮЩЕМ poll: контракт `AsyncRead` запрещает Err
    /// после того, как байты уже положены в буфер этого вызова.
    pending_err: bool,
}

impl<R> CappedReader<R> {
    pub fn new(inner: R, cap: usize) -> Self {
        Self {
            inner,
            run: 0,
            cap,
            pending_err: false,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for CappedReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.pending_err {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("MCP line exceeded {} bytes — connection dropped", self.cap),
            )));
        }
        let before = buf.filled().len();
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                for &b in &buf.filled()[before..] {
                    if b == b'\n' {
                        self.run = 0;
                    } else {
                        self.run += 1;
                        if self.run > self.cap {
                            self.pending_err = true;
                            break;
                        }
                    }
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

/// Child в своей process group; на drop — SIGTERM группе (системным вызовом через
/// rustix, без unsafe — как в bash-туле). stderr сервера — в /dev/null.
// ponytail: stderr в лог-файл сервера — когда появится логирование; grace-KILL не нужен, child без state
#[derive(Debug)]
pub struct ChildGuard {
    pub child: tokio::process::Child,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // не внешняя команда `kill`: на Linux она разбирает отрицательный pgid как флаг
        if let Some(group) = self
            .child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::TERM);
        }
        let _ = self.child.start_kill();
    }
}

type StdioPair = (
    CappedReader<tokio::process::ChildStdout>,
    tokio::process::ChildStdin,
);

/// Спавнит stdio-сервер; возвращает guard и пару (reader, writer) для rmcp.
pub fn spawn_stdio(
    command: &[String],
    env: &std::collections::BTreeMap<String, String>,
    cwd: Option<&std::path::Path>,
) -> std::io::Result<(ChildGuard, StdioPair)> {
    let Some((program, args)) = command.split_first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty MCP command",
        ));
    };
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let mut child = cmd.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("no stdout"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("no stdin"))?;
    Ok((
        ChildGuard { child },
        (CappedReader::new(stdout, MAX_LINE_BYTES), stdin),
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn newline_resets_counter_overflow_errors() {
        // строки под капом проходят
        let data = b"short line\nanother\n".to_vec();
        let mut r = CappedReader::new(std::io::Cursor::new(data), 16);
        let mut out = Vec::new();
        r.read_to_end(&mut out).await.unwrap();
        assert_eq!(out.len(), 19);

        // одна строка сверх капа — ошибка, не OOM
        let data = vec![b'x'; 64];
        let mut r = CappedReader::new(std::io::Cursor::new(data), 16);
        let mut out = Vec::new();
        let err = r.read_to_end(&mut out).await.unwrap_err();
        assert!(err.to_string().contains("exceeded"));
    }
}
