//! Трункация вывода тулов: сверх лимита — спилл на диск + превью + «рулящая»
//! подсказка по классификации содержимого (grok-build: JSON → jq; opencode:
//! Grep/Read с offset). PLAN §3.1.

use std::path::{Path, PathBuf};

/// ≈10k токенов — сходится у обеих кодовых баз.
pub const MAX_BYTES: usize = 40_000;
pub const MAX_LINES: usize = 2_000;

#[derive(Debug)]
pub struct Truncated {
    /// Текст для модели (возможно, с превью и подсказкой).
    pub text: String,
    /// Путь спилла, если вывод был усечён.
    pub spill_path: Option<PathBuf>,
}

/// Классификация спилла для рулящей подсказки.
fn steering_hint(content: &str, path: &Path) -> String {
    let p = path.display();
    if serde_json::from_str::<serde_json::Value>(content).is_ok() {
        return format!(
            "Full output is valid JSON saved to {p}. Use `bash` with `jq` to query it."
        );
    }
    let long_lines = content.lines().any(|l| l.len() > 500);
    if long_lines {
        format!(
            "Full output saved to {p}. Lines are very long — grep is ineffective; use `bash` to slice it."
        )
    } else {
        format!("Full output saved to {p}. Use `grep` to search it or `read` with offset/limit.")
    }
}

/// Усекает вывод по байтам И строкам; сверх лимита пишет полный текст в
/// Уникальный приватный файл в `spill_dir` и возвращает превью с подсказкой.
/// Границы UTF-8 соблюдаются; превью — голова вывода (хвост живёт в спилле).
pub fn output(text: String, spill_dir: &Path, call_id: &str) -> Truncated {
    let over_bytes = text.len() > MAX_BYTES;
    let over_lines = text.lines().count() > MAX_LINES;
    if !over_bytes && !over_lines {
        return Truncated {
            text,
            spill_path: None,
        };
    }
    let artifact = crate::artifact::create(spill_dir, call_id, "txt")
        .and_then(|(path, mut file)| {
            use std::io::Write as _;
            file.write_all(text.as_bytes())?;
            Ok(path)
        })
        .ok();

    let mut cut = 0;
    for (i, line) in text.split_inclusive('\n').enumerate() {
        if i >= MAX_LINES || cut + line.len() > MAX_BYTES {
            break;
        }
        cut += line.len();
    }
    let total_lines = text.lines().count();
    let total_bytes = text.len();
    let mut preview = text[..cut].to_owned();
    {
        use std::fmt::Write as _;
        let _ = write!(
            preview,
            "\n[Output truncated: {total_lines} lines / {total_bytes} bytes total.]\n"
        );
    }
    if let Some(path) = &artifact {
        preview.push_str(&steering_hint(&text, path));
    }
    Truncated {
        text: preview,
        spill_path: artifact,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("makedo-trunc-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn small_output_untouched() {
        let t = output("hello".into(), &dir(), "c1");
        assert_eq!(t.text, "hello");
        assert!(t.spill_path.is_none());
    }

    #[test]
    fn big_output_spills_with_json_hint() {
        let big = format!("[{}]", "1,".repeat(30_000).trim_end_matches(','));
        let t = output(big.clone(), &dir(), "c2");
        assert!(t.text.contains("jq"));
        let spill = t.spill_path.unwrap();
        assert_eq!(std::fs::read_to_string(spill).unwrap(), big);
    }

    #[test]
    fn many_lines_spill_with_grep_hint() {
        let big = (0..3000).fold(String::new(), |mut s, i| {
            use std::fmt::Write;
            let _ = writeln!(s, "line {i}");
            s
        });
        let t = output(big, &dir(), "c3");
        assert!(t.text.contains("grep"));
        assert!(t.text.lines().count() < 2100);
    }
}
