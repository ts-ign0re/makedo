//! Ресурсы машины: makedo ЗНАЕТ, на чём работает (принцип: маломощные девайсы),
//! и выводит из этого бюджеты. Детект алгоритмический, без зависимостей и сети.

use std::process::Command;

#[derive(Debug, Clone, Copy)]
pub struct Resources {
    pub cores: usize,
    /// Общая RAM в байтах; None = не удалось определить.
    pub ram_bytes: Option<u64>,
}

/// Бюджеты, выведенные из ресурсов. Единая точка правды для всех подсистем.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// Потолок параллельных тул-коллов.
    pub tool_concurrency: usize,
    /// Потолок захвата вывода команды в память.
    pub max_capture_bytes: usize,
    /// Потолок blocking-пула tokio.
    pub max_blocking_threads: usize,
}

impl Resources {
    #[must_use]
    pub fn detect() -> Self {
        Self {
            cores: std::thread::available_parallelism().map_or(2, std::num::NonZero::get),
            ram_bytes: detect_ram(),
        }
    }

    /// Правила: <8GB RAM = «тесная» машина — жмёмся сильнее.
    #[must_use]
    pub fn budget(&self) -> Budget {
        let tight = self.ram_bytes.is_some_and(|r| r < 8 * 1024 * 1024 * 1024);
        Budget {
            tool_concurrency: if tight {
                self.cores.min(2)
            } else {
                self.cores.min(4)
            },
            max_capture_bytes: if tight { 2_000_000 } else { 5_000_000 },
            max_blocking_threads: if tight { 16 } else { 32 },
        }
    }
}

fn detect_ram() -> Option<u64> {
    // Linux: /proc/meminfo; macOS: sysctl hw.memsize (командой — без unsafe FFI)
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        return text.lines().find_map(|l| {
            l.strip_prefix("MemTotal:")
                .and_then(|v| v.trim().trim_end_matches(" kB").parse::<u64>().ok())
                .map(|kb| kb * 1024)
        });
    }
    let out = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Свободно на диске (текущий том), байты. Только для `makedo doctor`: бюджеты
/// от диска не зависят, а `df` — лишний процесс на каждом старте.
#[must_use]
pub fn disk_free_bytes() -> Option<u64> {
    // df -k . — POSIX-переносимо; колонка Available в KiB
    let out = Command::new("df").args(["-k", "."]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1)?;
    let avail_kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_kb * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_something_sane() {
        let r = Resources::detect();
        assert!(r.cores >= 1);
        // на машинах CI/dev RAM и диск должны определяться
        assert!(r.ram_bytes.is_some_and(|b| b > 1024 * 1024 * 1024));
        assert!(disk_free_bytes().is_some_and(|b| b > 0));
        let b = r.budget();
        assert!(b.tool_concurrency >= 1 && b.tool_concurrency <= 4);
        assert!(b.max_capture_bytes >= 2_000_000);
    }

    #[test]
    fn tight_machine_gets_tighter_budget() {
        let tight = Resources {
            cores: 4,
            ram_bytes: Some(4 * 1024 * 1024 * 1024),
        };
        let b = tight.budget();
        assert_eq!(b.tool_concurrency, 2);
        assert_eq!(b.max_capture_bytes, 2_000_000);
        let roomy = Resources {
            ram_bytes: Some(16 * 1024 * 1024 * 1024),
            ..tight
        };
        assert_eq!(roomy.budget().tool_concurrency, 4);
    }
}
