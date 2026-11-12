//! Append-only transcript and pending prompts. Completed tool calls are never replayed.
use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::Path;

use makedo_providers::{AssistantPart, Message};

#[derive(Debug, Default)]
pub struct Restored {
    pub messages: Vec<Message>,
    pub pending: Vec<(String, String)>,
    pub interrupted: Vec<String>,
}

pub fn append(path: &Path, value: &impl serde::Serialize) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(&bytes)
}

pub fn restore(path: &Path) -> std::io::Result<Restored> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Restored::default());
        }
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > 64 * 1024 * 1024 {
        return Err(std::io::Error::other(
            "session exceeds the 64 MiB restore limit",
        ));
    }
    let mut values = Vec::new();
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? > 0 {
        // Only an incomplete final write may be ignored. Corruption in a complete row is an error.
        if !line.ends_with(b"\n") {
            break;
        }
        let value: serde_json::Value = serde_json::from_slice(&line)?;
        line.clear();
        values.push(value);
    }
    restore_values(values)
}

pub fn restore_values(values: Vec<serde_json::Value>) -> std::io::Result<Restored> {
    let mut restored = Restored::default();
    let mut delivered = HashSet::new();
    let mut calls = HashSet::new();
    for value in values {
        if let Some(queued) = value.get("queued") {
            let id = queued["id"]
                .as_str()
                .ok_or_else(|| std::io::Error::other("invalid queued id"))?;
            let text = queued["text"]
                .as_str()
                .ok_or_else(|| std::io::Error::other("invalid queued text"))?;
            restored.pending.push((id.to_owned(), text.to_owned()));
            continue;
        }
        if let Some(id) = value.get("cancelled").and_then(serde_json::Value::as_str) {
            delivered.insert(id.to_owned());
            continue;
        }
        let message: Message =
            if let Some(id) = value.get("delivered").and_then(serde_json::Value::as_str) {
                delivered.insert(id.to_owned());
                serde_json::from_value(value["message"].clone())?
            } else {
                serde_json::from_value(value)?
            };
        match &message {
            Message::Assistant { content } => {
                for part in content {
                    if let AssistantPart::ToolCall { call_id, .. } = part {
                        calls.insert(call_id.clone());
                    }
                }
            }
            Message::ToolResult { call_id, .. } => {
                calls.remove(call_id);
            }
            _ => {}
        }
        restored.messages.push(message);
    }
    restored.pending.retain(|(id, _)| !delivered.contains(id));
    restored.interrupted = calls.into_iter().collect();
    restored.interrupted.sort();
    Ok(restored)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn queued_delivery_is_atomic_and_partial_final_row_is_ignored() {
        let path =
            std::env::temp_dir().join(format!("makedo-history-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        append(
            &path,
            &serde_json::json!({"queued":{"id":"a","text":"first"}}),
        )
        .unwrap();
        append(
            &path,
            &serde_json::json!({"queued":{"id":"b","text":"next"}}),
        )
        .unwrap();
        append(&path, &serde_json::json!({"delivered":"a","message":Message::User { content:"first".into() }})).unwrap();
        append(
            &path,
            &Message::Assistant {
                content: vec![AssistantPart::ToolCall {
                    call_id: "unfinished".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                }],
            },
        )
        .unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"role\":\"\xF0\x9F")
            .unwrap();
        let result = restore(&path).unwrap();
        assert_eq!(result.messages.len(), 2);
        assert_eq!(result.pending, vec![("b".into(), "next".into())]);
        assert_eq!(result.interrupted, vec!["unfinished"]);
        std::fs::remove_file(path).unwrap();
    }
}
